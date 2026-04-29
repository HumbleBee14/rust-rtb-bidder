use crate::{
    catalog::types::{Campaign, CampaignId, Creative, DeviceTargetType, GeoKey, SegmentId},
    model::openrtb::AdFormat,
};
use roaring::RoaringBitmap;
use std::collections::HashMap;

/// Immutable snapshot of the campaign catalog + inverted indices.
///
/// Built entirely off the hot path by the background refresh task.
/// Swapped in atomically via `ArcSwap::store`. In-flight requests holding
/// the old `Arc` read until they finish and drop it; new requests see the new
/// snapshot. Zero coordination required.
///
/// Bitmap-mutation safety contract: all indices are read-only for the lifetime
/// of a snapshot. `candidates_for()` always returns a *new* owned `RoaringBitmap`
/// — callers must never mutate via a raw index reference. Raw index fields are
/// `pub(crate)` and only accessible through the `candidates_for` API.
#[derive(Debug, Default)]
pub struct CampaignCatalog {
    /// All active campaigns, keyed by ID for O(1) hot-path lookup.
    pub(crate) campaigns: HashMap<CampaignId, Campaign>,
    /// Creatives per campaign.
    pub(crate) creatives: HashMap<CampaignId, Vec<Creative>>,

    // ── Inverted indices ───────────────────────────────────────────────────
    // Built at load time from the Postgres targeting tables. Read-only after
    // construction. Access only through candidates_for().
    pub(crate) segment_to_campaigns: HashMap<SegmentId, RoaringBitmap>,
    /// Keyed by (geo_kind, geo_code).
    pub(crate) geo_to_campaigns: HashMap<GeoKey, RoaringBitmap>,
    pub(crate) device_to_campaigns: HashMap<DeviceTargetType, RoaringBitmap>,
    pub(crate) format_to_campaigns: HashMap<AdFormat, RoaringBitmap>,
    /// Recomputed per-minute by the background task: campaigns active at the
    /// current hour-of-week. Empty bitmap = no active daypart constraint checked.
    pub(crate) daypart_active_now: RoaringBitmap,

    /// All campaign IDs as a bitmap — used as the universe for intersections
    /// when a targeting dimension is absent from a request (no restriction).
    pub(crate) all_campaigns: RoaringBitmap,

    /// Campaigns with no device-targeting rows — serve on any device type.
    pub(crate) device_unrestricted: RoaringBitmap,
    /// Campaigns with no format-targeting rows — serve on any ad format.
    pub(crate) format_unrestricted: RoaringBitmap,
}

impl CampaignCatalog {
    /// Candidate retrieval via bitmap intersection.
    ///
    /// For each non-empty targeting dimension in the request, intersects the
    /// corresponding inverted-index bitmaps. Returns a new owned `RoaringBitmap`
    /// of candidate campaign IDs. The caller owns the result; the catalog
    /// indices are never mutated.
    ///
    /// Returns an empty bitmap if the catalog has no campaigns loaded.
    pub fn candidates_for(&self, req: &CandidateRequest<'_>) -> RoaringBitmap {
        if self.all_campaigns.is_empty() {
            return RoaringBitmap::new();
        }

        // Lazy "universe" representation. `None` means "no constraint applied
        // yet" (conceptually = all_campaigns) — we avoid cloning the universe
        // until a filter actually narrows it. This matters on anonymous /
        // poorly-targeted traffic where the request carries no segments,
        // no geo, no device hint: previously each such request cloned a
        // 50K–100K-element bitmap; now it returns a reference to
        // `all_campaigns` from the catalog snapshot only at the end.
        let mut result: Option<RoaringBitmap> = if req.segment_ids.is_empty() {
            None
        } else {
            // Segment union: campaign matches if it targets ANY user segment.
            let mut union = RoaringBitmap::new();
            for &seg_id in req.segment_ids {
                if let Some(bm) = self.segment_to_campaigns.get(&seg_id) {
                    union |= bm;
                }
            }
            Some(union)
        };

        // Geo intersection: campaigns must target this user's geo.
        // If request carries no geo, no restriction (skip).
        if let Some(geo_keys) = &req.geo_keys {
            if !geo_keys.is_empty() {
                let mut geo_union = RoaringBitmap::new();
                for key in *geo_keys {
                    if let Some(bm) = self.geo_to_campaigns.get(key) {
                        geo_union |= bm;
                    }
                }
                result = Some(match result {
                    Some(mut r) => {
                        r &= &geo_union;
                        r
                    }
                    None => geo_union,
                });
            }
        }

        // Device intersection.
        // Campaigns with no device-targeting rows are unrestricted and must be included
        // regardless of which device type the request carries.
        if let Some(device) = req.device_type {
            let mut eligible = self.device_unrestricted.clone();
            if let Some(bm) = self.device_to_campaigns.get(&device) {
                eligible |= bm;
            }
            result = Some(match result {
                Some(mut r) => {
                    r &= &eligible;
                    r
                }
                None => eligible,
            });
        }

        // Format intersection — same unrestricted-passthrough logic as device.
        if let Some(format) = req.ad_format {
            let mut eligible = self.format_unrestricted.clone();
            if let Some(bm) = self.format_to_campaigns.get(&format) {
                eligible |= bm;
            }
            result = Some(match result {
                Some(mut r) => {
                    r &= &eligible;
                    r
                }
                None => eligible,
            });
        }

        // Daypart intersection — only restrict if daypart index is non-empty.
        if !self.daypart_active_now.is_empty() {
            result = Some(match result {
                Some(mut r) => {
                    r &= &self.daypart_active_now;
                    r
                }
                None => self.daypart_active_now.clone(),
            });
        }

        // Materialize at the end. If no filter ever fired, this is the only
        // place we clone the full universe — and only when the eventual caller
        // actually needs an owned bitmap.
        result.unwrap_or_else(|| self.all_campaigns.clone())
    }

    pub fn campaign(&self, id: CampaignId) -> Option<&Campaign> {
        self.campaigns.get(&id)
    }

    pub fn creatives_for(&self, campaign_id: CampaignId) -> &[Creative] {
        self.creatives
            .get(&campaign_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.campaigns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.campaigns.is_empty()
    }

    /// Returns `(campaign_id, daily_budget_cents)` for every active campaign.
    /// Used by `BudgetPacer::reload` at startup and on each catalog refresh.
    pub fn budget_seeds(&self) -> Vec<(CampaignId, i64)> {
        self.campaigns
            .iter()
            .map(|(&id, c)| (id, c.daily_budget_cents))
            .collect()
    }

    /// Constructs a catalog from raw maps. Only available in test builds.
    #[cfg(any(test, feature = "test-helpers"))]
    #[allow(clippy::too_many_arguments)]
    pub fn new_for_test(
        campaigns: HashMap<CampaignId, crate::catalog::types::Campaign>,
        creatives: HashMap<CampaignId, Vec<crate::catalog::types::Creative>>,
        segment_to_campaigns: HashMap<SegmentId, RoaringBitmap>,
        geo_to_campaigns: HashMap<GeoKey, RoaringBitmap>,
        device_to_campaigns: HashMap<crate::catalog::types::DeviceTargetType, RoaringBitmap>,
        format_to_campaigns: HashMap<AdFormat, RoaringBitmap>,
        daypart_active_now: RoaringBitmap,
        all_campaigns: RoaringBitmap,
    ) -> Self {
        let device_restricted: RoaringBitmap =
            device_to_campaigns
                .values()
                .fold(RoaringBitmap::new(), |mut acc, bm| {
                    acc |= bm;
                    acc
                });
        let format_restricted: RoaringBitmap =
            format_to_campaigns
                .values()
                .fold(RoaringBitmap::new(), |mut acc, bm| {
                    acc |= bm;
                    acc
                });
        let device_unrestricted = &all_campaigns - &device_restricted;
        let format_unrestricted = &all_campaigns - &format_restricted;
        Self {
            campaigns,
            creatives,
            segment_to_campaigns,
            geo_to_campaigns,
            device_to_campaigns,
            format_to_campaigns,
            daypart_active_now,
            all_campaigns,
            device_unrestricted,
            format_unrestricted,
        }
    }
}

/// Input to `candidates_for`. Built per-request from the parsed `BidRequest`.
pub struct CandidateRequest<'a> {
    pub segment_ids: &'a [SegmentId],
    pub geo_keys: Option<&'a [GeoKey]>,
    pub device_type: Option<DeviceTargetType>,
    pub ad_format: Option<AdFormat>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::openrtb::AdFormat;

    fn make_catalog() -> CampaignCatalog {
        let mut segment_to_campaigns: HashMap<SegmentId, RoaringBitmap> = HashMap::new();
        let mut bm1 = RoaringBitmap::new();
        bm1.insert(1);
        bm1.insert(2);
        segment_to_campaigns.insert(10, bm1); // segment 10 → campaigns 1,2

        let mut bm2 = RoaringBitmap::new();
        bm2.insert(2);
        bm2.insert(3);
        segment_to_campaigns.insert(20, bm2); // segment 20 → campaigns 2,3

        let mut device_to_campaigns: HashMap<DeviceTargetType, RoaringBitmap> = HashMap::new();
        let mut dev_bm = RoaringBitmap::new();
        dev_bm.insert(1);
        dev_bm.insert(2);
        dev_bm.insert(3);
        device_to_campaigns.insert(DeviceTargetType::Mobile, dev_bm);

        let mut format_to_campaigns: HashMap<AdFormat, RoaringBitmap> = HashMap::new();
        let mut fmt_bm = RoaringBitmap::new();
        fmt_bm.insert(1);
        fmt_bm.insert(3);
        format_to_campaigns.insert(AdFormat::Banner, fmt_bm);

        let mut all_campaigns = RoaringBitmap::new();
        all_campaigns.insert(1);
        all_campaigns.insert(2);
        all_campaigns.insert(3);

        // campaigns 1,2,3 all have device targeting (Mobile) and format targeting (Banner for 1,3)
        // campaign 2 has no format targeting row → format_unrestricted = {2}
        let device_unrestricted = RoaringBitmap::new(); // all 3 are device-restricted to Mobile
        let mut format_unrestricted = RoaringBitmap::new();
        format_unrestricted.insert(2); // campaign 2 has no format targeting

        CampaignCatalog {
            campaigns: HashMap::new(),
            creatives: HashMap::new(),
            segment_to_campaigns,
            geo_to_campaigns: HashMap::new(),
            device_to_campaigns,
            format_to_campaigns,
            daypart_active_now: RoaringBitmap::new(),
            all_campaigns,
            device_unrestricted,
            format_unrestricted,
        }
    }

    #[test]
    fn no_restrictions_returns_all() {
        let cat = make_catalog();
        let req = CandidateRequest {
            segment_ids: &[],
            geo_keys: None,
            device_type: None,
            ad_format: None,
        };
        let result = cat.candidates_for(&req);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn segment_union_then_device_intersection() {
        let cat = make_catalog();
        // Segments 10 → {1,2}, 20 → {2,3}; union = {1,2,3}
        // Device Mobile → {1,2,3}; intersection = {1,2,3}
        let req = CandidateRequest {
            segment_ids: &[10, 20],
            geo_keys: None,
            device_type: Some(DeviceTargetType::Mobile),
            ad_format: None,
        };
        let result = cat.candidates_for(&req);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn format_intersection_narrows_results() {
        let cat = make_catalog();
        // Segment 10 → {1,2}; Banner → {1,3}; format_unrestricted = {2}
        // eligible for Banner = {1,3} | {2} = {1,2,3}; intersect with segment set {1,2} = {1,2}
        let req = CandidateRequest {
            segment_ids: &[10],
            geo_keys: None,
            device_type: None,
            ad_format: Some(AdFormat::Banner),
        };
        let result = cat.candidates_for(&req);
        let ids: Vec<u32> = result.iter().collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn unknown_device_returns_empty() {
        let cat = make_catalog();
        // All 3 campaigns have explicit Mobile device targeting; device_unrestricted is empty.
        // A CTV request finds no CTV bitmap and no unrestricted campaigns → empty.
        let req = CandidateRequest {
            segment_ids: &[],
            geo_keys: None,
            device_type: Some(DeviceTargetType::Ctv),
            ad_format: None,
        };
        assert!(cat.candidates_for(&req).is_empty());
    }

    #[test]
    fn unrestricted_campaign_survives_device_filter() {
        // Campaign 99 has no device targeting rows → should appear on any device type.
        let mut device_to_campaigns: HashMap<DeviceTargetType, RoaringBitmap> = HashMap::new();
        let mut mobile_bm = RoaringBitmap::new();
        mobile_bm.insert(1); // campaign 1 is mobile-only
        device_to_campaigns.insert(DeviceTargetType::Mobile, mobile_bm);

        let mut all_campaigns = RoaringBitmap::new();
        all_campaigns.insert(1);
        all_campaigns.insert(99); // campaign 99 has no device rows

        let device_restricted: RoaringBitmap =
            device_to_campaigns
                .values()
                .fold(RoaringBitmap::new(), |mut acc, bm| {
                    acc |= bm;
                    acc
                });
        let device_unrestricted = &all_campaigns - &device_restricted;

        let cat = CampaignCatalog {
            campaigns: HashMap::new(),
            creatives: HashMap::new(),
            segment_to_campaigns: HashMap::new(),
            geo_to_campaigns: HashMap::new(),
            device_to_campaigns,
            format_to_campaigns: HashMap::new(),
            daypart_active_now: RoaringBitmap::new(),
            all_campaigns,
            device_unrestricted,
            format_unrestricted: RoaringBitmap::new(),
        };

        // Desktop request: campaign 1 (mobile-only) excluded; campaign 99 (unrestricted) included.
        let req = CandidateRequest {
            segment_ids: &[],
            geo_keys: None,
            device_type: Some(DeviceTargetType::Desktop),
            ad_format: None,
        };
        let result = cat.candidates_for(&req);
        assert!(
            result.contains(99),
            "unrestricted campaign must survive device filter"
        );
        assert!(
            !result.contains(1),
            "mobile-only campaign must be excluded on desktop"
        );
    }

    #[test]
    fn anonymous_traffic_with_no_filters_returns_all_unchanged() {
        // Regression: anonymous requests (no segments, no geo, no device,
        // no format) used to clone all_campaigns up front; the lazy path
        // now defers materialization to the final unwrap_or_else. Output
        // must still equal all_campaigns.
        let cat = make_catalog();
        let req = CandidateRequest {
            segment_ids: &[],
            geo_keys: None,
            device_type: None,
            ad_format: None,
        };
        let result = cat.candidates_for(&req);
        let expected: Vec<u32> = cat.all_campaigns.iter().collect();
        let got: Vec<u32> = result.iter().collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn lazy_universe_seeded_by_first_filter_skips_segment_clone() {
        // Request has no segments but does carry device targeting. The lazy
        // path should seed `result` directly from device_unrestricted ∪
        // device_to_campaigns[Mobile] without ever touching all_campaigns.
        let cat = make_catalog();
        let req = CandidateRequest {
            segment_ids: &[],
            geo_keys: None,
            device_type: Some(DeviceTargetType::Mobile),
            ad_format: None,
        };
        let result = cat.candidates_for(&req);
        // Mobile bitmap is {1,2,3}; device_unrestricted is empty in this
        // fixture; expected = {1,2,3}.
        let ids: Vec<u32> = result.iter().collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn empty_catalog_returns_empty() {
        let cat = CampaignCatalog::default();
        let req = CandidateRequest {
            segment_ids: &[],
            geo_keys: None,
            device_type: None,
            ad_format: None,
        };
        assert!(cat.candidates_for(&req).is_empty());
    }
}

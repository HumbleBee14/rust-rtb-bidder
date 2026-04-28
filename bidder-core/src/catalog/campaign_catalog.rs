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
}

impl CampaignCatalog {
    /// Candidate retrieval via bitmap intersection.
    ///
    /// For each non-empty targeting dimension in the request, intersects the
    /// corresponding inverted-index bitmaps. Returns a new owned `RoaringBitmap`
    /// of candidate campaign IDs. The caller owns the result; the catalog
    /// indices are never mutated.
    ///
    /// Returns `None` if the catalog is empty (no campaigns loaded).
    pub fn candidates_for(&self, req: &CandidateRequest<'_>) -> RoaringBitmap {
        if self.all_campaigns.is_empty() {
            return RoaringBitmap::new();
        }

        // Segment union: OR all per-segment bitmaps into a working set.
        // A campaign matches if it targets ANY of the user's segments.
        let mut result = if req.segment_ids.is_empty() {
            // No user segments → no segment targeting restriction; start with all.
            self.all_campaigns.clone()
        } else {
            let mut union = RoaringBitmap::new();
            for &seg_id in req.segment_ids {
                if let Some(bm) = self.segment_to_campaigns.get(&seg_id) {
                    union |= bm;
                }
            }
            union
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
                result &= &geo_union;
            }
        }

        // Device intersection.
        if let Some(device) = req.device_type {
            if let Some(bm) = self.device_to_campaigns.get(&device) {
                result &= bm;
            } else {
                return RoaringBitmap::new();
            }
        }

        // Format intersection.
        if let Some(format) = req.ad_format {
            if let Some(bm) = self.format_to_campaigns.get(&format) {
                result &= bm;
            } else {
                return RoaringBitmap::new();
            }
        }

        // Daypart intersection — only restrict if daypart index is non-empty.
        if !self.daypart_active_now.is_empty() {
            result &= &self.daypart_active_now;
        }

        result
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
}

/// Input to `candidates_for`. Built per-request from the parsed `BidRequest`.
pub struct CandidateRequest<'a> {
    pub segment_ids: &'a [SegmentId],
    pub geo_keys: Option<&'a [GeoKey]>,
    pub device_type: Option<DeviceTargetType>,
    pub ad_format: Option<AdFormat>,
}

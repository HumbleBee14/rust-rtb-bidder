use crate::{
    catalog::types::DeviceTargetType,
    catalog::{CandidateRequest, GeoKey, GeoKind},
    model::{candidate::AdCandidate, context::BidContext, openrtb::AdFormat},
    pipeline::stage::Stage,
};
use tracing::instrument;

/// Runs bitmap intersection for each impression and populates `ctx.candidates`.
///
/// For each imp, derives targeting dimensions from the request (geo, device,
/// ad format) and calls `catalog.candidates_for()`. The resulting campaign IDs
/// are expanded into `AdCandidate` entries using the catalog's campaign map.
///
/// Candidates have bid_price_cents set to campaign.bid_floor_cents (adjusted
/// upward by BudgetPacingStage if needed). Score starts at 0.0 until ScoringStage.
pub struct CandidateRetrievalStage;

impl Stage for CandidateRetrievalStage {
    fn name(&self) -> &'static str {
        "candidate_retrieval"
    }

    #[instrument(name = "stage.candidate_retrieval", skip(self, ctx), fields(request_id = %ctx.request.id))]
    async fn execute<'a>(&'a self, ctx: &'a mut BidContext) -> anyhow::Result<()> {
        let catalog = match ctx.catalog.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(()), // no catalog — no candidates
        };

        if catalog.is_empty() {
            return Ok(());
        }

        // Build geo keys from device.geo (country) and device.geo.metro.
        let geo_keys: Option<Vec<GeoKey>> = ctx
            .request
            .device
            .as_ref()
            .and_then(|d| d.geo.as_ref())
            .map(|geo| {
                let mut keys = Vec::new();
                if let Some(country) = geo.country.as_deref() {
                    keys.push(GeoKey {
                        kind: GeoKind::Country,
                        code: country.to_string(),
                    });
                }
                if let Some(metro) = geo.metro.as_deref() {
                    keys.push(GeoKey {
                        kind: GeoKind::Metro,
                        code: metro.to_string(),
                    });
                }
                keys
            });

        // Map OpenRTB device type to catalog targeting type.
        let device_type: Option<DeviceTargetType> = ctx
            .request
            .device
            .as_ref()
            .and_then(|d| d.devicetype)
            .map(|dt| DeviceTargetType::from_openrtb(dt.0));

        // Ensure candidates Vec is sized to the number of impressions.
        ctx.candidates
            .resize(ctx.request.imp.len().max(1), Vec::new());

        for (imp_idx, imp) in ctx.request.imp.iter().enumerate() {
            let ad_format = imp_ad_format(imp);
            let candidate_req = CandidateRequest {
                segment_ids: &ctx.segment_ids,
                geo_keys: geo_keys.as_deref(),
                device_type,
                ad_format,
            };

            let bitmap = catalog.candidates_for(&candidate_req);

            let candidates: Vec<AdCandidate> = bitmap
                .iter()
                .filter_map(|cid| {
                    let campaign = catalog.campaign(cid)?;
                    // Pick first creative matching the imp format. If none match,
                    // skip — the campaign can't serve this impression.
                    let creative = catalog
                        .creatives_for(cid)
                        .iter()
                        .find(|cr| ad_format.is_none_or(|fmt| cr.ad_format == fmt))?;
                    Some(AdCandidate {
                        campaign_id: cid,
                        creative_id: creative.id,
                        bid_price_cents: campaign.bid_floor_cents,
                        score: 0.0,
                    })
                })
                .collect();

            metrics::histogram!("bidder.candidate_retrieval.count", "imp_idx" => imp_idx.to_string())
                .record(candidates.len() as f64);

            ctx.candidates[imp_idx] = candidates;
        }

        Ok(())
    }
}

/// Extract the dominant ad format from an impression.
/// If an imp has banner, returns Banner; video → Video; audio → Audio; else Native.
/// Returns None if the imp has no recognized format (unlikely in practice).
fn imp_ad_format(imp: &crate::model::openrtb::Imp) -> Option<AdFormat> {
    if imp.banner.is_some() {
        Some(AdFormat::Banner)
    } else if imp.video.is_some() {
        Some(AdFormat::Video)
    } else if imp.audio.is_some() {
        Some(AdFormat::Audio)
    } else if imp.native.is_some() {
        Some(AdFormat::Native)
    } else {
        None
    }
}

use crate::{
    catalog::types::DeviceTargetType,
    clock::current_hour_of_day,
    model::context::BidContext,
    pipeline::stage::Stage,
    pipeline::stages::candidate_retrieval::imp_ad_format,
    scoring::{Scorer, ScoringContext},
};
use std::sync::Arc;
use tracing::instrument;

pub struct ScoringStage {
    pub scorer: Arc<dyn Scorer>,
}

impl Stage for ScoringStage {
    fn name(&self) -> &'static str {
        "scoring"
    }

    #[instrument(name = "stage.scoring", skip(self, ctx), fields(request_id = %ctx.request.id))]
    async fn execute<'a>(&'a self, ctx: &'a mut BidContext) -> anyhow::Result<()> {
        // Per-request features pulled out of the OpenRTB tree once; the same
        // ScoringContext is shared across all impressions and all candidates.
        let device_type = ctx
            .request
            .device
            .as_ref()
            .and_then(|d| d.devicetype)
            .map(|dt| DeviceTargetType::from_openrtb(dt.0));
        let user_id = ctx
            .request
            .user
            .as_ref()
            .and_then(|u| u.id.as_deref())
            .unwrap_or("");
        let hour_of_day = current_hour_of_day();
        // Geo top-market detection is request-level; CONTRACT § 2 placeholder
        // until DS team confirms the metro list. Defaulted to false.
        let is_top_market = false;

        let segment_ids = ctx.segment_ids.clone();

        for (imp_idx, candidates) in ctx.candidates.iter_mut().enumerate() {
            if candidates.is_empty() {
                continue;
            }
            let ad_format = ctx.request.imp.get(imp_idx).and_then(imp_ad_format);
            let scoring_ctx = ScoringContext {
                segment_ids: &segment_ids,
                device_type,
                ad_format,
                hour_of_day,
                user_id,
                is_top_market,
            };
            self.scorer.score_all(candidates, &scoring_ctx).await;
        }
        Ok(())
    }
}

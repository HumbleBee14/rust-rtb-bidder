use crate::{model::context::BidContext, pipeline::stage::Stage, scoring::Scorer};
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
        for candidates in ctx.candidates.iter_mut() {
            if candidates.is_empty() {
                continue;
            }
            self.scorer.score_all(candidates, &ctx.segment_ids).await;
        }
        Ok(())
    }
}

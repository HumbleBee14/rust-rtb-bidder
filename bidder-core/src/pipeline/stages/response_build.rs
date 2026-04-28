use crate::{
    model::{BidContext, NoBidReason, PipelineOutcome},
    pipeline::Stage,
};
use tracing::instrument;

/// Final stage: resolves the pipeline outcome into a decision.
///
/// Phase 2: if still Pending (no stage set a no-bid), emit no-bid with
/// NO_ELIGIBLE_BIDS. Phase 4 replaces this with actual bid construction when
/// the candidate + scoring stages populate winners.
pub struct ResponseBuildStage;

impl Stage for ResponseBuildStage {
    fn name(&self) -> &'static str {
        "response_build"
    }

    #[instrument(name = "stage.response_build", skip(self, ctx), fields(request_id = %ctx.request.id))]
    async fn execute<'a>(&'a self, ctx: &'a mut BidContext) -> anyhow::Result<()> {
        if ctx.outcome == PipelineOutcome::Pending {
            ctx.outcome = PipelineOutcome::NoBid(NoBidReason::NO_ELIGIBLE_BIDS);
        }
        Ok(())
    }
}

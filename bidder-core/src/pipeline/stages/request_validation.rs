use crate::{
    model::{BidContext, NoBidReason, PipelineOutcome},
    pipeline::Stage,
};
use tracing::instrument;

pub struct RequestValidationStage;

impl Stage for RequestValidationStage {
    fn name(&self) -> &'static str {
        "request_validation"
    }

    #[instrument(name = "stage.request_validation", skip(self, ctx), fields(request_id = %ctx.request.id))]
    async fn execute<'a>(&'a self, ctx: &'a mut BidContext) -> anyhow::Result<()> {
        let req = &ctx.request;

        if req.id.is_empty() {
            ctx.outcome = PipelineOutcome::NoBid(NoBidReason::INVALID_REQUEST);
            metrics::counter!("bidder.validation.rejected", "reason" => "missing_id").increment(1);
            return Ok(());
        }

        if req.imp.is_empty() {
            ctx.outcome = PipelineOutcome::NoBid(NoBidReason::INVALID_REQUEST);
            metrics::counter!("bidder.validation.rejected", "reason" => "no_impressions")
                .increment(1);
            return Ok(());
        }

        // GDPR: if gdpr=1 and no consent string, no-bid.
        if let Some(regs) = &req.regs {
            if regs.gdpr == Some(1) {
                let has_consent = req
                    .user
                    .as_ref()
                    .and_then(|u| u.ext.as_ref())
                    .and_then(|e| e.get("consent"))
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);

                if !has_consent {
                    ctx.outcome = PipelineOutcome::NoBid(NoBidReason::REQUEST_BLOCKED_PRIVACY);
                    metrics::counter!(
                        "bidder.validation.rejected",
                        "reason" => "gdpr_no_consent"
                    )
                    .increment(1);
                    return Ok(());
                }
            }
        }

        metrics::counter!("bidder.validation.passed").increment(1);
        Ok(())
    }
}

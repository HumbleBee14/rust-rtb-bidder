use crate::{
    model::{context::BidContext, NoBidReason, PipelineOutcome},
    pacing::{BudgetPacer, PaceDecision},
    pipeline::stage::Stage,
};
use std::sync::Arc;

pub struct BudgetPacingStage {
    pub pacer: Arc<dyn BudgetPacer>,
}

impl Stage for BudgetPacingStage {
    fn name(&self) -> &'static str {
        "budget_pacing"
    }

    #[allow(clippy::manual_async_fn)]
    fn execute<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a {
        async move {
            for candidates in ctx.candidates.iter_mut() {
                if candidates.is_empty() {
                    continue;
                }

                let mut exhausted_ids: Vec<u32> = Vec::new();

                for candidate in candidates.iter() {
                    let decision = self
                        .pacer
                        .check_and_reserve(candidate.campaign_id, candidate.bid_price_cents)
                        .await;
                    if decision == PaceDecision::Exhausted {
                        exhausted_ids.push(candidate.campaign_id);
                    }
                }

                if !exhausted_ids.is_empty() {
                    let exhausted: std::collections::HashSet<u32> =
                        exhausted_ids.into_iter().collect();
                    // Release the ones we already reserved for campaigns we're keeping.
                    // (check_and_reserve only reserved those that returned HasBudget)
                    candidates.retain(|c| !exhausted.contains(&c.campaign_id));
                    metrics::counter!("bidder.budget.exhausted_filtered")
                        .increment(exhausted.len() as u64);
                }
            }

            let any_remaining = ctx.candidates.iter().any(|v| !v.is_empty());
            if !any_remaining {
                ctx.outcome = PipelineOutcome::NoBid(NoBidReason::NO_ELIGIBLE_BIDS);
            }

            Ok(())
        }
    }
}

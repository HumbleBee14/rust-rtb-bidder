pub mod stage;
pub mod stages;

use crate::{
    config::LatencyBudgetConfig,
    model::{BidContext, NoBidReason, PipelineOutcome},
};
use std::{sync::Arc, time::Instant};
use tracing::instrument;

pub use stage::Stage;

/// Orchestrates the ordered sequence of pipeline stages.
///
/// Each stage is executed in order. Per-stage latency is recorded and checked
/// against the declared budget — a warning metric fires when the budget is
/// exceeded, but the stage result is still used (enforcement = metrics + alerts,
/// not hard abort at the stage level). Hard abort is handled by the
/// `pipeline_deadline_ms` check before each stage: if the cumulative elapsed
/// time has crossed the pipeline deadline the stage is skipped and a NoBid is
/// set immediately.
pub struct Pipeline {
    stages: Vec<Arc<dyn ErasedStage>>,
    budget: LatencyBudgetConfig,
}

impl Pipeline {
    pub fn new(budget: LatencyBudgetConfig) -> Self {
        Self {
            stages: Vec::new(),
            budget,
        }
    }

    pub fn add_stage<S: Stage>(mut self, stage: S) -> Self {
        self.stages.push(Arc::new(ErasedStageWrapper(stage)));
        self
    }

    #[instrument(skip(self, ctx), fields(request_id = %ctx.request.id))]
    pub async fn execute(&self, ctx: &mut BidContext) -> anyhow::Result<()> {
        let deadline_ms = self.budget.pipeline_deadline_ms;

        for stage_ref in &self.stages {
            // Check pipeline deadline before starting each stage.
            if ctx.elapsed_ms() >= deadline_ms {
                metrics::counter!("bidder.pipeline.early_drop").increment(1);
                tracing::warn!(
                    elapsed_ms = ctx.elapsed_ms(),
                    deadline_ms,
                    "pipeline deadline exceeded — returning no-bid"
                );
                ctx.outcome = PipelineOutcome::NoBid(NoBidReason::PIPELINE_DEADLINE);
                return Ok(());
            }

            let name = stage_ref.stage_name();
            let stage_start = Instant::now();

            let span = tracing::debug_span!("pipeline.stage", stage = name);
            let result = {
                let _enter = span.enter();
                stage_ref.execute_erased(ctx).await
            };

            let stage_elapsed = stage_start.elapsed();
            let stage_elapsed_ms = stage_elapsed.as_secs_f64() * 1000.0;

            metrics::histogram!(
                "bidder.pipeline.stage.duration_seconds",
                "stage" => name
            )
            .record(stage_elapsed.as_secs_f64());

            // Check per-stage budget.
            let budget_ms = self.budget.budget_for_stage(name);
            if let Some(budget_ms) = budget_ms {
                if stage_elapsed_ms > budget_ms as f64 {
                    metrics::counter!(
                        "bidder.pipeline.stage.budget_exceeded",
                        "stage" => name
                    )
                    .increment(1);
                    tracing::warn!(
                        stage = name,
                        elapsed_ms = stage_elapsed_ms,
                        budget_ms,
                        "stage exceeded latency budget"
                    );
                }
            }

            result?;

            // Short-circuit if a stage set a no-bid outcome.
            if ctx.outcome != PipelineOutcome::Pending {
                break;
            }
        }

        Ok(())
    }
}

// ── Type erasure ─────────────────────────────────────────────────────────────
// Stage uses RPITIT, which is not object-safe. We erase the concrete type here
// so the Pipeline can hold a heterogeneous Vec without boxing every future in
// the trait itself.

trait ErasedStage: Send + Sync + 'static {
    fn stage_name(&self) -> &'static str;
    fn execute_erased<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

struct ErasedStageWrapper<S>(S);

impl<S: Stage> ErasedStage for ErasedStageWrapper<S> {
    fn stage_name(&self) -> &'static str {
        self.0.name()
    }

    fn execute_erased<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(self.0.execute(ctx))
    }
}

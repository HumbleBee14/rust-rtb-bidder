use crate::{
    clock::current_hour_of_day,
    frequency::{FreqCapOutcome, FrequencyCapper},
    model::context::BidContext,
    pipeline::stage::Stage,
};
use std::sync::Arc;

pub struct FreqCapStage {
    pub capper: Arc<dyn FrequencyCapper>,
}

impl Stage for FreqCapStage {
    fn name(&self) -> &'static str {
        "frequency_cap"
    }

    #[allow(clippy::manual_async_fn)]
    fn execute<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a {
        async move {
            let user_id = match ctx.request.user.as_ref().and_then(|u| u.id.as_deref()) {
                Some(id) => id.to_string(),
                None => return Ok(()), // FreqCapOutcome::SkippedNoUser path
            };

            let device_type_val = ctx
                .request
                .device
                .as_ref()
                .and_then(|d| d.devicetype)
                .map(|dt| dt.0)
                .unwrap_or(0);

            let hour_of_day = current_hour_of_day();

            for (imp_idx, candidates) in ctx.candidates.iter_mut().enumerate() {
                if candidates.is_empty() {
                    continue;
                }

                let outcome = self
                    .capper
                    .check(&user_id, candidates, device_type_val, hour_of_day)
                    .await;

                match outcome {
                    FreqCapOutcome::SkippedTimeout | FreqCapOutcome::SkippedNoUser => {
                        // Proceed without filtering — SLA > bid quality.
                    }
                    FreqCapOutcome::Checked(results) => {
                        // Record results in context for observability.
                        for r in &results {
                            ctx.freq_cap_results
                                .push((imp_idx, r.campaign_id, r.capped));
                        }
                        // Filter capped candidates in-place.
                        let capped: std::collections::HashSet<u32> = results
                            .iter()
                            .filter(|r| r.capped)
                            .map(|r| r.campaign_id)
                            .collect();
                        if !capped.is_empty() {
                            candidates.retain(|c| !capped.contains(&c.campaign_id));
                            metrics::counter!("bidder.freq_cap.filtered")
                                .increment(capped.len() as u64);
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

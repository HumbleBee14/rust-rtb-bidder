use crate::{
    model::{candidate::ImpWinner, context::BidContext, NoBidReason, PipelineOutcome},
    pacing::BudgetPacer,
    pipeline::stage::Stage,
};
use std::sync::Arc;

/// Selects the highest-scoring candidate per impression.
///
/// Picks the highest-scoring candidate per impression (tie-broken by price),
/// then releases the reserved budget for every non-winning candidate so the
/// pacer counters reflect actual spend, not speculative reservations.
pub struct RankingStage {
    pub pacer: Arc<dyn BudgetPacer>,
}

impl Stage for RankingStage {
    fn name(&self) -> &'static str {
        "ranking"
    }

    #[allow(clippy::manual_async_fn)]
    fn execute<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a {
        async move {
            for (idx, candidates) in ctx.candidates.iter().enumerate() {
                let imp_id = ctx
                    .request
                    .imp
                    .get(idx)
                    .map(|i| i.id.clone())
                    .unwrap_or_else(|| idx.to_string());

                let winner = candidates.iter().max_by(|a, b| {
                    a.score
                        .partial_cmp(&b.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.bid_price_cents.cmp(&b.bid_price_cents))
                });

                if let Some(w) = winner {
                    // Release budget for every non-winning candidate.
                    for c in candidates.iter().filter(|c| c.campaign_id != w.campaign_id) {
                        self.pacer.release(c.campaign_id, c.bid_price_cents).await;
                    }
                    ctx.winners.push(ImpWinner {
                        imp_id,
                        campaign_id: w.campaign_id,
                        creative_id: w.creative_id,
                        bid_price_cents: w.bid_price_cents,
                        score: w.score,
                    });
                }
            }

            if ctx.winners.is_empty() {
                ctx.outcome = PipelineOutcome::NoBid(NoBidReason::NO_ELIGIBLE_BIDS);
            } else {
                ctx.outcome = PipelineOutcome::Bid;
                metrics::counter!("bidder.ranking.winners").increment(ctx.winners.len() as u64);
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{candidate::AdCandidate, openrtb::BidRequest},
        pacing::LocalBudgetPacer,
        pipeline::stage::Stage,
    };

    async fn stage() -> RankingStage {
        let pacer = Arc::new(LocalBudgetPacer::new());
        // Seed generous budgets so no candidate is blocked during ranking tests.
        pacer
            .reload(vec![
                (1, 100_000),
                (2, 100_000),
                (3, 100_000),
                (4, 100_000),
                (10, 100_000),
                (20, 100_000),
            ])
            .await;
        RankingStage { pacer }
    }

    fn make_ctx(candidates_per_imp: Vec<Vec<AdCandidate>>) -> BidContext {
        let req: BidRequest =
            serde_json::from_str(r#"{"id":"test","imp":[{"id":"imp1"},{"id":"imp2"}]}"#).unwrap();
        let mut ctx = BidContext::new(req);
        ctx.candidates = candidates_per_imp;
        ctx
    }

    fn candidate(campaign_id: u32, score: f32) -> AdCandidate {
        AdCandidate {
            campaign_id,
            creative_id: 1,
            bid_price_cents: 100,
            score,
            daily_cap_imps: u32::MAX,
            hourly_cap_imps: u32::MAX,
        }
    }

    #[tokio::test]
    async fn picks_highest_score() {
        let mut ctx = make_ctx(vec![vec![
            candidate(1, 0.3),
            candidate(2, 0.8),
            candidate(3, 0.5),
        ]]);
        stage().await.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.winners.len(), 1);
        assert_eq!(ctx.winners[0].campaign_id, 2);
        assert_eq!(ctx.outcome, PipelineOutcome::Bid);
    }

    #[tokio::test]
    async fn empty_candidates_gives_no_bid() {
        let mut ctx = make_ctx(vec![vec![]]);
        stage().await.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.winners.len(), 0);
        assert_eq!(
            ctx.outcome,
            PipelineOutcome::NoBid(NoBidReason::NO_ELIGIBLE_BIDS)
        );
    }

    #[tokio::test]
    async fn one_winner_per_impression() {
        let mut ctx = make_ctx(vec![
            vec![candidate(1, 0.9), candidate(2, 0.5)],
            vec![candidate(3, 0.7), candidate(4, 0.2)],
        ]);
        stage().await.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.winners.len(), 2);
        assert_eq!(ctx.winners[0].campaign_id, 1);
        assert_eq!(ctx.winners[1].campaign_id, 3);
    }

    #[tokio::test]
    async fn tie_broken_by_price() {
        let mut ctx = make_ctx(vec![vec![
            AdCandidate {
                campaign_id: 10,
                creative_id: 1,
                bid_price_cents: 50,
                score: 0.5,
                daily_cap_imps: u32::MAX,
                hourly_cap_imps: u32::MAX,
            },
            AdCandidate {
                campaign_id: 20,
                creative_id: 1,
                bid_price_cents: 200,
                score: 0.5,
                daily_cap_imps: u32::MAX,
                hourly_cap_imps: u32::MAX,
            },
        ]]);
        stage().await.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.winners[0].campaign_id, 20);
    }
}

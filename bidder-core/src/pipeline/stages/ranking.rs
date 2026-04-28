use crate::{
    model::{candidate::ImpWinner, context::BidContext, NoBidReason, PipelineOutcome},
    pipeline::stage::Stage,
};

/// Selects the highest-scoring candidate per impression.
///
/// Iterates candidates (already limited + scored) and picks the one with the
/// highest score, breaking ties by bid_price_cents. Populates ctx.winners.
/// If no impression has any candidate, sets NoBid(NO_ELIGIBLE_BIDS).
pub struct RankingStage;

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
        pipeline::stage::Stage,
    };

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
        }
    }

    #[tokio::test]
    async fn picks_highest_score() {
        let mut ctx = make_ctx(vec![vec![
            candidate(1, 0.3),
            candidate(2, 0.8),
            candidate(3, 0.5),
        ]]);
        RankingStage.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.winners.len(), 1);
        assert_eq!(ctx.winners[0].campaign_id, 2);
        assert_eq!(ctx.outcome, PipelineOutcome::Bid);
    }

    #[tokio::test]
    async fn empty_candidates_gives_no_bid() {
        let mut ctx = make_ctx(vec![vec![]]);
        RankingStage.execute(&mut ctx).await.unwrap();
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
        RankingStage.execute(&mut ctx).await.unwrap();
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
            },
            AdCandidate {
                campaign_id: 20,
                creative_id: 1,
                bid_price_cents: 200,
                score: 0.5,
            },
        ]]);
        RankingStage.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.winners[0].campaign_id, 20);
    }
}

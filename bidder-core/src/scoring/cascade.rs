//! Cascade scorer: cheap stage 1 over all candidates, expensive stage 2 over the
//! top-K survivors only.
//!
//! Both stages are full `Scorer`s, so any combination is composable: the typical
//! production wiring is `stage1=feature_weighted, stage2=ml`, but the type system
//! supports `stage1=ml, stage2=cascade(...)` if a multi-tier ranker is ever
//! needed.
//!
//! Threshold semantics:
//!   - `top_k > 0`: stage 2 runs on the top-K candidates by stage-1 score.
//!   - `threshold > 0`: stage 2 also requires stage-1 score >= threshold.
//!   - Both defaults to 0 → all candidates pass through (degenerate; logs at startup).
//!
//! CONTRACT: docs/SCORING-FEATURES.md § 5.

use crate::{
    model::candidate::AdCandidate,
    scoring::{Scorer, ScoringContext},
};
use async_trait::async_trait;
use std::sync::Arc;

pub struct CascadeScorer {
    pub stage1: Arc<dyn Scorer>,
    pub stage2: Arc<dyn Scorer>,
    /// How many candidates to forward to stage 2 (sorted by stage-1 score, desc).
    /// 0 disables the cap.
    pub top_k: usize,
    /// Minimum stage-1 score required to advance to stage 2. 0.0 disables.
    pub threshold: f32,
}

#[async_trait]
impl Scorer for CascadeScorer {
    async fn score_all(&self, candidates: &mut Vec<AdCandidate>, ctx: &ScoringContext<'_>) {
        if candidates.is_empty() {
            return;
        }
        // Stage 1: score everyone.
        self.stage1.score_all(candidates, ctx).await;

        // Pick which indices advance to stage 2: top_k by score, filtered by threshold.
        let mut indexed: Vec<(usize, f32)> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.score))
            .collect();
        // Partial sort by score desc — full sort is fine at typical N (few hundred).
        indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut advance: Vec<usize> = indexed
            .into_iter()
            .filter(|(_, score)| *score >= self.threshold)
            .map(|(i, _)| i)
            .collect();
        if self.top_k > 0 && advance.len() > self.top_k {
            advance.truncate(self.top_k);
        }
        if advance.is_empty() {
            metrics::counter!("bidder.scoring.cascade.empty_advance_total").increment(1);
            return;
        }

        // Build a contiguous slice for stage 2 (it expects &mut Vec).
        let mut stage2_batch: Vec<AdCandidate> =
            advance.iter().map(|&i| candidates[i].clone()).collect();
        self.stage2.score_all(&mut stage2_batch, ctx).await;

        // Write the stage-2 scores back. Candidates not in `advance` keep their
        // stage-1 scores — they're still ranked, just not re-evaluated.
        for (rank_idx, &original_idx) in advance.iter().enumerate() {
            candidates[original_idx].score = stage2_batch[rank_idx].score;
        }

        metrics::histogram!("bidder.scoring.cascade.advanced_count").record(advance.len() as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoring::FeatureWeightedScorer;

    /// A scorer that adds a fixed bonus — used to detect that stage 2 ran.
    struct PlusOne;

    #[async_trait]
    impl Scorer for PlusOne {
        async fn score_all(&self, candidates: &mut Vec<AdCandidate>, _: &ScoringContext<'_>) {
            for c in candidates.iter_mut() {
                c.score += 1.0;
            }
        }
    }

    fn ctx() -> ScoringContext<'static> {
        ScoringContext {
            segment_ids: &[],
            device_type: None,
            ad_format: None,
            hour_of_day: 12,
            user_id: "",
            is_top_market: false,
        }
    }

    fn cand(id: u32, price: i32) -> AdCandidate {
        AdCandidate {
            campaign_id: id,
            creative_id: 1,
            bid_price_cents: price,
            score: 0.0,
            daily_cap_imps: u32::MAX,
            hourly_cap_imps: u32::MAX,
        }
    }

    #[tokio::test]
    async fn top_k_limits_stage2() {
        let cascade = CascadeScorer {
            stage1: Arc::new(FeatureWeightedScorer::default()),
            stage2: Arc::new(PlusOne),
            top_k: 2,
            threshold: 0.0,
        };
        let mut candidates = vec![cand(1, 100), cand(2, 500), cand(3, 1000), cand(4, 50)];
        cascade.score_all(&mut candidates, &ctx()).await;
        // After stage 1 the top 2 are the highest priced; those plus stage 2 +1.
        // We can't predict exact scores after stage 1 mathematically here, but
        // we can assert exactly two candidates have score > 1.0 (stage-1 max
        // is < 1, so > 1.0 means stage 2 added 1.0).
        let advanced = candidates.iter().filter(|c| c.score > 1.0).count();
        assert_eq!(advanced, 2, "exactly top_k=2 candidates should advance");
    }

    #[tokio::test]
    async fn threshold_filters_below() {
        let cascade = CascadeScorer {
            stage1: Arc::new(FeatureWeightedScorer::default()),
            stage2: Arc::new(PlusOne),
            top_k: 0,
            threshold: 100.0, // unreachably high
        };
        let mut candidates = vec![cand(1, 100), cand(2, 500)];
        cascade.score_all(&mut candidates, &ctx()).await;
        for c in &candidates {
            assert!(
                c.score <= 1.0,
                "no candidate should advance when threshold is unreachable"
            );
        }
    }
}

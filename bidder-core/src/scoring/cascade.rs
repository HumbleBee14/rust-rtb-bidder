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

        // Pick which candidates advance to stage 2: top_k by score, filtered by
        // threshold. We key on the (campaign_id, creative_id) tuple so the
        // writeback survives any reordering or filtering stage 2 might do, AND
        // remains correct if a future change ever produces multiple candidates
        // for the same campaign with different creatives. Today
        // candidate_retrieval.rs picks one creative per campaign per imp, but
        // keying on the tuple removes any silent-collision risk.
        type Key = (u32, u32);
        let mut indexed: Vec<(Key, f32)> = candidates
            .iter()
            .map(|c| ((c.campaign_id, c.creative_id), c.score))
            .collect();
        indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut advance_keys: Vec<Key> = indexed
            .into_iter()
            .filter(|(_, score)| *score >= self.threshold)
            .map(|(k, _)| k)
            .collect();
        if self.top_k > 0 && advance_keys.len() > self.top_k {
            advance_keys.truncate(self.top_k);
        }
        if advance_keys.is_empty() {
            metrics::counter!("bidder.scoring.cascade.empty_advance_total").increment(1);
            return;
        }

        let advance_set: std::collections::HashSet<Key> = advance_keys.iter().copied().collect();
        let mut stage2_batch: Vec<AdCandidate> = candidates
            .iter()
            .filter(|c| advance_set.contains(&(c.campaign_id, c.creative_id)))
            .cloned()
            .collect();
        let advanced_count = stage2_batch.len();
        self.stage2.score_all(&mut stage2_batch, ctx).await;

        // Match stage-2 scores back to the original candidates by
        // (campaign_id, creative_id). The Scorer trait makes no guarantee that
        // score_all preserves input order or length — matching by ID is the
        // only correct way. Candidates that stage 2 dropped keep their stage-1
        // score (still ranked, just not re-evaluated).
        let stage2_scores: std::collections::HashMap<Key, f32> = stage2_batch
            .iter()
            .map(|c| ((c.campaign_id, c.creative_id), c.score))
            .collect();
        for c in candidates.iter_mut() {
            if let Some(&s) = stage2_scores.get(&(c.campaign_id, c.creative_id)) {
                c.score = s;
            }
        }

        metrics::histogram!("bidder.scoring.cascade.advanced_count").record(advanced_count as f64);
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
            is_weekend: false,
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

    /// A stage-2 scorer that REORDERS its input by campaign_id ascending and
    /// writes a unique score per campaign. Used to verify the cascade matches
    /// scores back by campaign_id, not by position. Without that fix, this test
    /// would assign the wrong score to each candidate.
    struct ReorderingMarker;

    #[async_trait]
    impl Scorer for ReorderingMarker {
        async fn score_all(&self, candidates: &mut Vec<AdCandidate>, _: &ScoringContext<'_>) {
            // Reorder by campaign_id ascending — opposite of the score-desc order
            // the cascade hands us in.
            candidates.sort_by_key(|c| c.campaign_id);
            // Tag each candidate with score = 100 + campaign_id so we can verify
            // the writeback found the right one.
            for c in candidates.iter_mut() {
                c.score = 100.0 + c.campaign_id as f32;
            }
        }
    }

    #[tokio::test]
    async fn writeback_matches_by_campaign_id_not_position() {
        let cascade = CascadeScorer {
            stage1: Arc::new(FeatureWeightedScorer::default()),
            stage2: Arc::new(ReorderingMarker),
            top_k: 0,
            threshold: 0.0,
        };
        // Construct in NON-ascending campaign_id order so the reordering inside
        // stage 2 is observable.
        let mut candidates = vec![cand(7, 100), cand(3, 200), cand(11, 300)];
        cascade.score_all(&mut candidates, &ctx()).await;
        // After cascade: each original candidate must carry score = 100 + its
        // own campaign_id. If writeback was by position, candidate[0] (id=7)
        // would have got score 103 (which would be id=3's score after sort).
        for c in &candidates {
            assert_eq!(
                c.score,
                100.0 + c.campaign_id as f32,
                "score for campaign {} should be {}, got {}",
                c.campaign_id,
                100.0 + c.campaign_id as f32,
                c.score
            );
        }
    }

    #[tokio::test]
    async fn writeback_handles_stage2_dropping_candidates() {
        // A stage 2 that DROPS some candidates entirely. The dropped ones must
        // keep their stage-1 score, not get zeroed.
        struct DropHalf;
        #[async_trait]
        impl Scorer for DropHalf {
            async fn score_all(&self, candidates: &mut Vec<AdCandidate>, _: &ScoringContext<'_>) {
                candidates.retain(|c| c.campaign_id % 2 == 0);
                for c in candidates.iter_mut() {
                    c.score = 0.99;
                }
            }
        }
        let cascade = CascadeScorer {
            stage1: Arc::new(FeatureWeightedScorer::default()),
            stage2: Arc::new(DropHalf),
            top_k: 0,
            threshold: 0.0,
        };
        let mut candidates = vec![cand(1, 100), cand(2, 200), cand(3, 300)];
        // Stage-1 scores aren't predictable here; capture them after stage 1
        // would run, by mocking. Easier: just assert the surviving condition —
        // even campaigns get score 0.99, odd campaigns retain their stage-1
        // score (which is < 1.0 by construction of FeatureWeightedScorer).
        cascade.score_all(&mut candidates, &ctx()).await;
        for c in &candidates {
            if c.campaign_id % 2 == 0 {
                assert!(
                    (c.score - 0.99).abs() < 1e-6,
                    "even campaign {} should have stage-2 score 0.99, got {}",
                    c.campaign_id,
                    c.score
                );
            } else {
                assert!(
                    c.score < 1.0 && c.score >= 0.0,
                    "odd campaign {} should retain stage-1 score in [0, 1), got {}",
                    c.campaign_id,
                    c.score
                );
            }
        }
    }

    #[tokio::test]
    async fn writeback_distinguishes_creatives_within_same_campaign() {
        // Today candidate_retrieval emits one entry per (campaign, imp), but
        // keying writeback on (campaign_id, creative_id) tuple guarantees we
        // stay correct if a future change ever produces multiple candidates
        // with the same campaign_id but different creatives (e.g. multi-format
        // negotiation, A/B'd creatives within a campaign).
        //
        // Stage 2 here writes a unique score per (campaign, creative) pair so
        // a HashMap<u32, f32> keyed on campaign_id alone would clobber one of
        // them; the tuple key keeps them distinct.
        struct StageTwo;
        #[async_trait]
        impl Scorer for StageTwo {
            async fn score_all(&self, candidates: &mut Vec<AdCandidate>, _: &ScoringContext<'_>) {
                for c in candidates.iter_mut() {
                    c.score = 100.0 + (c.campaign_id as f32) * 10.0 + (c.creative_id as f32);
                }
            }
        }
        let cascade = CascadeScorer {
            stage1: Arc::new(FeatureWeightedScorer::default()),
            stage2: Arc::new(StageTwo),
            top_k: 0,
            threshold: 0.0,
        };
        let mut candidates = vec![
            AdCandidate {
                campaign_id: 5,
                creative_id: 1,
                bid_price_cents: 100,
                score: 0.0,
                daily_cap_imps: u32::MAX,
                hourly_cap_imps: u32::MAX,
            },
            AdCandidate {
                campaign_id: 5,
                creative_id: 2,
                bid_price_cents: 200,
                score: 0.0,
                daily_cap_imps: u32::MAX,
                hourly_cap_imps: u32::MAX,
            },
        ];
        cascade.score_all(&mut candidates, &ctx()).await;
        // Each (campaign, creative) pair should have its own stage-2 score;
        // a campaign-only key would have left both with score 100+50+2 (last
        // write wins) or 100+50+1, depending on iteration order.
        assert_eq!(candidates[0].score, 100.0 + 50.0 + 1.0);
        assert_eq!(candidates[1].score, 100.0 + 50.0 + 2.0);
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

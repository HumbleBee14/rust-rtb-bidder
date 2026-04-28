//! A/B test scorer decorator.
//!
//! Routes a configurable fraction of traffic to a `treatment` scorer and the
//! rest to `control`. Assignment is deterministic per `user_id` — repeated
//! requests from the same user always land in the same arm — so a single
//! experiment generates stable per-user samples.
//!
//! Hashing is FNV-1a 64-bit — fast, deterministic, no crypto guarantees needed
//! (the assignment isn't security-sensitive). The `hash_seed` parameter prefixes
//! the user_id so multiple concurrent experiments don't entangle with each
//! other: an experiment with seed "rollout-A" and one with "rollout-B" hash the
//! same user_id to different positions.
//!
//! When `user_id` is empty the request gets the control arm — anonymous users
//! shouldn't skew an experiment that's measuring user-level outcomes.
//!
//! CONTRACT: docs/SCORING-FEATURES.md § 6 (version-rollout strategy).

use crate::{
    model::candidate::AdCandidate,
    scoring::{Scorer, ScoringContext},
};
use async_trait::async_trait;
use std::sync::Arc;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a(seed: &str, user_id: &str) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in seed.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h ^= b'|' as u64;
    h = h.wrapping_mul(FNV_PRIME);
    for &b in user_id.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

pub struct ABTestScorer {
    pub control: Arc<dyn Scorer>,
    pub treatment: Arc<dyn Scorer>,
    /// Treatment share in [0.0, 1.0]. 0.10 = 10% of users routed to treatment.
    pub treatment_share: f32,
    /// Disambiguates concurrent experiments; prefixes the hash input.
    pub hash_seed: String,
}

impl ABTestScorer {
    fn pick_arm(&self, user_id: &str) -> Arm {
        if user_id.is_empty() || self.treatment_share <= 0.0 {
            return Arm::Control;
        }
        if self.treatment_share >= 1.0 {
            return Arm::Treatment;
        }
        let h = fnv1a(&self.hash_seed, user_id);
        let bucket = (h % 10_000) as f32 / 10_000.0;
        if bucket < self.treatment_share {
            Arm::Treatment
        } else {
            Arm::Control
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    Control,
    Treatment,
}

impl Arm {
    fn label(self) -> &'static str {
        match self {
            Arm::Control => "control",
            Arm::Treatment => "treatment",
        }
    }
}

#[async_trait]
impl Scorer for ABTestScorer {
    async fn score_all(&self, candidates: &mut Vec<AdCandidate>, ctx: &ScoringContext<'_>) {
        let arm = self.pick_arm(ctx.user_id);
        metrics::counter!("bidder.scoring.ab_test.assignments_total", "arm" => arm.label())
            .increment(1);
        match arm {
            Arm::Control => self.control.score_all(candidates, ctx).await,
            Arm::Treatment => self.treatment.score_all(candidates, ctx).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoring::FeatureWeightedScorer;

    fn ctx_for(user_id: &'static str) -> ScoringContext<'static> {
        ScoringContext {
            segment_ids: &[],
            device_type: None,
            ad_format: None,
            hour_of_day: 12,
            user_id,
            is_top_market: false,
        }
    }

    #[tokio::test]
    async fn empty_user_id_always_goes_to_control() {
        let scorer = ABTestScorer {
            control: Arc::new(FeatureWeightedScorer::default()),
            treatment: Arc::new(FeatureWeightedScorer::default()),
            treatment_share: 1.0,
            hash_seed: "test".to_string(),
        };
        assert_eq!(scorer.pick_arm(""), Arm::Control);
    }

    #[test]
    fn hashing_is_stable() {
        let scorer = ABTestScorer {
            control: Arc::new(FeatureWeightedScorer::default()),
            treatment: Arc::new(FeatureWeightedScorer::default()),
            treatment_share: 0.5,
            hash_seed: "experiment-1".to_string(),
        };
        let a = scorer.pick_arm("user-42");
        let b = scorer.pick_arm("user-42");
        assert_eq!(a, b, "same user must always map to same arm");
    }

    #[test]
    fn different_seeds_route_independently() {
        let s1 = ABTestScorer {
            control: Arc::new(FeatureWeightedScorer::default()),
            treatment: Arc::new(FeatureWeightedScorer::default()),
            treatment_share: 0.5,
            hash_seed: "seed-A".to_string(),
        };
        let s2 = ABTestScorer {
            control: Arc::new(FeatureWeightedScorer::default()),
            treatment: Arc::new(FeatureWeightedScorer::default()),
            treatment_share: 0.5,
            hash_seed: "seed-B".to_string(),
        };
        // Across a population of users, seed-A and seed-B assignments should
        // differ for at least some — this is a crude smoke check.
        let mut differ = 0;
        for u in 0..1000 {
            let user = format!("u{}", u);
            if s1.pick_arm(&user) != s2.pick_arm(&user) {
                differ += 1;
            }
        }
        assert!(
            differ > 100,
            "different seeds should yield different assignments for >10% of users; got {}",
            differ
        );
    }

    #[test]
    fn treatment_share_approximates_target() {
        let scorer = ABTestScorer {
            control: Arc::new(FeatureWeightedScorer::default()),
            treatment: Arc::new(FeatureWeightedScorer::default()),
            treatment_share: 0.10,
            hash_seed: "rollout".to_string(),
        };
        let mut treatment = 0;
        let n = 10_000;
        for u in 0..n {
            if scorer.pick_arm(&format!("u{}", u)) == Arm::Treatment {
                treatment += 1;
            }
        }
        let actual = treatment as f32 / n as f32;
        // Expect ~10%; tolerate ±2 percentage points (well within FNV-1a noise at n=10K).
        assert!(
            (actual - 0.10).abs() < 0.02,
            "treatment_share=0.10 should yield ~10% assignments; got {}",
            actual
        );
    }

    #[tokio::test]
    async fn ctx_for_compiles() {
        // smoke: ScoringContext borrows compile cleanly through the decorator.
        let scorer = ABTestScorer {
            control: Arc::new(FeatureWeightedScorer::default()),
            treatment: Arc::new(FeatureWeightedScorer::default()),
            treatment_share: 0.5,
            hash_seed: "x".to_string(),
        };
        let mut candidates = vec![AdCandidate {
            campaign_id: 1,
            creative_id: 1,
            bid_price_cents: 100,
            score: 0.0,
            daily_cap_imps: u32::MAX,
            hourly_cap_imps: u32::MAX,
        }];
        scorer.score_all(&mut candidates, &ctx_for("user-1")).await;
    }
}

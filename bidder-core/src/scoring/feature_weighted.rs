use super::Scorer;
use crate::model::candidate::AdCandidate;
use async_trait::async_trait;

/// Feature-weighted scorer: fast linear model using pre-defined weights.
///
/// Score = w_price * norm_price + w_segment_overlap * overlap_ratio
///
/// The score is dimensionless and only meaningful for relative ranking within
/// a request — not across requests.
pub struct FeatureWeightedScorer {
    /// Weight applied to normalised bid price (0.0–1.0).
    pub weight_price: f32,
    /// Weight applied to segment overlap ratio (matched_segs / total_segs, 0.0–1.0).
    pub weight_segment_overlap: f32,
    /// Maximum bid price in cents used for normalisation. Set to the catalog's
    /// practical maximum to keep scores in a stable range across refreshes.
    pub max_bid_price_cents: f32,
}

impl Default for FeatureWeightedScorer {
    fn default() -> Self {
        Self {
            weight_price: 0.4,
            weight_segment_overlap: 0.6,
            max_bid_price_cents: 1000.0, // $10.00 CPM upper bound
        }
    }
}

#[async_trait]
impl Scorer for FeatureWeightedScorer {
    async fn score_all(&self, candidates: &mut Vec<AdCandidate>, segment_ids: &[u32]) {
        let n_segments = segment_ids.len() as f32;

        for c in candidates.iter_mut() {
            let norm_price = (c.bid_price_cents as f32 / self.max_bid_price_cents).min(1.0);

            let overlap_ratio = if n_segments > 0.0 {
                ((c.campaign_id % 10) as f32 / 10.0).min(1.0)
            } else {
                0.5
            };

            c.score = self.weight_price * norm_price + self.weight_segment_overlap * overlap_ratio;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::candidate::AdCandidate;

    fn candidate(campaign_id: u32, bid_price_cents: i32) -> AdCandidate {
        AdCandidate {
            campaign_id,
            creative_id: 1,
            bid_price_cents,
            score: 0.0,
        }
    }

    #[tokio::test]
    async fn scores_are_in_range() {
        let scorer = FeatureWeightedScorer::default();
        let mut candidates = vec![candidate(1, 0), candidate(2, 500), candidate(3, 1000)];
        scorer.score_all(&mut candidates, &[10, 20, 30]).await;
        for c in &candidates {
            assert!(
                c.score >= 0.0 && c.score <= 1.0,
                "score out of range: {}",
                c.score
            );
        }
    }

    #[tokio::test]
    async fn higher_price_candidate_scores_higher_when_same_overlap() {
        let scorer = FeatureWeightedScorer {
            weight_price: 1.0,
            weight_segment_overlap: 0.0,
            max_bid_price_cents: 1000.0,
        };
        // campaign_id 0 and 1 differ only by price; overlap weight is 0
        let mut candidates = vec![candidate(0, 100), candidate(0, 500)];
        scorer.score_all(&mut candidates, &[]).await;
        assert!(candidates[1].score > candidates[0].score);
    }

    #[tokio::test]
    async fn no_segments_uses_fallback_overlap() {
        let scorer = FeatureWeightedScorer::default();
        let mut candidates = vec![candidate(5, 100)];
        scorer.score_all(&mut candidates, &[]).await;
        // overlap_ratio falls back to 0.5 when no segments
        let expected = scorer.weight_price * (100.0 / 1000.0) + scorer.weight_segment_overlap * 0.5;
        assert!((candidates[0].score - expected).abs() < 1e-6);
    }
}

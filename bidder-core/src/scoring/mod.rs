mod feature_weighted;
pub use feature_weighted::FeatureWeightedScorer;

use crate::model::candidate::AdCandidate;
use async_trait::async_trait;

/// Assigns a relevance score to each candidate in a batch.
///
/// `score_all` encodes request features once and reuses them across all
/// candidates — avoiding per-candidate setup cost.
#[async_trait]
pub trait Scorer: Send + Sync + 'static {
    async fn score_all(&self, candidates: &mut Vec<AdCandidate>, segment_ids: &[u32]);
}

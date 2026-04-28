//! Scoring trait and implementations.
//!
//! Three scorers ship in Phase 6:
//!   - `FeatureWeightedScorer` — cheap linear combo of price + segment overlap.
//!   - `MLScorer` — ONNX inference via `ort`. Hot-reloads on model file change.
//!   - `CascadeScorer` — `stage1` for all candidates, `stage2` for top-K survivors.
//!
//! And one decorator:
//!   - `ABTestScorer` — hashes `user_id` to a deterministic variant, routes the
//!     request to one of two scorers, tags the variant in metrics.
//!
//! All scorers receive a borrowed `ScoringContext` so the request features
//! (segments, device, format, hour) are available without re-extracting from
//! the OpenRTB tree. The trait stays object-safe; we promote to generics only
//! if profiling shows dyn-dispatch overhead is measurable.

mod ab_test;
mod cascade;
mod feature_weighted;
mod features;
mod ml;

pub use ab_test::ABTestScorer;
pub use cascade::CascadeScorer;
pub use feature_weighted::FeatureWeightedScorer;
pub use features::{ScoringFeatures, FEATURE_COUNT};
pub use ml::{MLScorer, MLScorerConfig};

use crate::{
    catalog::{DeviceTargetType, SegmentId},
    model::{candidate::AdCandidate, openrtb::AdFormat},
};
use async_trait::async_trait;

/// Per-request features the scorer can read. Borrowed; lifetime tied to the
/// pipeline call.
///
/// Adding a field here is a contract change with the data-science training
/// pipeline — see `docs/SCORING-FEATURES.md`. Don't add fields without
/// updating the schema and the parity-test fixture.
#[derive(Debug, Clone, Copy)]
pub struct ScoringContext<'a> {
    pub segment_ids: &'a [SegmentId],
    pub device_type: Option<DeviceTargetType>,
    pub ad_format: Option<AdFormat>,
    /// 0–23, in UTC. The training pipeline must use the same timezone.
    pub hour_of_day: u8,
    /// Saturday or Sunday in UTC. Derived from the clock by ScoringStage
    /// so the feature extractor doesn't need timezone awareness.
    pub is_weekend: bool,
    /// Anonymous-safe user identifier. Empty when the bid request omits user.id.
    /// Used by `ABTestScorer` for variant assignment; `FeatureWeighted` and `ML`
    /// ignore it.
    pub user_id: &'a str,
    /// Top-market geo flag. `true` when the request's geo lands in the
    /// pre-defined top-N markets (Phase 6: top-10 metros). False otherwise or
    /// when geo is missing.
    pub is_top_market: bool,
}

/// Assigns a relevance score to each candidate in a batch.
///
/// Scorers mutate `AdCandidate.score`. Higher = better; range is scorer-defined
/// but Phase 6 contract is `[0, 1]` so cross-scorer ranking is comparable.
#[async_trait]
pub trait Scorer: Send + Sync + 'static {
    async fn score_all(&self, candidates: &mut Vec<AdCandidate>, ctx: &ScoringContext<'_>);
}

use crate::{
    catalog::{CampaignCatalog, SegmentId},
    model::{
        candidate::{AdCandidate, ImpWinner},
        openrtb::{BidRequest, BidResponse, NoBidReason},
    },
};
use std::{sync::Arc, time::Instant};

/// Per-request mutable state threaded through the pipeline.
///
/// Owned, not pooled — standard heap allocation under jemalloc.
/// Arena allocation is a Phase 7 experiment; this lifetime is simple and correct.
#[derive(Debug)]
pub struct BidContext {
    pub request: BidRequest,
    pub started_at: Instant,
    pub outcome: PipelineOutcome,
    /// Resolved segment IDs for the requesting user. Populated by UserEnrichmentStage.
    pub segment_ids: Vec<SegmentId>,
    /// Catalog snapshot held for the duration of this request. Populated by UserEnrichmentStage.
    pub catalog: Option<Arc<CampaignCatalog>>,
    /// Candidates from CandidateRetrievalStage → CandidateLimitStage → ScoringStage.
    /// One Vec per impression (aligned with request.imp by index).
    pub candidates: Vec<Vec<AdCandidate>>,
    /// Final winner per impression after RankingStage.
    /// Populated only when at least one imp has a winner.
    pub winners: Vec<ImpWinner>,
    /// Freq-cap check results per candidate, keyed by (imp_index, campaign_id).
    /// None means freq-cap was skipped (timeout fallback); Some(true) = capped.
    pub freq_cap_results: Vec<(usize, u32, bool)>,
    /// Populated by ResponseBuildStage. None until that stage runs.
    pub bid_response: Option<BidResponse>,
}

impl BidContext {
    pub fn new(request: BidRequest) -> Self {
        let imp_count = request.imp.len();
        Self {
            request,
            started_at: Instant::now(),
            outcome: PipelineOutcome::Pending,
            segment_ids: Vec::new(),
            catalog: None,
            candidates: vec![Vec::new(); imp_count.max(1)],
            winners: Vec::new(),
            freq_cap_results: Vec::new(),
            bid_response: None,
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        (self.started_at.elapsed().as_secs_f64() * 1000.0) as u64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipelineOutcome {
    Pending,
    NoBid(NoBidReason),
    Bid,
}

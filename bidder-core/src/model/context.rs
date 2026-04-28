use crate::{
    catalog::{CampaignCatalog, SegmentId},
    model::openrtb::{BidRequest, NoBidReason},
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
}

impl BidContext {
    pub fn new(request: BidRequest) -> Self {
        Self {
            request,
            started_at: Instant::now(),
            outcome: PipelineOutcome::Pending,
            segment_ids: Vec::new(),
            catalog: None,
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
    // Bid variant added Phase 4 when real candidates exist.
}

use crate::model::openrtb::{BidRequest, NoBidReason};
use std::time::Instant;

/// Per-request mutable state threaded through the pipeline.
///
/// Owned, not pooled — standard heap allocation under jemalloc.
/// Arena allocation is a Phase 7 experiment; this lifetime is simple and correct.
#[derive(Debug)]
pub struct BidContext {
    pub request: BidRequest,
    pub started_at: Instant,
    pub outcome: PipelineOutcome,
}

impl BidContext {
    pub fn new(request: BidRequest) -> Self {
        Self {
            request,
            started_at: Instant::now(),
            outcome: PipelineOutcome::Pending,
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

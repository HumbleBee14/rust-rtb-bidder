use bidder_core::{health::HealthState, pipeline::Pipeline};
use std::sync::Arc;

/// Shared state injected into every request handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub health: HealthState,
    pub pipeline: Arc<Pipeline>,
}

impl AppState {
    pub fn new(health: HealthState, pipeline: Pipeline) -> Self {
        Self {
            health,
            pipeline: Arc::new(pipeline),
        }
    }
}

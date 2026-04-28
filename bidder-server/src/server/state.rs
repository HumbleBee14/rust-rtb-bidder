use bidder_core::{
    cache::SegmentCache, catalog::SharedCatalog, health::HealthState, pipeline::Pipeline,
};
use fred::clients::Pool as RedisPool;
use std::sync::Arc;

/// Shared state injected into every request handler via axum's `State` extractor.
#[derive(Clone)]
#[allow(dead_code)]
pub struct AppState {
    pub health: HealthState,
    pub pipeline: Arc<Pipeline>,
    pub catalog: SharedCatalog,
    pub redis: RedisPool,
    pub segment_cache: SegmentCache,
}

impl AppState {
    pub fn new(
        health: HealthState,
        pipeline: Pipeline,
        catalog: SharedCatalog,
        redis: RedisPool,
        segment_cache: SegmentCache,
    ) -> Self {
        Self {
            health,
            pipeline: Arc::new(pipeline),
            catalog,
            redis,
            segment_cache,
        }
    }
}

use bidder_core::{
    cache::SegmentCache, catalog::SharedCatalog, health::HealthState, pipeline::Pipeline,
};
use fred::clients::Pool as RedisPool;
use std::sync::Arc;

/// Shared state injected into every request handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub health: HealthState,
    pub pipeline: Arc<Pipeline>,
    #[allow(dead_code)] // Phase 4: used by admin/stats handlers
    pub catalog: SharedCatalog,
    #[allow(dead_code)] // Phase 4: used by cache-invalidation handler
    pub redis: RedisPool,
    #[allow(dead_code)] // Phase 4: used by bid handler
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

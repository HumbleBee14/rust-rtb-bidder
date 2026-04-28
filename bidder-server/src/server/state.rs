use bidder_core::{
    cache::SegmentCache, catalog::SharedCatalog, frequency::ImpressionRecorder,
    health::HealthState, pipeline::Pipeline,
};
use fred::clients::Pool as RedisPool;
use std::sync::Arc;

/// Shared state injected into every request handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub health: HealthState,
    pub pipeline: Arc<Pipeline>,
    #[allow(dead_code)] // Phase 5: admin/stats handlers
    pub catalog: SharedCatalog,
    #[allow(dead_code)] // Phase 5: cache-invalidation handler
    pub redis: RedisPool,
    #[allow(dead_code)] // Phase 5: direct cache access by admin handler
    pub segment_cache: SegmentCache,
    pub impression_recorder: Arc<ImpressionRecorder>,
}

impl AppState {
    pub fn new(
        health: HealthState,
        pipeline: Pipeline,
        catalog: SharedCatalog,
        redis: RedisPool,
        segment_cache: SegmentCache,
        impression_recorder: Arc<ImpressionRecorder>,
    ) -> Self {
        Self {
            health,
            pipeline: Arc::new(pipeline),
            catalog,
            redis,
            segment_cache,
            impression_recorder,
        }
    }
}

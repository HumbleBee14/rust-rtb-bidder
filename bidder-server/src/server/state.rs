use crate::win_notice::WinNoticeGateService;
use bidder_core::{
    cache::SegmentCache,
    catalog::SharedCatalog,
    events::EventPublisher,
    exchange::ExchangeAdapter,
    frequency::ImpressionRecorder,
    health::HealthState,
    hedge_feedback::{LoadShedTracker, RedisLatencyTracker},
    pipeline::Pipeline,
};
use fred::clients::Pool as RedisPool;
use std::sync::Arc;

/// Shared state injected into every request handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub health: HealthState,
    pub pipeline: Arc<Pipeline>,
    #[allow(dead_code)] // admin/stats handlers
    pub catalog: SharedCatalog,
    #[allow(dead_code)] // cache-invalidation handler
    pub redis: RedisPool,
    #[allow(dead_code)] // direct cache access by admin handler
    pub segment_cache: SegmentCache,
    pub impression_recorder: Arc<ImpressionRecorder>,
    pub event_publisher: Arc<dyn EventPublisher>,
    /// Kafka topic for all AdEvent messages. Driven by config.kafka.events_topic.
    pub events_topic: Arc<str>,
    /// Win-notice HMAC verification + Redis SET-NX dedup.
    pub win_notice_gate: Arc<WinNoticeGateService>,
    /// Exchange adapter — encodes/decodes wire bytes ↔ internal `BidRequest`/
    /// `BidResponse`. Today every route uses a single adapter from config;
    /// Phase 7 multi-exchange routes wire one adapter per route via
    /// `Router::route(...).with_state(...)`.
    pub adapter: Arc<dyn ExchangeAdapter>,
    /// Tracks accept/shed counts so the hedge-budget feedback loop can
    /// compute load_shed_rate over a rolling window. Incremented by the
    /// HTTP timeout middleware on every request and on every 503 response.
    pub load_shed_tracker: Arc<LoadShedTracker>,
    /// Records observed Redis call durations from the freq-cap and segment
    /// repo paths. The hedge-feedback loop drains this every interval to
    /// derive a coarse p95 estimate that drives the hedge trigger.
    #[allow(dead_code)] // held for future admin/debug handlers; loop reads via Arc clone
    pub redis_latency_tracker: Arc<RedisLatencyTracker>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        health: HealthState,
        pipeline: Pipeline,
        catalog: SharedCatalog,
        redis: RedisPool,
        segment_cache: SegmentCache,
        impression_recorder: Arc<ImpressionRecorder>,
        event_publisher: Arc<dyn EventPublisher>,
        events_topic: Arc<str>,
        win_notice_gate: Arc<WinNoticeGateService>,
        adapter: Arc<dyn ExchangeAdapter>,
        load_shed_tracker: Arc<LoadShedTracker>,
        redis_latency_tracker: Arc<RedisLatencyTracker>,
    ) -> Self {
        Self {
            health,
            pipeline: Arc::new(pipeline),
            catalog,
            redis,
            segment_cache,
            impression_recorder,
            event_publisher,
            events_topic,
            win_notice_gate,
            adapter,
            load_shed_tracker,
            redis_latency_tracker,
        }
    }
}

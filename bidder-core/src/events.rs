use bidder_protos::events::AdEvent;

/// Synchronous, non-blocking event publisher.
///
/// Phase 8 contract: `publish` MUST return immediately — no `.await`, no
/// blocking syscalls, no allocator-pressure paths beyond a small encode +
/// queue-push. This guarantees that the bid-path caller does not pay any
/// scheduler or backpressure cost for events; if the underlying queue
/// (e.g. rdkafka's internal send queue) is full, the implementation drops
/// the event silently and records `bidder.kafka.events_dropped`.
///
/// Earlier (Phase 5–7) shape was `async fn publish(&self, ...)` invoked
/// inside `tokio::spawn(async move { publisher.publish(..).await })`.
/// That added one task allocation per winning bid (~100K/s at 50K RPS ×
/// avg 2 winners) and pushed real backpressure visibility *behind* the
/// spawn boundary. Phase 8 collapses both: callers call `publish()` once,
/// inline, and rdkafka's bounded queue + `queue.full.behavior=error`
/// becomes the actual backpressure surface (as the architecture diagram
/// always claimed).
pub trait EventPublisher: Send + Sync + 'static {
    /// Enqueue an event for async delivery. Must never block.
    /// On queue-full the implementation drops and increments
    /// `bidder.kafka.events_dropped`.
    fn publish(&self, topic: &str, key: &[u8], event: AdEvent);
}

pub struct NoOpEventPublisher;

impl EventPublisher for NoOpEventPublisher {
    fn publish(&self, _topic: &str, _key: &[u8], _event: AdEvent) {}
}

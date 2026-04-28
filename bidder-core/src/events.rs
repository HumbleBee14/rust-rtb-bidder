use async_trait::async_trait;
use bidder_protos::events::AdEvent;

#[async_trait]
pub trait EventPublisher: Send + Sync + 'static {
    /// Enqueue an event for async delivery. Must never block the caller.
    /// On queue-full the implementation drops and increments `bidder.kafka.events_dropped`.
    async fn publish(&self, topic: &str, key: &[u8], event: AdEvent);
}

pub struct NoOpEventPublisher;

#[async_trait]
impl EventPublisher for NoOpEventPublisher {
    async fn publish(&self, _topic: &str, _key: &[u8], _event: AdEvent) {}
}

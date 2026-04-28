use crate::catalog::SegmentId;

/// Fetches the segment IDs for a given user from Redis.
///
/// Implemented by the live Redis-backed adapter in bidder-server.
/// Kept as a trait so pipeline stages don't import fred directly.
#[async_trait::async_trait]
pub trait UserSegmentRepository: Send + Sync + 'static {
    async fn segments_for(&self, user_id: &str) -> anyhow::Result<Vec<SegmentId>>;
}

use async_trait::async_trait;
use bidder_core::{
    catalog::SegmentId, repository::UserSegmentRepository, targeting::SegmentRegistry,
};
use fred::{clients::Pool as RedisPool, prelude::*};
use std::sync::Arc;

/// Redis-backed user segment repository.
///
/// Keys follow `user_segments:{user_id}` and hold a SMEMBERS set of
/// segment name strings. The segment registry resolves names to IDs.
pub struct RedisSegmentRepo {
    pool: RedisPool,
    registry: Arc<SegmentRegistry>,
}

impl RedisSegmentRepo {
    pub fn new(pool: RedisPool, registry: Arc<SegmentRegistry>) -> Self {
        Self { pool, registry }
    }
}

#[async_trait]
impl UserSegmentRepository for RedisSegmentRepo {
    async fn segments_for(&self, user_id: &str) -> anyhow::Result<Vec<SegmentId>> {
        let key = format!("user_segments:{user_id}");
        let names: Vec<String> = self.pool.smembers(&key).await?;
        Ok(self.registry.resolve(&names))
    }
}

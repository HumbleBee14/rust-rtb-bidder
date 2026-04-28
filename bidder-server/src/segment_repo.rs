use async_trait::async_trait;
use bidder_core::{
    catalog::{SegmentId, SharedRegistry},
    repository::UserSegmentRepository,
};
use fred::{clients::Pool as RedisPool, prelude::*};

/// Redis-backed user segment repository.
///
/// Keys follow `user_segments:{user_id}` and hold a SMEMBERS set of
/// segment name strings. The segment registry resolves names to IDs.
pub struct RedisSegmentRepo {
    pool: RedisPool,
    registry: SharedRegistry,
}

impl RedisSegmentRepo {
    pub fn new(pool: RedisPool, registry: SharedRegistry) -> Self {
        Self { pool, registry }
    }
}

#[async_trait]
impl UserSegmentRepository for RedisSegmentRepo {
    async fn segments_for(&self, user_id: &str) -> anyhow::Result<Vec<SegmentId>> {
        let key = format!("user_segments:{user_id}");
        let names: Vec<String> = self.pool.smembers(&key).await?;
        Ok(self.registry.load_full().resolve(&names))
    }
}

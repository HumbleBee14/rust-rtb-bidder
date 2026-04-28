use async_trait::async_trait;
use bidder_core::{
    breaker::{BreakerConfig, CircuitBreaker},
    catalog::SegmentId,
    repository::UserSegmentRepository,
};
use fred::{clients::Pool as RedisPool, interfaces::KeysInterface};
use std::sync::Arc;

/// Redis-backed user segment repository with circuit-breaker protection.
///
/// Key:   v1:seg:{u:<userId>}
/// Value: raw little-endian packed u32 segment IDs (no header, no name→ID hop).
/// See docs/REDIS-KEYS.md § "Family: User segments".
pub struct RedisSegmentRepo {
    pool: RedisPool,
    breaker: Arc<CircuitBreaker>,
}

impl RedisSegmentRepo {
    pub fn new(pool: RedisPool) -> Self {
        Self {
            pool,
            breaker: Arc::new(CircuitBreaker::new(BreakerConfig::redis("segment_redis"))),
        }
    }
}

#[async_trait]
impl UserSegmentRepository for RedisSegmentRepo {
    #[tracing::instrument(
        name = "redis.get",
        skip(self),
        fields(
            db.system = "redis",
            db.operation = "GET",
            bidder.redis.dependency = "segment_repo",
        )
    )]
    async fn segments_for(&self, user_id: &str) -> anyhow::Result<Vec<SegmentId>> {
        if !self.breaker.allow_request().await {
            metrics::counter!("bidder.segment_repo.skipped", "reason" => "breaker_open")
                .increment(1);
            return Ok(Vec::new());
        }
        let start = std::time::Instant::now();
        let key = format!("v1:seg:{{u:{user_id}}}");
        let result: anyhow::Result<Option<bytes::Bytes>> =
            self.pool.get(&key).await.map_err(Into::into);
        let bytes = match result {
            Err(e) => {
                self.breaker.record_outcome(true, start.elapsed()).await;
                return Err(e);
            }
            Ok(b) => {
                self.breaker.record_outcome(false, start.elapsed()).await;
                b
            }
        };
        let Some(bytes) = bytes else {
            return Ok(Vec::new());
        };
        if bytes.len() % 4 != 0 {
            anyhow::bail!(
                "segment value for {key} has length {} — not a multiple of 4",
                bytes.len()
            );
        }
        let ids = bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Ok(ids)
    }
}

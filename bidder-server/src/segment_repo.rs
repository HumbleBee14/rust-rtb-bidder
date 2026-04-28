use async_trait::async_trait;
use bidder_core::{catalog::SegmentId, repository::UserSegmentRepository};
use fred::{clients::Pool as RedisPool, interfaces::KeysInterface};

/// Redis-backed user segment repository.
///
/// Key:   v1:seg:{u:<userId>}
/// Value: raw little-endian packed u32 segment IDs (no header, no name→ID hop).
/// See docs/REDIS-KEYS.md § "Family: User segments".
pub struct RedisSegmentRepo {
    pool: RedisPool,
}

impl RedisSegmentRepo {
    pub fn new(pool: RedisPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UserSegmentRepository for RedisSegmentRepo {
    async fn segments_for(&self, user_id: &str) -> anyhow::Result<Vec<SegmentId>> {
        let key = format!("v1:seg:{{u:{user_id}}}");
        let bytes: Option<bytes::Bytes> = self.pool.get(&key).await?;
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

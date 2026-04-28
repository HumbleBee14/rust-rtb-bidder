use async_trait::async_trait;
use bidder_core::{
    breaker::CircuitBreaker,
    catalog::SegmentId,
    hedge::{hedged_call, HedgeBudget},
    repository::UserSegmentRepository,
};
use fred::{clients::Pool as RedisPool, interfaces::KeysInterface};
use std::{sync::Arc, time::Duration};

/// Redis-backed user segment repository with circuit-breaker and hedged-read protection.
///
/// Key:   v1:seg:{u:<userId>}
/// Value: raw little-endian packed u32 segment IDs.
/// See docs/REDIS-KEYS.md § "Family: User segments".
pub struct RedisSegmentRepo {
    pool: RedisPool,
    /// Shared per-Redis-dependency circuit breaker.
    breaker: Arc<CircuitBreaker>,
    /// Shared hedge budget.
    hedge: Arc<HedgeBudget>,
    /// Records observed call durations for the hedge feedback loop's
    /// p95 estimator. Optional: when None, the loop's p95 derives from
    /// freq_cap calls only.
    latency_tracker: Option<Arc<bidder_core::hedge_feedback::RedisLatencyTracker>>,
}

impl RedisSegmentRepo {
    pub fn new(
        pool: RedisPool,
        breaker: Arc<CircuitBreaker>,
        hedge: Arc<HedgeBudget>,
        latency_tracker: Option<Arc<bidder_core::hedge_feedback::RedisLatencyTracker>>,
    ) -> Self {
        Self {
            pool,
            breaker,
            hedge,
            latency_tracker,
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
        let pool = self.pool.clone();
        let key_ref = key.clone();

        // Hedge trigger: 8 ms floor until p95 tracking is wired in Phase 7.
        let hedge_trigger = Duration::from_millis(8);
        let result: anyhow::Result<Option<bytes::Bytes>> = hedged_call(
            || {
                let pool = pool.clone();
                let k = key_ref.clone();
                async move {
                    pool.get::<Option<bytes::Bytes>, _>(&k)
                        .await
                        .map_err(Into::into)
                }
            },
            hedge_trigger,
            &self.breaker,
            &self.hedge,
        )
        .await;

        let elapsed = start.elapsed();
        if let Some(t) = &self.latency_tracker {
            t.record(elapsed);
        }
        let bytes = match result {
            Err(e) => {
                self.breaker.record_outcome(true, elapsed).await;
                return Err(e);
            }
            Ok(b) => {
                self.breaker.record_outcome(false, elapsed).await;
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

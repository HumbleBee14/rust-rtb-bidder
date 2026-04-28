use async_trait::async_trait;
use bidder_core::{
    breaker::CircuitBreaker,
    frequency::{CapResult, CapWindow, FreqCapOutcome, FrequencyCapper},
    hedge::{hedged_call, HedgeBudget},
    model::candidate::AdCandidate,
};
use fred::{clients::Pool as RedisPool, interfaces::KeysInterface};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;
use tracing::warn;

/// Redis-backed frequency capper with circuit-breaker and hedged-read protection.
///
/// Per request: builds one MGET for all cap keys (campaign-day + campaign-hour).
/// If the breaker is open, returns `SkippedTimeout` immediately.
/// If the MGET exceeds `timeout_ms`, records the timeout and returns `SkippedTimeout`.
/// Hedging fires a second parallel MGET if the first hasn't returned by the hedge trigger.
///
/// Key shape: `v1:fc:{u:<userId>}:<dim>:<dimVal>:<window>` — see REDIS-KEYS.md.
pub struct RedisFrequencyCapper {
    pool: RedisPool,
    /// MGET timeout. Config: latency_budget.frequency_cap_ms.
    timeout_ms: u64,
    /// Shared per-Redis-dependency circuit breaker.
    breaker: Arc<CircuitBreaker>,
    /// Shared hedge budget + p95 tracker.
    hedge: Arc<HedgeBudget>,
}

impl RedisFrequencyCapper {
    pub fn new(
        pool: RedisPool,
        timeout_ms: u64,
        breaker: Arc<CircuitBreaker>,
        hedge: Arc<HedgeBudget>,
    ) -> Self {
        Self {
            pool,
            timeout_ms,
            breaker,
            hedge,
        }
    }
}

#[async_trait]
impl FrequencyCapper for RedisFrequencyCapper {
    #[tracing::instrument(
        name = "redis.mget",
        skip(self, candidates),
        fields(
            db.system = "redis",
            db.operation = "MGET",
            bidder.redis.dependency = "freq_cap",
        )
    )]
    async fn check(
        &self,
        user_id: &str,
        candidates: &[AdCandidate],
        _device_type_val: u8,
        _hour_of_day: u8,
    ) -> FreqCapOutcome {
        if candidates.is_empty() {
            return FreqCapOutcome::Checked(Vec::new());
        }

        let mut keys: Vec<String> = Vec::with_capacity(candidates.len() * 2);
        for c in candidates {
            keys.push(fc_key(user_id, "c", c.campaign_id, CapWindow::Day));
            keys.push(fc_key(user_id, "c", c.campaign_id, CapWindow::Hour));
        }

        if !self.breaker.allow_request().await {
            metrics::counter!("bidder.freq_cap.skipped", "reason" => "breaker_open").increment(1);
            return FreqCapOutcome::SkippedTimeout;
        }

        let start = std::time::Instant::now();
        // Hedge trigger: max(p95, 8 ms floor). HedgeBudget doesn't track p95 itself;
        // the RedisHedgeState wrapper does. Here we use the conservative 8 ms floor
        // until p95 tracking is wired in Phase 7.
        let hedge_trigger = Duration::from_millis(8);
        let pool = self.pool.clone();
        let keys_ref = &keys;

        let mget_result = timeout(
            Duration::from_millis(self.timeout_ms),
            hedged_call(
                || {
                    let pool = pool.clone();
                    let k = keys_ref.to_vec();
                    async move { pool.mget::<Vec<Option<i64>>, _>(k).await }
                },
                hedge_trigger,
                &self.breaker,
                &self.hedge,
            ),
        )
        .await;

        match mget_result {
            Err(_elapsed) => {
                self.breaker.record_outcome(true, start.elapsed()).await;
                metrics::counter!("bidder.freq_cap.skipped", "reason" => "timeout").increment(1);
                FreqCapOutcome::SkippedTimeout
            }
            Ok(Err(e)) => {
                self.breaker.record_outcome(true, start.elapsed()).await;
                warn!(error = %e, "freq cap MGET failed — skipping");
                metrics::counter!("bidder.freq_cap.skipped", "reason" => "redis_error")
                    .increment(1);
                FreqCapOutcome::SkippedTimeout
            }
            Ok(Ok(values)) => {
                self.breaker.record_outcome(false, start.elapsed()).await;
                // values aligned 2:1 with candidates: [day, hour, day, hour, ...]
                let results = candidates
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let day_count = values.get(i * 2).copied().flatten().unwrap_or(0);
                        let hour_count = values.get(i * 2 + 1).copied().flatten().unwrap_or(0);
                        // Per-campaign caps loaded from the catalog at retrieval time.
                        // i64 compare against u32 is safe — Redis counters are non-negative.
                        let capped = day_count >= c.daily_cap_imps as i64
                            || hour_count >= c.hourly_cap_imps as i64;
                        CapResult {
                            campaign_id: c.campaign_id,
                            capped,
                        }
                    })
                    .collect();
                FreqCapOutcome::Checked(results)
            }
        }
    }
}

/// Workers that consume `ImpressionEvent`s and write freq-cap counters via
/// a Lua script: atomic INCR + EXPIRE on first increment.
pub fn spawn_impression_workers(
    pool: RedisPool,
    mut rx: tokio::sync::mpsc::Receiver<bidder_core::frequency::ImpressionEvent>,
    num_workers: usize,
) {
    let pool = std::sync::Arc::new(pool);
    tokio::spawn(async move {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(num_workers.max(1)));
        while let Some(event) = rx.recv().await {
            let pool = std::sync::Arc::clone(&pool);
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            tokio::spawn(async move {
                let _permit = permit;
                write_impression_counters(&pool, &event).await;
            });
        }
    });
}

async fn write_impression_counters(
    pool: &RedisPool,
    event: &bidder_core::frequency::ImpressionEvent,
) {
    // Lua: INCR the key; if result == 1 (first increment), set EXPIRE.
    const SCRIPT: &str = r#"
local current = redis.call('INCR', KEYS[1])
if current == 1 then
  redis.call('EXPIRE', KEYS[1], ARGV[1])
end
return current
"#;

    let windows: &[(&str, u32, &str)] = &[("h", 3600, "h"), ("d", 86400, "d")];

    for &(_label, ttl, window_suffix) in windows {
        let key = format!(
            "v1:fc:{{u:{}}}:c:{}:{}",
            event.user_id, event.campaign_id, window_suffix
        );
        use fred::interfaces::LuaInterface;
        if let Err(e) = pool
            .eval::<i64, _, _, _>(SCRIPT, vec![key], vec![ttl.to_string()])
            .await
        {
            metrics::counter!("bidder.freq_cap.recorder.write_error").increment(1);
            tracing::debug!(error = %e, "freq cap EVAL failed");
        }
    }
}

fn fc_key(user_id: &str, dim: &str, dim_val: u32, window: CapWindow) -> String {
    format!("v1:fc:{{u:{user_id}}}:{dim}:{dim_val}:{}", window.suffix())
}

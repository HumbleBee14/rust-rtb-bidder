use async_trait::async_trait;
use bidder_core::{
    frequency::{CapResult, CapWindow, FreqCapOutcome, FrequencyCapper},
    model::candidate::AdCandidate,
};
use fred::{clients::Pool as RedisPool, interfaces::KeysInterface};
use std::time::Duration;
use tokio::time::timeout;
use tracing::warn;

/// Redis-backed frequency capper.
///
/// Per request: builds one MGET containing all cap keys for the user across
/// all candidates (campaign-day, campaign-hour, creative-day). If the MGET
/// exceeds `timeout_ms`, returns `SkippedTimeout` — bid proceeds uncapped.
///
/// Key shape: `v1:fc:{u:<userId>}:<dim>:<dimVal>:<window>` — see REDIS-KEYS.md.
pub struct RedisFrequencyCapper {
    pool: RedisPool,
    /// MGET timeout. Config: latency_budget.frequency_cap_ms.
    timeout_ms: u64,
}

impl RedisFrequencyCapper {
    pub fn new(pool: RedisPool, timeout_ms: u64) -> Self {
        Self { pool, timeout_ms }
    }
}

#[async_trait]
impl FrequencyCapper for RedisFrequencyCapper {
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

        // Build the MGET key list: campaign-day + campaign-hour per candidate.
        // Creative-day is omitted in Phase 4 for simplicity; Phase 5 can add it.
        let mut keys: Vec<String> = Vec::with_capacity(candidates.len() * 2);
        for c in candidates {
            keys.push(fc_key(user_id, "c", c.campaign_id, CapWindow::Day));
            keys.push(fc_key(user_id, "c", c.campaign_id, CapWindow::Hour));
        }

        let mget_result = timeout(
            Duration::from_millis(self.timeout_ms),
            self.pool.mget::<Vec<Option<i64>>, _>(keys),
        )
        .await;

        match mget_result {
            Err(_elapsed) => {
                metrics::counter!("bidder.freq_cap.skipped", "reason" => "timeout").increment(1);
                FreqCapOutcome::SkippedTimeout
            }
            Ok(Err(e)) => {
                warn!(error = %e, "freq cap MGET failed — skipping");
                metrics::counter!("bidder.freq_cap.skipped", "reason" => "redis_error")
                    .increment(1);
                FreqCapOutcome::SkippedTimeout
            }
            Ok(Ok(values)) => {
                // values is aligned 2:1 with candidates: [day, hour, day, hour, ...]
                let results = candidates
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let day_count = values.get(i * 2).copied().flatten().unwrap_or(0);
                        let hour_count = values.get(i * 2 + 1).copied().flatten().unwrap_or(0);
                        // Phase 4: hard-coded per-campaign daily cap of 10 and hourly cap of 3.
                        // Phase 5 will load per-campaign cap limits from the catalog.
                        let capped = day_count >= 10 || hour_count >= 3;
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
///
/// Spawned at startup. Each worker loops over the channel; N workers share
/// the load. N = 2 by default (write path is much lower RPS than reads).
pub fn spawn_impression_workers(
    pool: RedisPool,
    mut rx: tokio::sync::mpsc::Receiver<bidder_core::frequency::ImpressionEvent>,
    num_workers: usize,
) {
    // Fanout: clone the receiver into N workers by wrapping in Arc<Mutex>.
    // Actually mpsc is single-consumer; use a single task that pipelines writes.
    // num_workers pipelines concurrent EVAL calls via tokio::spawn per event.
    let pool = std::sync::Arc::new(pool);
    tokio::spawn(async move {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(num_workers));
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
    // This is atomic: if the key didn't exist, we set the TTL in the same round-trip.
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

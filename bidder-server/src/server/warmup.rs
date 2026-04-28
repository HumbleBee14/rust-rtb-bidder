use anyhow::Context;
use bidder_core::{cache::SegmentCache, catalog::SegmentId, health::HealthState};
use fred::{clients::Pool as RedisPool, interfaces::KeysInterface};
use std::time::Instant;
use tracing::{info, warn};

pub async fn run(
    health: HealthState,
    bind: std::net::SocketAddr,
    redis: RedisPool,
    segment_cache: SegmentCache,
) -> anyhow::Result<()> {
    let start = Instant::now();

    // Step 1-4: catalog load, pool connections, and catalog bitmap touch happen
    // in main() before this runs — catalog::start() blocks until the first build
    // completes and all pool connections are primed.
    info!("warmup: data plane ready (catalog loaded, pools connected)");

    // Step 3: segment cache pre-population.
    // Read the hot-user warm-set (v1:warm:users — rkyv-archived Vec<u32>),
    // then MGET v1:seg:{u:<id>} in batches and populate moka.
    // Failure is non-fatal: the pod still starts, just with a cold moka cache.
    if let Err(e) = warm_segment_cache(&redis, &segment_cache).await {
        warn!(error = %e, "segment cache pre-population failed — continuing with cold cache");
    }

    // Step 5: self-test — 100 synthetic bid requests through the local endpoint.
    self_test(bind).await.context("warmup self-test failed")?;

    let elapsed = start.elapsed();
    metrics::gauge!("bidder.warmup.duration_seconds").set(elapsed.as_secs_f64());
    info!(elapsed_ms = elapsed.as_millis(), "warmup complete");

    health.set_ready();
    Ok(())
}

/// Reads `v1:warm:users` (rkyv-archived `Vec<u32>`), fetches each user's segment
/// payload from `v1:seg:{u:<id>}` in batches of 200, and inserts decoded IDs into
/// the moka segment cache.
async fn warm_segment_cache(redis: &RedisPool, cache: &SegmentCache) -> anyhow::Result<()> {
    let warm_bytes: Option<bytes::Bytes> = redis
        .get("v1:warm:users")
        .await
        .context("GET v1:warm:users failed")?;

    let Some(warm_bytes) = warm_bytes else {
        info!("warmup: v1:warm:users not found — skipping segment pre-population");
        return Ok(());
    };

    // Zero-copy decode of the rkyv-archived Vec<u32>.
    let archived = rkyv::access::<rkyv::Archived<Vec<u32>>, rkyv::rancor::Error>(&warm_bytes)
        .context("failed to decode v1:warm:users (rkyv)")?;
    let user_ids: Vec<u32> = rkyv::deserialize::<Vec<u32>, rkyv::rancor::Error>(archived)
        .context("failed to deserialize v1:warm:users")?;

    let total = user_ids.len();
    info!(total, "warmup: pre-populating segment cache");

    const BATCH: usize = 200;
    let mut populated: usize = 0;

    for chunk in user_ids.chunks(BATCH) {
        let keys: Vec<String> = chunk
            .iter()
            .map(|id| format!("v1:seg:{{u:{id}}}"))
            .collect();

        let values: Vec<Option<bytes::Bytes>> = redis
            .mget(keys.clone())
            .await
            .context("MGET v1:seg batch failed")?;

        for (key, maybe_bytes) in keys.iter().zip(values) {
            let Some(raw) = maybe_bytes else { continue };
            if raw.len() % 4 != 0 {
                warn!(
                    key,
                    len = raw.len(),
                    "segment value not multiple of 4 — skipping"
                );
                continue;
            }
            let ids: Vec<SegmentId> = raw
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();

            // Extract user_id from key "v1:seg:{u:<id>}" for the cache key.
            let user_id = key.trim_start_matches("v1:seg:{u:").trim_end_matches('}');

            // Non-fatal on individual insert errors.
            if let Err(e) = cache.get_or_fetch(user_id, || async { Ok(ids) }).await {
                warn!(error = %e, user_id, "failed to insert into segment cache during warmup");
            } else {
                populated += 1;
            }
        }
    }

    metrics::gauge!("bidder.warmup.segment_cache_populated").set(populated as f64);
    info!(
        populated,
        total, "warmup: segment cache pre-population complete"
    );
    Ok(())
}

async fn self_test(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://{}/rtb/openrtb/bid", addr);
    let body = r#"{"id":"warmup","imp":[]}"#;

    // Retry with backoff until the accept loop is ready.
    let mut connected = false;
    for delay_ms in [10u64, 20, 40, 80] {
        match client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(_) => {
                connected = true;
                break;
            }
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
    }
    if !connected {
        anyhow::bail!("self-test: server not reachable after backoff retries");
    }

    let mut failures = 0u32;
    for _ in 0..100 {
        match client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {}
            Ok(r) => {
                warn!(
                    status = r.status().as_u16(),
                    "warmup self-test unexpected status"
                );
                failures += 1;
            }
            Err(e) => {
                warn!(error = %e, "warmup self-test request failed");
                failures += 1;
            }
        }
    }

    if failures > 10 {
        anyhow::bail!("self-test: {} / 100 requests failed", failures);
    }

    info!(failures, "warmup self-test done");
    Ok(())
}

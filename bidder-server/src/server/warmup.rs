use anyhow::Context;
use bidder_core::health::HealthState;
use std::time::Instant;
use tracing::{info, warn};

pub async fn run(health: HealthState, bind: std::net::SocketAddr) -> anyhow::Result<()> {
    let start = Instant::now();

    // Step 1: catalog load — no-op in Phase 1; Phase 3 populates.
    info!("warmup: catalog load (skipped in phase 1)");

    // Step 2: connection priming — no-op in Phase 1; Phase 3 wires Redis/Postgres.
    info!("warmup: connection priming (skipped in phase 1)");

    // Step 3: hot-cache pre-population — no-op in Phase 1; Phase 3 wires moka.
    info!("warmup: hot-cache pre-population (skipped in phase 1)");

    // Step 4: memory pre-touch — no-op in Phase 1; Phase 3 walks catalog structures.
    info!("warmup: memory pre-touch (skipped in phase 1)");

    // Step 5: self-test — send 100 synthetic bid requests through the local endpoint.
    self_test(bind).await.context("warmup self-test failed")?;

    let elapsed = start.elapsed();
    metrics::gauge!("bidder.warmup.duration_seconds").set(elapsed.as_secs_f64());
    info!(elapsed_ms = elapsed.as_millis(), "warmup complete");

    health.set_ready();
    Ok(())
}

async fn self_test(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://{}/rtb/openrtb/bid", addr);
    // Minimal valid-enough body for Phase 1 — just needs to reach the handler.
    let body = r#"{"id":"warmup","imp":[]}"#;
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

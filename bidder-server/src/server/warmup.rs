use anyhow::Context;
use bidder_core::health::HealthState;
use std::time::Instant;
use tracing::{info, warn};

pub async fn run(health: HealthState, bind: std::net::SocketAddr) -> anyhow::Result<()> {
    let start = Instant::now();

    // Steps 1-4 are handled before run() is called: catalog load happens in
    // main() before server startup (catalog::start() blocks until first build
    // completes), Postgres/Redis pools prime their connections during connect(),
    // moka cache is cold and warms under real traffic, and the catalog bitmaps
    // are touched by the build() call itself. By the time we reach here the
    // data plane is ready; only the self-test remains.
    info!("warmup: data plane ready (catalog loaded, pools connected)");

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
    let body = r#"{"id":"warmup","imp":[]}"#;

    // Wait for the accept loop to be ready before counting failures.
    // Retry with backoff instead of a fixed sleep so slow CI doesn't cause false failures.
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

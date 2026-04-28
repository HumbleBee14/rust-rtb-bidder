use anyhow::Context;
use bidder_core::{config::Config, health::HealthState};
use clap::Parser;
use tracing::info;

mod server;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser, Debug)]
#[command(version, about = "RTB bidder server")]
struct Args {
    /// Override the config file path.
    #[arg(long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _args = Args::parse();

    let cfg = Config::load().context("failed to load config")?;

    let _telemetry =
        bidder_core::telemetry::init(&cfg.telemetry).context("failed to init telemetry")?;

    bidder_core::metrics::init(&cfg.metrics).context("failed to init metrics")?;

    info!(
        bind = %cfg.server.bind,
        service = %cfg.telemetry.service_name,
        "starting"
    );

    let health = HealthState::new();
    let listener =
        server::socket::build_listener(cfg.server.bind).context("failed to bind listener")?;
    let local_addr = listener.local_addr()?;
    let router = server::routes::build(&cfg, health.clone());

    // Spawn the server first so the warmup self-test has something to hit.
    let server_handle = {
        let shutdown = shutdown_signal();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await
                .expect("server error")
        })
    };

    if cfg.server.warmup_enabled {
        server::warmup::run(health, local_addr)
            .await
            .context("warmup failed")?;
    } else {
        health.set_ready();
    }

    info!("ready");
    server_handle.await.context("server task panicked")?;
    info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("ctrl-c handler failed");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler failed")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, draining");
}

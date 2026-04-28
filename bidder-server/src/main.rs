use anyhow::Context;
use bidder_core::{
    cache::SegmentCache,
    config::Config,
    frequency::ImpressionRecorder,
    health::HealthState,
    pacing::LocalBudgetPacer,
    pipeline::{
        stages::{
            BudgetPacingStage, CandidateLimitStage, CandidateRetrievalStage, FreqCapStage,
            RankingStage, RequestValidationStage, ResponseBuildStage, ScoringStage,
            UserEnrichmentStage,
        },
        Pipeline,
    },
    scoring::FeatureWeightedScorer,
};
use clap::Parser;
use fred::{
    clients::Pool as RedisPool,
    interfaces::ClientLike,
    types::config::{Config as FredConfig, PerformanceConfig, ReconnectPolicy},
};
use sqlx::postgres::PgPoolOptions;
use std::{sync::Arc, time::Duration};
use tracing::info;

mod freq_cap;
mod segment_repo;
mod server;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser, Debug)]
#[command(version, about = "RTB bidder server")]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let cfg = Config::load_from(&args.config).context("failed to load config")?;

    let _telemetry =
        bidder_core::telemetry::init(&cfg.telemetry).context("failed to init telemetry")?;

    bidder_core::metrics::init(&cfg.metrics).context("failed to init metrics")?;

    info!(
        bind = %cfg.server.bind,
        service = %cfg.telemetry.service_name,
        "starting"
    );

    // Postgres pool.
    let pg_pool = PgPoolOptions::new()
        .max_connections(cfg.postgres.max_connections)
        .min_connections(cfg.postgres.min_connections)
        .acquire_timeout(Duration::from_millis(cfg.postgres.acquire_timeout_ms))
        .idle_timeout(Duration::from_secs(cfg.postgres.idle_timeout_secs))
        .max_lifetime(Duration::from_secs(cfg.postgres.max_lifetime_secs))
        .connect(&cfg.postgres.url)
        .await
        .context("failed to connect to postgres")?;

    // Catalog load (spawns background refresh task).
    let (catalog, _segment_registry) = bidder_core::catalog::start(pg_pool, cfg.catalog.clone())
        .await
        .context("failed to load initial catalog")?;

    // Redis round-robin pool.
    let pool_size = if cfg.redis.pool_size == 0 {
        num_cpus::get()
    } else {
        cfg.redis.pool_size
    };
    let redis_cfg = FredConfig::from_url(&cfg.redis.url).context("invalid redis url")?;
    let redis_pool = RedisPool::new(
        redis_cfg,
        Some(PerformanceConfig::default()),
        None,
        Some(ReconnectPolicy::default()),
        pool_size,
    )
    .context("failed to create redis pool")?;
    redis_pool.connect();
    redis_pool
        .wait_for_connect()
        .await
        .context("failed to connect to redis")?;

    // Segment cache.
    let segment_cache = SegmentCache::new(
        cfg.redis.segment_cache_capacity,
        cfg.redis.segment_cache_ttl_secs,
    );

    // Segment repository.
    let segment_repo = Arc::new(segment_repo::RedisSegmentRepo::new(redis_pool.clone()));

    // Impression recorder: bounded channel + Redis writer workers.
    let (impression_recorder, imp_rx) = ImpressionRecorder::new();
    freq_cap::spawn_impression_workers(redis_pool.clone(), imp_rx, cfg.freq_cap.impression_workers);

    // Frequency capper.
    let freq_capper = Arc::new(freq_cap::RedisFrequencyCapper::new(
        redis_pool.clone(),
        cfg.latency_budget.frequency_cap_ms,
    ));

    // Budget pacer (local in-process for Phase 4; distributed in Phase 5+).
    let budget_pacer = Arc::new(LocalBudgetPacer::new());

    // Scorer.
    let scorer: Arc<dyn bidder_core::scoring::Scorer> = Arc::new(FeatureWeightedScorer::default());

    let health = HealthState::new();

    let pipeline = Pipeline::new(cfg.latency_budget.clone())
        .add_stage(RequestValidationStage)
        .add_stage(UserEnrichmentStage {
            catalog: Arc::clone(&catalog),
            segment_cache: segment_cache.clone(),
            segment_repo,
        })
        .add_stage(CandidateRetrievalStage)
        .add_stage(CandidateLimitStage {
            top_k: cfg.pipeline.max_candidates_per_imp,
        })
        .add_stage(ScoringStage {
            scorer: Arc::clone(&scorer),
        })
        .add_stage(FreqCapStage {
            capper: Arc::clone(&freq_capper) as Arc<dyn bidder_core::frequency::FrequencyCapper>,
        })
        .add_stage(BudgetPacingStage {
            pacer: Arc::clone(&budget_pacer) as Arc<dyn bidder_core::pacing::BudgetPacer>,
        })
        .add_stage(RankingStage)
        .add_stage(ResponseBuildStage);

    let app_state = server::state::AppState::new(
        health.clone(),
        pipeline,
        catalog,
        redis_pool.clone(),
        segment_cache.clone(),
    );

    let listener =
        server::socket::build_listener(cfg.server.bind).context("failed to bind listener")?;
    let local_addr = listener.local_addr()?;
    let router = server::routes::build(&cfg, app_state);

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
        server::warmup::run(
            health,
            local_addr,
            redis_pool.clone(),
            segment_cache.clone(),
        )
        .await
        .context("warmup failed")?;
    } else {
        health.set_ready();
    }

    info!("ready");
    server_handle.await.context("server task panicked")?;
    info!("shutdown complete");

    // Suppress unused-variable warning; impression_recorder is kept alive for the process lifetime.
    drop(impression_recorder);

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

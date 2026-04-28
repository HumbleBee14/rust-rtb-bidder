use anyhow::Context;
use bidder_core::{
    breaker::{BreakerConfig, CircuitBreaker},
    cache::SegmentCache,
    config::Config,
    events::{EventPublisher, NoOpEventPublisher},
    frequency::ImpressionRecorder,
    health::HealthState,
    hedge::RedisHedgeState,
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
mod kafka;
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

    // Budget pacer created before catalog::start — the start function seeds it
    // immediately from the initial catalog, and refresh_loop reseeds on every rebuild.
    let budget_pacer: Arc<dyn bidder_core::pacing::BudgetPacer> = Arc::new(LocalBudgetPacer::new());

    // Catalog load (spawns background refresh task + seeds budget_pacer on each rebuild).
    let (catalog, _segment_registry) =
        bidder_core::catalog::start(pg_pool, cfg.catalog.clone(), Arc::clone(&budget_pacer))
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

    // One circuit breaker shared across all Redis call sites (freq-cap + segment repo).
    // One breaker per logical dependency, not per call site — both callers observe
    // the same Redis cluster and should react to its health as a unit.
    let redis_breaker: Arc<CircuitBreaker> = Arc::new(CircuitBreaker::new(BreakerConfig {
        slow_call_duration: Duration::from_millis(cfg.latency_budget.frequency_cap_ms * 2),
        ..BreakerConfig::redis("redis")
    }));

    // Shared hedge budget for all Redis idempotent reads.
    // Nominal capacity = 10% of max_concurrency (rough proxy for RPS budget).
    let redis_hedge = RedisHedgeState::new((cfg.server.max_concurrency / 10).max(1) as u64);
    let redis_hedge_budget = Arc::clone(&redis_hedge.budget);

    // Segment repository.
    let segment_repo = Arc::new(segment_repo::RedisSegmentRepo::new(
        redis_pool.clone(),
        Arc::clone(&redis_breaker),
        Arc::clone(&redis_hedge_budget),
    ));

    // Impression recorder: bounded channel + Redis writer workers.
    let (impression_recorder, imp_rx) = ImpressionRecorder::new();
    freq_cap::spawn_impression_workers(redis_pool.clone(), imp_rx, cfg.freq_cap.impression_workers);
    let impression_recorder = Arc::new(impression_recorder);

    // Frequency capper.
    let freq_capper: Arc<dyn bidder_core::frequency::FrequencyCapper> =
        Arc::new(freq_cap::RedisFrequencyCapper::new(
            redis_pool.clone(),
            cfg.latency_budget.frequency_cap_ms,
            Arc::clone(&redis_breaker),
            Arc::clone(&redis_hedge_budget),
        ));

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
            capper: Arc::clone(&freq_capper),
        })
        .add_stage(BudgetPacingStage {
            pacer: Arc::clone(&budget_pacer),
        })
        .add_stage(RankingStage {
            pacer: Arc::clone(&budget_pacer),
        })
        .add_stage(ResponseBuildStage);

    // Event publisher: KafkaEventPublisher on successful producer init (rdkafka ClientConfig::create),
    // NoOpEventPublisher on config/init errors. Note: create() does not contact brokers — broker
    // reachability failures surface later as per-message errors, not here.
    let event_publisher: Arc<dyn EventPublisher> = match kafka::KafkaEventPublisher::new(&cfg.kafka)
    {
        Ok(p) => {
            info!(brokers = %cfg.kafka.brokers, "kafka event publisher initialised");
            Arc::new(p)
        }
        Err(e) => {
            tracing::warn!(error = %e, "kafka producer init failed — using no-op publisher");
            Arc::new(NoOpEventPublisher)
        }
    };

    let app_state = server::state::AppState::new(
        health.clone(),
        pipeline,
        catalog,
        redis_pool.clone(),
        segment_cache.clone(),
        Arc::clone(&impression_recorder),
        event_publisher,
        Arc::from(cfg.kafka.events_topic.as_str()),
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

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
mod win_notice;

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

    // Hedge feedback trackers — constructed early so Redis-touching components
    // (segment_repo, freq_cap) can record their call durations into the same
    // tracker that the feedback loop drains.
    let load_shed_tracker = Arc::new(bidder_core::hedge_feedback::LoadShedTracker::new());
    let redis_latency_tracker = Arc::new(bidder_core::hedge_feedback::RedisLatencyTracker::new());

    // Segment repository.
    let segment_repo = Arc::new(segment_repo::RedisSegmentRepo::new(
        redis_pool.clone(),
        Arc::clone(&redis_breaker),
        Arc::clone(&redis_hedge_budget),
        Some(Arc::clone(&redis_latency_tracker)),
    ));

    // Impression recorder: bounded channel + Redis writer workers.
    let (impression_recorder, imp_rx) = ImpressionRecorder::new();
    freq_cap::spawn_impression_workers(redis_pool.clone(), imp_rx, cfg.freq_cap.impression_workers);
    let impression_recorder = Arc::new(impression_recorder);

    // Frequency capper. The Redis-backed impl is the source of truth; the
    // optional in-process wrapper layers a DashMap + write-behind queue on
    // top, addressing the Phase 6.5 baseline finding that the freq-cap MGET
    // is breaker-skipped on 37% of requests at 10K RPS.
    let redis_capper: Arc<dyn bidder_core::frequency::FrequencyCapper> =
        Arc::new(freq_cap::RedisFrequencyCapper::new(
            redis_pool.clone(),
            cfg.latency_budget.frequency_cap_ms,
            Arc::clone(&redis_breaker),
            Arc::clone(&redis_hedge_budget),
            Some(Arc::clone(&redis_latency_tracker)),
        ));

    let freq_capper: Arc<dyn bidder_core::frequency::FrequencyCapper> =
        if cfg.freq_cap.in_process_enabled {
            tracing::info!(
                cap_capacity = cfg.freq_cap.in_process_cap_capacity,
                write_buffer = cfg.freq_cap.in_process_write_buffer_size,
                flush_interval_ms = cfg.freq_cap.in_process_flush_interval_ms,
                "InProcessFrequencyCapper enabled — single-instance authoritative; \
                 verify LB user-stickiness or accept brief multi-instance inconsistency"
            );
            let in_proc_cfg = bidder_core::frequency::InProcessConfig {
                cap_capacity: cfg.freq_cap.in_process_cap_capacity,
                write_buffer_size: cfg.freq_cap.in_process_write_buffer_size,
                flush_interval: Duration::from_millis(cfg.freq_cap.in_process_flush_interval_ms),
            };
            let (wrapper, write_rx) = bidder_core::frequency::InProcessFrequencyCapper::new(
                Arc::clone(&redis_capper),
                Arc::clone(&redis_breaker),
                in_proc_cfg,
            );
            // Drain the write-behind queue into the existing ImpressionRecorder
            // pipeline, which already handles Redis EVAL with TTL. This keeps
            // one path for Redis writes — InProcessFrequencyCapper is purely a
            // read-side cache + write enqueue.
            let recorder_for_drain = Arc::clone(&impression_recorder);
            tokio::spawn(in_process_write_behind_drain(write_rx, recorder_for_drain));
            Arc::new(wrapper)
        } else {
            Arc::clone(&redis_capper)
        };

    // Scorer — built recursively from [scoring] config.
    let scorer: Arc<dyn bidder_core::scoring::Scorer> = build_scorer_root(&cfg.scoring)
        .context("failed to construct scorer from [scoring] config")?;

    // Win-notice gate (HMAC verify + Redis SET-NX dedup) and the matching nurl
    // builder embedded in each bid response. Built before the pipeline so the
    // ResponseBuildStage can reference the builder.
    if cfg.win_notice.require_auth && cfg.win_notice.secret.is_empty() {
        tracing::warn!(
            "win_notice.require_auth=true but secret is empty — HMAC verification is effectively DISABLED. \
             Set BIDDER__WIN_NOTICE__SECRET in production."
        );
    }
    let win_notice_gate = Arc::new(win_notice::WinNoticeGateService::new(
        redis_pool.clone(),
        cfg.win_notice.clone(),
    ));
    let notice_url_builder: Arc<dyn bidder_core::notice::NoticeUrlBuilder> =
        Arc::clone(&win_notice_gate) as _;

    // Exchange adapter — translates wire bytes ↔ internal BidRequest/BidResponse.
    // Phase 7 default is OpenRtbGeneric; multi-exchange routes (GoogleAdx,
    // Magnite, etc.) ship as additional `with_state(...)` wirings on the
    // router as their adapter impls land. Built before the pipeline so its
    // id() can be threaded into ResponseBuildStage for per-SSP HMAC selection.
    let adapter: Arc<dyn bidder_core::exchange::ExchangeAdapter> =
        Arc::new(bidder_core::exchange::OpenRtbGenericAdapter);

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
        .add_stage(ResponseBuildStage {
            notice_url_builder: Arc::clone(&notice_url_builder),
            exchange_id: Arc::from(adapter.id()),
        });

    // Event publisher: KafkaEventPublisher on successful producer init (rdkafka ClientConfig::create),
    // NoOpEventPublisher on config/init errors. Note: create() does not contact brokers — broker
    // reachability failures surface later as per-message errors, not here.
    // Kafka drop-policy state machine. The publisher reads `effective_policy()`
    // on every send; the monitor flips it to RandomSample when the rolling
    // drop rate sustains above 1% for 5 min, and reverts when it normalises.
    let kafka_incident = Arc::new(bidder_core::kafka_incident::KafkaIncidentState::new(
        cfg.kafka.drop_policy.clone(),
    ));
    bidder_core::kafka_incident::spawn_monitor(
        Arc::clone(&kafka_incident),
        Duration::from_secs(30),
        10,
        0.01,
    );

    let event_publisher: Arc<dyn EventPublisher> =
        match kafka::KafkaEventPublisher::new(&cfg.kafka, Arc::clone(&kafka_incident)) {
            Ok(p) => {
                info!(brokers = %cfg.kafka.brokers, "kafka event publisher initialised");
                Arc::new(p)
            }
            Err(e) => {
                tracing::warn!(error = %e, "kafka producer init failed — using no-op publisher");
                Arc::new(NoOpEventPublisher)
            }
        };

    // Hedge feedback loop: drains trackers every 1s, pushes load_shed_rate and
    // redis_p95 into RedisHedgeState. Closes the Phase 5 gap where
    // HedgeBudget::set_load_shed_rate() existed but wasn't called.
    bidder_core::hedge_feedback::spawn_feedback_loop(
        Arc::new(redis_hedge),
        Arc::clone(&load_shed_tracker),
        Arc::clone(&redis_latency_tracker),
        Duration::from_secs(1),
    );

    let app_state = server::state::AppState::new(
        health.clone(),
        pipeline,
        catalog,
        redis_pool.clone(),
        segment_cache.clone(),
        Arc::clone(&impression_recorder),
        event_publisher,
        Arc::from(cfg.kafka.events_topic.as_str()),
        win_notice_gate,
        adapter,
        Arc::clone(&load_shed_tracker),
        Arc::clone(&redis_latency_tracker),
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

/// Build the scorer tree from `[scoring]` config.
///
/// Nesting rules:
///   - Root may be any of `feature_weighted`, `ml`, `cascade`, `ab_test`.
///   - `cascade.stage1`/`stage2` may be a leaf (`feature_weighted` or `ml`).
///     Nested cascade or ab_test inside cascade is rejected — keeps the
///     stage-1/stage-2 shape predictable.
///   - `ab_test.control`/`treatment` may be a leaf OR a cascade. This is the
///     real production rollout shape: 10% of users to a cascade, 90% to the
///     baseline scorer. Nested ab_test inside ab_test is rejected (no
///     concurrent experiment chains via config — those go via the
///     experiment platform, not here).
///   - Maximum effective depth is 2 (ab_test → cascade → leaf). The recursion
///     is bounded by the type of decorator at each level; unbounded recursion
///     is impossible.
fn build_scorer_root(
    cfg: &bidder_core::config::ScoringConfig,
) -> anyhow::Result<Arc<dyn bidder_core::scoring::Scorer>> {
    use bidder_core::config::ScoringKind;
    match cfg.kind {
        ScoringKind::FeatureWeighted => Ok(Arc::new(FeatureWeightedScorer::default())),
        ScoringKind::Ml => Ok(build_ml(cfg)?),
        ScoringKind::Cascade => build_cascade(cfg),
        ScoringKind::AbTest => build_ab_test(cfg),
    }
}

fn build_cascade(
    cfg: &bidder_core::config::ScoringConfig,
) -> anyhow::Result<Arc<dyn bidder_core::scoring::Scorer>> {
    use bidder_core::scoring::CascadeScorer;
    let cascade_cfg = cfg
        .cascade
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("[scoring] kind=cascade but [scoring.cascade] missing"))?;
    let stage1 = build_leaf(cfg, &cascade_cfg.stage1, "cascade.stage1")?;
    let stage2 = build_leaf(cfg, &cascade_cfg.stage2, "cascade.stage2")?;
    Ok(Arc::new(CascadeScorer {
        stage1,
        stage2,
        top_k: cascade_cfg.top_k,
        threshold: cascade_cfg.threshold,
    }))
}

fn build_ab_test(
    cfg: &bidder_core::config::ScoringConfig,
) -> anyhow::Result<Arc<dyn bidder_core::scoring::Scorer>> {
    use bidder_core::scoring::ABTestScorer;
    let ab_cfg = cfg
        .ab_test
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("[scoring] kind=ab_test but [scoring.ab_test] missing"))?;
    let control = build_leaf_or_cascade(cfg, &ab_cfg.control, "ab_test.control")?;
    let treatment = build_leaf_or_cascade(cfg, &ab_cfg.treatment, "ab_test.treatment")?;
    Ok(Arc::new(ABTestScorer {
        control,
        treatment,
        treatment_share: ab_cfg.treatment_share,
        hash_seed: ab_cfg.hash_seed.clone(),
    }))
}

/// Build a leaf scorer (used inside cascade). Rejects nested decorators —
/// cascade is a stage-1/stage-2 shape, not a recursive tree.
fn build_leaf(
    cfg: &bidder_core::config::ScoringConfig,
    kind: &bidder_core::config::ScoringKind,
    where_in_config: &str,
) -> anyhow::Result<Arc<dyn bidder_core::scoring::Scorer>> {
    use bidder_core::config::ScoringKind;
    match kind {
        ScoringKind::FeatureWeighted => Ok(Arc::new(FeatureWeightedScorer::default())),
        ScoringKind::Ml => Ok(build_ml(cfg)?),
        ScoringKind::Cascade | ScoringKind::AbTest => Err(anyhow::anyhow!(
            "[scoring.{where_in_config}] cannot be cascade or ab_test — \
             only leaf scorers (feature_weighted, ml) are allowed inside cascade"
        )),
    }
}

/// Build a leaf OR a cascade scorer (used inside ab_test). Rejects nested
/// ab_test — concurrent experiment chains go via the experiment platform, not
/// via stacked config.
fn build_leaf_or_cascade(
    cfg: &bidder_core::config::ScoringConfig,
    kind: &bidder_core::config::ScoringKind,
    where_in_config: &str,
) -> anyhow::Result<Arc<dyn bidder_core::scoring::Scorer>> {
    use bidder_core::config::ScoringKind;
    match kind {
        ScoringKind::FeatureWeighted => Ok(Arc::new(FeatureWeightedScorer::default())),
        ScoringKind::Ml => Ok(build_ml(cfg)?),
        ScoringKind::Cascade => build_cascade(cfg),
        ScoringKind::AbTest => Err(anyhow::anyhow!(
            "[scoring.{where_in_config}] cannot be ab_test — \
             ab_test cannot nest ab_test (concurrent experiments go via the experiment platform)"
        )),
    }
}

fn build_ml(
    cfg: &bidder_core::config::ScoringConfig,
) -> anyhow::Result<Arc<dyn bidder_core::scoring::Scorer>> {
    use bidder_core::scoring::{MLScorer, MLScorerConfig};
    let ml = cfg.ml.as_ref().ok_or_else(|| {
        anyhow::anyhow!("[scoring] kind=ml or referenced by decorator but [scoring.ml] missing")
    })?;
    // CONTRACT: docs/SCORING-FEATURES.md § 8 — production always supplies
    // FeatureWeightedScorer as the inference-failure fallback so a degraded ML
    // path never emits zero-scored candidates.
    let fallback: Arc<dyn bidder_core::scoring::Scorer> =
        Arc::new(FeatureWeightedScorer::default());
    let scorer = MLScorer::new_with_fallback(
        MLScorerConfig {
            model_path: ml.model_path.clone().into(),
            parity_path: ml.parity_path.clone().map(Into::into),
            input_tensor_name: ml.input_tensor_name.clone(),
            output_tensor_name: ml.output_tensor_name.clone(),
            pool_size: ml.pool_size,
            max_batch: ml.max_batch,
            batch_pad_to: ml.batch_pad_to,
        },
        Some(fallback),
    )?;
    Ok(Arc::new(scorer))
}

/// Drain the InProcessFrequencyCapper write-behind channel into the existing
/// `ImpressionRecorder` pipeline. The recorder already handles Redis `EVAL`
/// with atomic INCR + EXPIRE, so this drain is intentionally thin — it just
/// translates `WriteBehindOp` (which knows about windows) into the legacy
/// `ImpressionEvent` shape (which encodes one record per impression and lets
/// the recorder write both day + hour keys).
///
/// One drain task per process; runs until the channel sender is dropped
/// (i.e. until the InProcessFrequencyCapper is dropped, which only happens
/// at process exit). On send failure (recorder channel full), increments
/// `bidder.freq_cap.in_process.recorder_full_total` and discards — the
/// next read-fallthrough recovers the count from Redis. Same drop policy
/// the recorder uses for its own bid-path callers.
async fn in_process_write_behind_drain(
    mut rx: tokio::sync::mpsc::Receiver<bidder_core::frequency::WriteBehindOp>,
    recorder: Arc<bidder_core::frequency::ImpressionRecorder>,
) {
    while let Some(op) = rx.recv().await {
        // The recorder's per-event write covers BOTH day + hour windows
        // for a single (user, campaign) pair, so we de-duplicate at the
        // drain by only forwarding the Day op (Hour is implicit in the
        // recorder's EVAL script). This avoids double-incrementing.
        if op.window != bidder_core::frequency::CapWindow::Day {
            continue;
        }
        let event = bidder_core::frequency::ImpressionEvent {
            user_id: op.user_id,
            campaign_id: op.campaign_id,
            // creative_id and device_type are not tracked by the in-process
            // capper today (it caps at campaign-level only). The recorder
            // accepts these fields for the legacy creative + device caps;
            // we pass zeros and rely on the recorder's existing behaviour
            // (creative_id=0 / device_type=0 are the catalog's "any" sentinels).
            creative_id: 0,
            device_type: 0,
            hour_of_day: 0,
        };
        // try_record is non-blocking. When it drops (recorder channel full,
        // typically because Redis is slow), the in-process cache holds an
        // increment that Redis does NOT — i.e. the local capper drifts ahead
        // of the cluster source-of-truth. Surface that as a distinct signal
        // so an operator can see the desync rate during a Redis incident
        // without it getting buried in the generic recorder.dropped counter
        // (which also fires for bid-path overflows).
        if !recorder.try_record(event) {
            metrics::counter!("bidder.freq_cap.in_process.redis_desync_total").increment(1);
        }
    }
}

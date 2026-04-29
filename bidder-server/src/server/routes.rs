use crate::server::{handlers, layers::metrics::MetricsLayer, state::AppState};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use bidder_core::{config::Config, hedge_feedback::LoadShedTracker};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

/// Per-route timeout + LoadShedTracker increments. The tracker is fed by
/// every accepted request and every shed/timeout response, so the hedge
/// feedback loop can derive load_shed_rate over a rolling window.
#[derive(Clone)]
struct TimeoutConfig {
    duration: Duration,
    tracker: Arc<LoadShedTracker>,
}

async fn timeout_middleware(
    State(cfg): State<TimeoutConfig>,
    req: Request,
    next: Next,
) -> Response {
    cfg.tracker.record_request();
    match tokio::time::timeout(cfg.duration, next.run(req)).await {
        Ok(resp) => {
            // 503 from a downstream layer (e.g. ConcurrencyLimitLayer rejection)
            // also counts as a load-shed event for hedge feedback purposes.
            if resp.status() == StatusCode::SERVICE_UNAVAILABLE {
                cfg.tracker.record_shed();
            }
            resp
        }
        Err(_) => {
            metrics::counter!("bidder.http.timeout_total").increment(1);
            cfg.tracker.record_shed();
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

pub fn build(cfg: &Config, app_state: AppState) -> Router {
    let bid_timeout = Duration::from_millis(cfg.latency_budget.http_timeout_ms);
    let win_timeout = Duration::from_millis(cfg.latency_budget.win_timeout_ms);
    let tracker = Arc::clone(&app_state.load_shed_tracker);

    // Health routes use HealthState directly.
    let health_router = Router::new()
        .route("/health/live", get(handlers::liveness))
        .route("/health/ready", get(handlers::readiness))
        .with_state(app_state.health.clone());

    // Bid path: concurrency-limited + tight 50 ms timeout.
    let bid_router = Router::new()
        .route("/rtb/openrtb/bid", post(handlers::bid))
        .with_state(app_state.clone())
        .layer(
            ServiceBuilder::new()
                .layer(tower::limit::ConcurrencyLimitLayer::new(
                    cfg.server.max_concurrency,
                ))
                .layer(TraceLayer::new_for_http()),
        )
        .layer(middleware::from_fn_with_state(
            TimeoutConfig {
                duration: bid_timeout,
                tracker: Arc::clone(&tracker),
            },
            timeout_middleware,
        ));

    // Win-notice path: bounded concurrency (replay-storm protection), longer
    // timeout to tolerate brief Kafka producer backpressure.
    //
    // The win handler does HMAC verify + Redis SET-NX dedup before any cheap
    // path. A misconfigured SSP or a replay attacker can fire identical win
    // notices at us; without an HTTP-level shedder they all reach Redis,
    // saturating its CPU on dedup work. Cap concurrent win handlers at 10%
    // of max_concurrency — matches the "nominal capacity" heuristic the
    // hedge budget uses, and is several × above legitimate win-notice
    // volume (which is bounded by win-rate × bid RPS).
    let win_concurrency = (cfg.server.max_concurrency / 10).max(1);
    let win_router = Router::new()
        .route("/rtb/win", get(handlers::win))
        .with_state(app_state)
        .layer(
            ServiceBuilder::new()
                .layer(tower::limit::ConcurrencyLimitLayer::new(win_concurrency))
                .layer(TraceLayer::new_for_http()),
        )
        .layer(middleware::from_fn_with_state(
            TimeoutConfig {
                duration: win_timeout,
                tracker,
            },
            timeout_middleware,
        ));

    // MetricsLayer wraps everything so timed-out requests are still counted.
    Router::new()
        .merge(health_router)
        .merge(bid_router)
        .merge(win_router)
        .layer(MetricsLayer)
}

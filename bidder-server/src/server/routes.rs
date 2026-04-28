use crate::server::{handlers, layers::metrics::MetricsLayer, state::AppState};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use bidder_core::config::Config;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
struct TimeoutConfig(Duration);

async fn timeout_middleware(
    State(TimeoutConfig(duration)): State<TimeoutConfig>,
    req: Request,
    next: Next,
) -> Response {
    match tokio::time::timeout(duration, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => {
            metrics::counter!("bidder.http.timeout_total").increment(1);
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

pub fn build(cfg: &Config, app_state: AppState) -> Router {
    let bid_timeout = Duration::from_millis(cfg.latency_budget.http_timeout_ms);
    let win_timeout = Duration::from_millis(cfg.latency_budget.win_timeout_ms);

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
            TimeoutConfig(bid_timeout),
            timeout_middleware,
        ));

    // Win-notice path: no concurrency limit (low RPS, not on critical bid path),
    // longer timeout to tolerate brief Kafka producer backpressure.
    let win_router = Router::new()
        .route("/rtb/win", get(handlers::win))
        .with_state(app_state)
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            TimeoutConfig(win_timeout),
            timeout_middleware,
        ));

    // MetricsLayer wraps everything so timed-out requests are still counted.
    Router::new()
        .merge(health_router)
        .merge(bid_router)
        .merge(win_router)
        .layer(MetricsLayer)
}

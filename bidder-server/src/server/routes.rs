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
    let timeout = Duration::from_millis(cfg.latency_budget.http_timeout_ms);

    // Health routes use HealthState directly; bid route uses AppState.
    let health_router = Router::new()
        .route("/health/live", get(handlers::liveness))
        .route("/health/ready", get(handlers::readiness))
        .with_state(app_state.health.clone());

    let bid_router = Router::new()
        .route("/rtb/openrtb/bid", post(handlers::bid))
        .route("/rtb/win", get(handlers::win))
        .with_state(app_state);

    // MetricsLayer is outermost so timed-out requests are still measured.
    // Order (outermost → innermost): Metrics → Timeout → ConcurrencyLimit → Trace → Handler
    let inner = ServiceBuilder::new()
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            cfg.server.max_concurrency,
        ))
        .layer(TraceLayer::new_for_http());

    Router::new()
        .merge(health_router)
        .merge(bid_router)
        .layer(inner)
        .layer(middleware::from_fn_with_state(
            TimeoutConfig(timeout),
            timeout_middleware,
        ))
        .layer(MetricsLayer)
}

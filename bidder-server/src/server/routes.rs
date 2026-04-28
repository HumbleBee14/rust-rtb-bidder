use crate::server::{handlers, layers::metrics::MetricsLayer};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use bidder_core::{config::Config, health::HealthState};
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

pub fn build(cfg: &Config, health: HealthState) -> Router {
    let timeout = Duration::from_millis(cfg.latency_budget.http_timeout_ms);

    // MetricsLayer is outermost so timed-out requests are still measured.
    // Order (outermost → innermost): Metrics → Timeout → ConcurrencyLimit → Trace → Handler
    let inner = ServiceBuilder::new()
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            cfg.server.max_concurrency,
        ))
        .layer(TraceLayer::new_for_http());

    Router::new()
        .route("/health/live", get(handlers::liveness))
        .route("/health/ready", get(handlers::readiness))
        .route("/rtb/openrtb/bid", post(handlers::bid))
        .layer(inner)
        .layer(middleware::from_fn_with_state(
            TimeoutConfig(timeout),
            timeout_middleware,
        ))
        .layer(MetricsLayer)
        .with_state(health)
}

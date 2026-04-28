use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bidder_core::health::HealthState;

pub async fn liveness() -> StatusCode {
    StatusCode::OK
}

pub async fn readiness(State(health): State<HealthState>) -> Response {
    if health.is_ready() {
        StatusCode::OK.into_response()
    } else {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    }
}

// Phase 1: hardcoded 204 No Bid. Real pipeline wired in Phase 2+.
pub async fn bid() -> StatusCode {
    metrics::counter!("bidder.bid.requests_total").increment(1);
    StatusCode::NO_CONTENT
}

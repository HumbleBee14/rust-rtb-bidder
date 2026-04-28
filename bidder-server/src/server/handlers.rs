use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bidder_core::{
    health::HealthState,
    model::{BidContext, PipelineOutcome},
};
use tracing::instrument;

use crate::server::state::AppState;

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

#[instrument(skip(state, body))]
pub async fn bid(State(state): State<AppState>, body: Bytes) -> Response {
    // simd-json requires &mut [u8] — body must be an owned mutable Vec, never Bytes.
    // We copy the Bytes into a Vec here; jemalloc absorbs this allocation.
    let mut buf: Vec<u8> = body.into();

    let request = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            metrics::counter!("bidder.bid.requests_total", "result" => "parse_error").increment(1);
            tracing::debug!(error = %e, "bid request parse failed");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let mut ctx = BidContext::new(request);

    if let Err(e) = state.pipeline.execute(&mut ctx).await {
        metrics::counter!("bidder.bid.requests_total", "result" => "pipeline_error").increment(1);
        tracing::error!(error = %e, "pipeline execution error");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    match ctx.outcome {
        PipelineOutcome::NoBid(_) => {
            metrics::counter!("bidder.bid.requests_total", "result" => "no_bid").increment(1);
            StatusCode::NO_CONTENT.into_response()
        }
        PipelineOutcome::Pending => {
            unreachable!("ResponseBuildStage always resolves Pending before pipeline returns")
        }
    }
}

use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bidder_core::{
    frequency::ImpressionEvent,
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
    // simd-json requires &mut [u8] — copy Bytes into an owned Vec.
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
        PipelineOutcome::Bid => {
            metrics::counter!("bidder.bid.requests_total", "result" => "bid").increment(1);

            // Enqueue impression events for every winner so freq-cap counters
            // are incremented asynchronously. Phase 5 replaces this with a
            // /rtb/win notice endpoint so only actual SSP wins are counted.
            let device_type = ctx
                .request
                .device
                .as_ref()
                .and_then(|d| d.devicetype)
                .map(|dt| dt.0)
                .unwrap_or(0);
            let hour_of_day = bidder_core::clock::current_hour_of_day();
            for winner in &ctx.winners {
                state.impression_recorder.try_record(ImpressionEvent {
                    user_id: ctx
                        .request
                        .user
                        .as_ref()
                        .and_then(|u| u.id.as_deref())
                        .unwrap_or("")
                        .to_string(),
                    campaign_id: winner.campaign_id,
                    creative_id: winner.creative_id,
                    device_type,
                    hour_of_day,
                });
            }

            match ctx.bid_response {
                Some(resp) => match serde_json::to_vec(&resp) {
                    Ok(body) => (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                        .into_response(),
                    Err(e) => {
                        tracing::error!(error = %e, "failed to serialize bid response");
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    }
                },
                None => {
                    tracing::error!("PipelineOutcome::Bid but bid_response is None");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        PipelineOutcome::Pending => {
            unreachable!("ResponseBuildStage always resolves Pending before pipeline returns")
        }
    }
}

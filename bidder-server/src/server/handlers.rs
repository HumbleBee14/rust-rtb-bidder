use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bidder_core::{
    clock::current_hour_of_day,
    frequency::ImpressionEvent,
    health::HealthState,
    model::{BidContext, PipelineOutcome},
};
use bidder_protos::events::{ad_event, AdEvent, BidEvent, WinEvent};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::instrument;

use crate::server::state::AppState;

/// Win notice query parameters from the SSP.
#[derive(Debug, serde::Deserialize)]
pub struct WinParams {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: u32,
    pub creative_id: u32,
    /// Clearing price in microdollars.
    pub clearing_price_micros: i64,
    pub user_id: Option<String>,
    /// HMAC-SHA256 hex over `request_id|imp_id|campaign_id|creative_id|clearing_price_micros`.
    /// Required when `[win_notice] require_auth = true` and a secret is configured.
    pub token: Option<String>,
}

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

            let device_type = ctx
                .request
                .device
                .as_ref()
                .and_then(|d| d.devicetype)
                .map(|dt| dt.0)
                .unwrap_or(0);
            let user_id = ctx
                .request
                .user
                .as_ref()
                .and_then(|u| u.id.as_deref())
                .unwrap_or("")
                .to_string();
            let request_id = ctx.request.id.clone();
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            // Serialize first — only publish events if the response is valid.
            let response_body = match ctx.bid_response {
                Some(resp) => match serde_json::to_vec(&resp) {
                    Ok(body) => body,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to serialize bid response");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                },
                None => {
                    tracing::error!("PipelineOutcome::Bid but bid_response is None");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };

            // Publish BidEvent per winner — fire-and-forget, never blocks bid path.
            let topic = state.events_topic.clone();
            for winner in &ctx.winners {
                let event = AdEvent {
                    body: Some(ad_event::Body::Bid(BidEvent {
                        request_id: request_id.clone(),
                        imp_id: winner.imp_id.clone(),
                        campaign_id: winner.campaign_id,
                        creative_id: winner.creative_id,
                        bid_price_micros: winner.bid_price_cents as i64 * 10_000,
                        timestamp_ms,
                        user_id: user_id.clone(),
                        device_type: device_type.into(),
                    })),
                };
                let key = winner.campaign_id.to_le_bytes();
                let publisher = state.event_publisher.clone();
                let t = topic.clone();
                tokio::spawn(async move {
                    publisher.publish(&t, &key, event).await;
                });
            }

            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                response_body,
            )
                .into_response()
        }
        PipelineOutcome::Pending => {
            unreachable!("ResponseBuildStage always resolves Pending before pipeline returns")
        }
    }
}

/// SSP win notice: fired when our bid wins the auction.
///
/// Order of operations:
///   1. HMAC verification — reject before touching Redis if the token is bad.
///   2. Redis SET-NX dedup — drop duplicates without incrementing counters.
///   3. Freq-cap impression record (when user_id is present).
///   4. Kafka WinEvent publish.
///
/// Returns 200 OK on accept and on duplicate (preserves SSP semantics — non-2xx
/// causes SSPs to retry, which would amplify dedup pressure). 401 on auth fail.
#[instrument(skip(state))]
pub async fn win(State(state): State<AppState>, Query(params): Query<WinParams>) -> StatusCode {
    metrics::counter!("bidder.win.notices_total").increment(1);

    let gate_outcome = state
        .win_notice_gate
        .check(
            &params.request_id,
            &params.imp_id,
            params.campaign_id,
            params.creative_id,
            params.token.as_deref(),
        )
        .await;
    match gate_outcome {
        crate::win_notice::WinNoticeGate::Accept => {}
        crate::win_notice::WinNoticeGate::AuthFailed => {
            return StatusCode::UNAUTHORIZED;
        }
        // Dedup hit OR Redis unavailable: fail-closed — return 200 with no side effects
        // so the SSP doesn't retry, but never double-count.
        crate::win_notice::WinNoticeGate::Duplicate
        | crate::win_notice::WinNoticeGate::DedupUnavailable => {
            return StatusCode::OK;
        }
    }

    // Only record freq-cap when a user_id is present; empty string would collapse
    // all anonymous traffic into a single v1:fc:{u:}:... key and corrupt caps.
    let hour_of_day = current_hour_of_day();
    if let Some(uid) = params.user_id.as_deref().filter(|s| !s.is_empty()) {
        state.impression_recorder.try_record(ImpressionEvent {
            user_id: uid.to_string(),
            campaign_id: params.campaign_id,
            creative_id: params.creative_id,
            device_type: 0,
            hour_of_day,
        });
    } else {
        metrics::counter!("bidder.win.skipped_no_user_total").increment(1);
    }

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let event = AdEvent {
        body: Some(ad_event::Body::Win(WinEvent {
            request_id: params.request_id,
            imp_id: params.imp_id,
            campaign_id: params.campaign_id,
            creative_id: params.creative_id,
            clearing_price_micros: params.clearing_price_micros,
            timestamp_ms,
            user_id: params.user_id.unwrap_or_default(),
        })),
    };
    let key = params.campaign_id.to_le_bytes();
    let publisher = state.event_publisher.clone();
    let topic = state.events_topic.clone();
    tokio::spawn(async move {
        publisher.publish(&topic, &key, event).await;
    });

    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::WinParams;

    #[test]
    fn win_params_deserializes_required_fields() {
        let p: WinParams = serde_json::from_str(
            r#"{
            "request_id":"req1","imp_id":"imp1",
            "campaign_id":42,"creative_id":7,
            "clearing_price_micros":1500000
        }"#,
        )
        .unwrap();
        assert_eq!(p.request_id, "req1");
        assert_eq!(p.imp_id, "imp1");
        assert_eq!(p.campaign_id, 42);
        assert_eq!(p.creative_id, 7);
        assert_eq!(p.clearing_price_micros, 1_500_000);
        assert!(p.user_id.is_none());
    }

    #[test]
    fn win_params_deserializes_optional_user_id() {
        let p: WinParams = serde_json::from_str(
            r#"{
            "request_id":"r","imp_id":"i",
            "campaign_id":1,"creative_id":2,
            "clearing_price_micros":0,"user_id":"user42"
        }"#,
        )
        .unwrap();
        assert_eq!(p.user_id.as_deref(), Some("user42"));
    }

    #[test]
    fn win_params_rejects_missing_required_field() {
        // Missing clearing_price_micros.
        let result: Result<WinParams, _> = serde_json::from_str(
            r#"{
            "request_id":"r","imp_id":"i","campaign_id":1,"creative_id":2
        }"#,
        );
        assert!(result.is_err());
    }
}

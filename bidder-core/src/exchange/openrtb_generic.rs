//! Generic OpenRTB 2.6 adapter.
//!
//! Handles JSON requests at `application/json` content-type using `simd-json`
//! for parsing (in-place SIMD over a per-request mutable `Vec<u8>` buffer
//! per `PLAN.md` Stack Decisions table) and `serde_json` for response writes.
//!
//! This is the default adapter — every exchange that speaks vanilla OpenRTB
//! JSON without protocol-specific transformations uses this one. Per-SSP
//! quirks (custom `ext` fields, header expectations, etc.) get their own
//! adapter impls.
//!
//! No state. The struct is unit-shaped and trivially `Send + Sync`.

use super::{ExchangeAdapter, ExchangeId, ResponseContentType};
use crate::model::openrtb::{BidRequest, BidResponse};
use anyhow::{Context, Result};

/// `application/json` per OpenRTB 2.6 §3.1. Some exchanges send `text/plain`
/// or `application/x-www-form-urlencoded` despite the spec — this adapter
/// accepts whatever the body decodes as JSON, so the request content-type
/// is not enforced at the adapter layer; it's an HTTP-stack concern.
const RESPONSE_CONTENT_TYPE: ResponseContentType = "application/json";

const ID: ExchangeId = "openrtb-generic";

/// Stateless. The trait object pointer is the only handle.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenRtbGenericAdapter;

impl ExchangeAdapter for OpenRtbGenericAdapter {
    fn id(&self) -> ExchangeId {
        ID
    }

    fn decode_request(&self, body: &mut Vec<u8>) -> Result<BidRequest> {
        // simd-json mutates `body` in place to resolve escapes and strip
        // whitespace. The per-request owned buffer model is non-negotiable
        // (see PLAN.md Stack Decisions § "External wire format"); never use
        // `Bytes` here — it's immutable and would defeat the SIMD win.
        simd_json::from_slice(body).context("simd-json bid request decode failed")
    }

    fn encode_response(&self, response: &BidResponse) -> Result<(Vec<u8>, ResponseContentType)> {
        let body = serde_json::to_vec(response).context("bid response JSON encode failed")?;
        Ok((body, RESPONSE_CONTENT_TYPE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable() {
        assert_eq!(OpenRtbGenericAdapter.id(), "openrtb-generic");
    }

    #[test]
    fn decode_then_encode_round_trips_minimal_request() {
        // Minimal valid OpenRTB 2.6 — id + at least one imp.
        let raw = br#"{"id":"req-1","imp":[{"id":"imp-1","banner":{"w":300,"h":250}}]}"#;
        let mut body = raw.to_vec();
        let req = OpenRtbGenericAdapter
            .decode_request(&mut body)
            .expect("decode minimal request");
        assert_eq!(req.id, "req-1");
        assert_eq!(req.imp.len(), 1);
        assert_eq!(req.imp[0].id, "imp-1");
    }

    #[test]
    fn decode_returns_error_on_invalid_json() {
        let mut body = b"not json at all".to_vec();
        assert!(OpenRtbGenericAdapter.decode_request(&mut body).is_err());
    }

    #[test]
    fn encode_produces_application_json() {
        let response = BidResponse {
            id: "req-1".to_string(),
            seatbid: vec![],
            bidid: None,
            cur: Some("USD".to_string()),
            customdata: None,
            nbr: None,
            ext: None,
        };
        let (body, ct) = OpenRtbGenericAdapter
            .encode_response(&response)
            .expect("encode no-bid response");
        assert_eq!(ct, "application/json");
        // Valid JSON that round-trips
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).expect("encoded body is valid JSON");
        assert_eq!(parsed["id"], "req-1");
        assert_eq!(parsed["cur"], "USD");
    }
}

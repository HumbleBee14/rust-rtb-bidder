//! Exchange adapter trait — decouples wire-format and bidder-internal logic.
//!
//! Different ad exchanges send bid requests in different shapes:
//!
//! - **Generic OpenRTB 2.6** (most exchanges) — JSON with `BidRequest`/`BidResponse`
//!   per the spec. `OpenRtbGeneric` adapter handles this.
//! - **Google AdX** — binary protobuf over HTTP, with Google-specific cookie matching,
//!   encrypted-hyperlocal targeting, and signed `nurl` templates. Phase 7 ships
//!   the protobuf parsing + field mapping; the auction-time signing values come
//!   from a Google partnership and are not in scope here.
//! - **Magnite / Index Exchange / others** — variations on OpenRTB JSON with
//!   per-exchange `ext` fields and slightly different content-type expectations.
//!
//! The bidder hot path operates on the common internal `BidRequest`/`BidResponse`
//! types. The adapter trait is the single seam between wire bytes and that
//! internal model.
//!
//! Routing: each exchange gets its own axum route (`/rtb/openrtb/bid`,
//! `/rtb/adx/bid`, etc.) and an `Arc<dyn ExchangeAdapter>` wired into the
//! handler. The handler is a thin wrapper over `adapter.decode_request()` →
//! `pipeline.execute()` → `adapter.encode_response()`. Adding a new exchange
//! is one trait impl + one route registration; the bid path is unchanged.

use crate::model::openrtb::{BidRequest, BidResponse};
use anyhow::Result;

/// Stable string identifier for an exchange. Used in metric labels, log
/// fields, per-SSP HMAC secret lookup, and trace span attributes.
///
/// Convention: lowercase, hyphen-separated, no whitespace. Examples:
/// `openrtb-generic`, `google-adx`, `magnite`, `index-exchange`.
pub type ExchangeId = &'static str;

/// HTTP `Content-Type` header to attach to a successful bid response.
///
/// Most OpenRTB exchanges expect `application/json`. Google AdX expects
/// `application/octet-stream` (binary protobuf). Stored as `&'static str`
/// because the value is exchange-specific and never user-controlled.
pub type ResponseContentType = &'static str;

/// Adapter that translates wire bytes ↔ the bidder's internal model.
///
/// Each impl handles one exchange's bid-request encoding (OpenRTB JSON,
/// AdX protobuf, etc.) and the matching bid-response encoding. The bid
/// pipeline itself never sees the wire format.
///
/// **Threading:** instances are `Send + Sync + 'static` so a single
/// `Arc<dyn ExchangeAdapter>` can be shared across all handler invocations
/// without lock contention. Most adapters hold no state beyond compile-time
/// constants; stateful adapters (e.g. ones with rotating per-SSP keys)
/// must wrap the mutable parts in `ArcSwap` or `RwLock` themselves.
pub trait ExchangeAdapter: Send + Sync + 'static {
    /// Stable identifier for this exchange. Used in metric labels.
    fn id(&self) -> ExchangeId;

    /// Decode the raw HTTP request body into the bidder's internal model.
    ///
    /// `body` is mutable because some decoders (notably `simd-json`) require
    /// `&mut [u8]` to perform in-place SIMD parsing without an extra copy.
    /// Adapters that don't need mutation can simply ignore the mutability.
    ///
    /// Returns `Err` on any decode failure. The handler maps this to HTTP 400.
    fn decode_request(&self, body: &mut Vec<u8>) -> Result<BidRequest>;

    /// Encode an internal `BidResponse` into the wire-format bytes the SSP
    /// expects. Returns the body bytes plus the `Content-Type` header value
    /// to attach to the HTTP response.
    ///
    /// Returns `Err` on any encode failure. The handler maps this to HTTP 500.
    fn encode_response(&self, response: &BidResponse) -> Result<(Vec<u8>, ResponseContentType)>;
}

mod google_adx;
mod openrtb_generic;
pub use google_adx::GoogleAdxAdapter;
pub use openrtb_generic::OpenRtbGenericAdapter;

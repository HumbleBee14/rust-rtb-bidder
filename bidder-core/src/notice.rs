use std::sync::Arc;

/// Inputs needed to compute the win-notice URL embedded in a Bid response's `nurl`.
///
/// The bidder generates one URL per winning bid. The SSP fires a GET against
/// it on auction win, carrying these same fields back as query params plus a
/// signing token. See `docs/REDIS-KEYS.md` § "Family: Win-notice deduplication".
pub struct WinNoticeRequest<'a> {
    pub request_id: &'a str,
    pub imp_id: &'a str,
    pub campaign_id: u32,
    pub creative_id: u32,
    pub clearing_price_micros: i64,
    /// Anonymous-safe identifier the SSP echoes back in the win-notice. May be
    /// empty if the bid request had no user.id.
    pub user_id: &'a str,
}

/// Builds the per-bid `nurl` URL. Implementations live in `bidder-server` so
/// that `bidder-core` does not depend on HMAC crates or know the wire shape of
/// the win endpoint.
///
/// Returning `None` means "do not set nurl on this bid" — used when no notice
/// base URL is configured (e.g. integration tests, dev runs without an
/// externally reachable hostname).
pub trait NoticeUrlBuilder: Send + Sync + 'static {
    fn build(&self, req: &WinNoticeRequest<'_>) -> Option<String>;
}

/// Default no-op implementation used when notice URLs aren't configured.
/// Bid responses get `nurl: None` and SSPs simply don't fire the win-notice.
pub struct NoNoticeUrl;

impl NoticeUrlBuilder for NoNoticeUrl {
    fn build(&self, _req: &WinNoticeRequest<'_>) -> Option<String> {
        None
    }
}

/// Convenience type alias for the trait object stored on the pipeline.
pub type SharedNoticeUrlBuilder = Arc<dyn NoticeUrlBuilder>;

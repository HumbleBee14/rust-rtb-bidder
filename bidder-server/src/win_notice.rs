use bidder_core::{
    config::WinNoticeConfig,
    notice::{NoticeUrlBuilder, WinNoticeRequest},
};
use fred::{
    clients::Pool as RedisPool,
    interfaces::KeysInterface,
    types::{Expiration, SetOptions},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::time::timeout;

type HmacSha256 = Hmac<Sha256>;

/// Tightest acceptable Redis SET-NX latency for the dedup gate. The win path has
/// a 500ms HTTP timeout; budgeting 50ms for the SET-NX leaves the rest for Kafka
/// publish + freq-cap recording. A timeout here fails-closed (treat as duplicate)
/// to prevent double-counting on Redis blips.
const DEDUP_REDIS_TIMEOUT: Duration = Duration::from_millis(50);

/// Outcome of running the win-notice through HMAC + dedup gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinNoticeGate {
    /// First time we've seen this notice — proceed to side effects.
    Accept,
    /// Token mismatch or missing token. Return 401.
    AuthFailed,
    /// Already processed this `(request_id, imp_id)` within the dedup window. Return 200, no side effects.
    Duplicate,
    /// Redis error/timeout on the dedup write. Fail-closed: treat as duplicate so we never double-count.
    DedupUnavailable,
}

pub struct WinNoticeGateService {
    pool: RedisPool,
    cfg: WinNoticeConfig,
    /// Empty when `require_auth=false` or no secret configured. Skips HMAC verification.
    secret: Option<Arc<[u8]>>,
}

/// Minimal RFC3986 percent-encoder for query-component values. We only encode
/// the characters that are unsafe inside `application/x-www-form-urlencoded`
/// values; everything else is passed through verbatim.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let safe = matches!(b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' |
            b'-' | b'_' | b'.' | b'~'
        );
        if safe {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

impl WinNoticeGateService {
    pub fn new(pool: RedisPool, cfg: WinNoticeConfig) -> Self {
        let secret = if cfg.require_auth && !cfg.secret.is_empty() {
            Some(Arc::from(cfg.secret.as_bytes()))
        } else {
            None
        };
        Self { pool, cfg, secret }
    }

    /// Builds the HMAC-SHA256 signing message for a notice. The order and
    /// separators are part of the integration contract — every SSP that generates
    /// a `nurl` must compute over the same canonical string. See
    /// `docs/notes/phase-6-ml-scoring.md` for the spec.
    pub fn signing_message(
        request_id: &str,
        imp_id: &str,
        campaign_id: u32,
        creative_id: u32,
        clearing_price_micros: i64,
    ) -> String {
        format!("{request_id}|{imp_id}|{campaign_id}|{creative_id}|{clearing_price_micros}")
    }

    /// Computes the hex-encoded HMAC-SHA256 token for a notice. Used by the bidder
    /// when emitting `nurl` and (by extension) any reference SSP test client.
    pub fn sign(&self, message: &str) -> Option<String> {
        let secret = self.secret.as_ref()?;
        let mut mac = HmacSha256::new_from_slice(secret).ok()?;
        mac.update(message.as_bytes());
        Some(hex::encode(mac.finalize().into_bytes()))
    }

    fn verify_token(&self, message: &str, presented_hex: Option<&str>) -> bool {
        // Auth disabled or no secret configured → accept unconditionally.
        let Some(secret) = self.secret.as_ref() else {
            return true;
        };
        let Some(presented_hex) = presented_hex.filter(|s| !s.is_empty()) else {
            return false;
        };
        let Ok(presented) = hex::decode(presented_hex) else {
            return false;
        };
        let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
            return false;
        };
        mac.update(message.as_bytes());
        let expected = mac.finalize().into_bytes();
        // Constant-time compare; subtle::ConstantTimeEq returns Choice (1 = equal).
        expected.as_slice().ct_eq(&presented).into()
    }

    /// Run both gates — HMAC verification, then Redis SET-NX dedup. The order matters:
    /// reject unauthenticated requests before touching Redis so a flood of fake notices
    /// can't pressure the dedup keyspace.
    pub async fn check(
        &self,
        request_id: &str,
        imp_id: &str,
        campaign_id: u32,
        creative_id: u32,
        clearing_price_micros: i64,
        token: Option<&str>,
    ) -> WinNoticeGate {
        let message = Self::signing_message(
            request_id,
            imp_id,
            campaign_id,
            creative_id,
            clearing_price_micros,
        );
        if !self.verify_token(&message, token) {
            metrics::counter!("bidder.win.auth_failed_total").increment(1);
            return WinNoticeGate::AuthFailed;
        }

        let key = format!("v1:winx:{request_id}:{imp_id}");
        let ttl = self.cfg.dedup_ttl_secs.max(1) as i64;

        let set_result: Result<Option<bool>, fred::error::Error> = timeout(
            DEDUP_REDIS_TIMEOUT,
            self.pool.set::<Option<bool>, _, &str>(
                &key,
                "1",
                Some(Expiration::EX(ttl)),
                Some(SetOptions::NX),
                false,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            metrics::counter!("bidder.win.dedup_redis_timeout_total").increment(1);
            Err(fred::error::Error::new(
                fred::error::ErrorKind::Timeout,
                "win-notice dedup SET-NX timed out",
            ))
        });

        match set_result {
            Ok(Some(true)) => WinNoticeGate::Accept,
            Ok(_) => {
                metrics::counter!("bidder.win.duplicate_dropped_total").increment(1);
                WinNoticeGate::Duplicate
            }
            Err(e) => {
                metrics::counter!("bidder.win.dedup_redis_error_total").increment(1);
                tracing::warn!(error = %e, "win-notice dedup SET-NX failed; failing closed");
                WinNoticeGate::DedupUnavailable
            }
        }
    }
}

impl NoticeUrlBuilder for WinNoticeGateService {
    fn build(&self, req: &WinNoticeRequest<'_>) -> Option<String> {
        if self.cfg.notice_base_url.is_empty() {
            return None;
        }
        let message = Self::signing_message(
            req.request_id,
            req.imp_id,
            req.campaign_id,
            req.creative_id,
            req.clearing_price_micros,
        );
        // Token is None when auth is disabled (no secret configured); SSPs that don't
        // verify will still receive the URL, and the handler will accept (auth-off
        // path).  When auth is on, the token is mandatory and present here.
        let token_param = self
            .sign(&message)
            .map(|t| format!("&token={}", t))
            .unwrap_or_default();
        Some(format!(
            "{base}?request_id={rid}&imp_id={iid}&campaign_id={cid}&creative_id={crid}&clearing_price_micros={p}&user_id={uid}{tok}",
            base = self.cfg.notice_base_url,
            rid = percent_encode(req.request_id),
            iid = percent_encode(req.imp_id),
            cid = req.campaign_id,
            crid = req.creative_id,
            p = req.clearing_price_micros,
            uid = percent_encode(req.user_id),
            tok = token_param,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(require_auth: bool, secret: &str) -> WinNoticeConfig {
        WinNoticeConfig {
            require_auth,
            secret: secret.to_string(),
            dedup_ttl_secs: 3600,
            notice_base_url: String::new(),
        }
    }

    fn dummy_pool() -> RedisPool {
        // Tests that don't hit Redis use a pool that never connects; HMAC
        // verification is exercised independently.
        let cfg = fred::types::config::Config::from_url("redis://127.0.0.1:6379")
            .expect("dummy redis url parses");
        RedisPool::new(cfg, None, None, None, 1).expect("dummy redis pool builds")
    }

    #[test]
    fn signing_message_is_canonical_form() {
        let m = WinNoticeGateService::signing_message("req1", "imp1", 42, 7, 1_500_000);
        assert_eq!(m, "req1|imp1|42|7|1500000");
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let svc = WinNoticeGateService::new(dummy_pool(), cfg(true, "shared-secret-XYZ"));
        let msg = WinNoticeGateService::signing_message("r", "i", 1, 2, 0);
        let token = svc.sign(&msg).expect("auth enabled and secret set");
        assert!(svc.verify_token(&msg, Some(&token)));
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let svc = WinNoticeGateService::new(dummy_pool(), cfg(true, "shared-secret-XYZ"));
        let msg = WinNoticeGateService::signing_message("r", "i", 1, 2, 0);
        assert!(!svc.verify_token(&msg, Some("00".repeat(32).as_str())));
    }

    #[test]
    fn verify_rejects_missing_token_when_auth_required() {
        let svc = WinNoticeGateService::new(dummy_pool(), cfg(true, "shared-secret-XYZ"));
        let msg = WinNoticeGateService::signing_message("r", "i", 1, 2, 0);
        assert!(!svc.verify_token(&msg, None));
        assert!(!svc.verify_token(&msg, Some("")));
    }

    #[test]
    fn verify_accepts_when_auth_disabled() {
        let svc = WinNoticeGateService::new(dummy_pool(), cfg(false, ""));
        let msg = WinNoticeGateService::signing_message("r", "i", 1, 2, 0);
        // No token, no secret — accept.
        assert!(svc.verify_token(&msg, None));
    }

    #[test]
    fn verify_accepts_when_secret_empty_even_if_required() {
        // Documented degenerate config: require_auth=true with empty secret disables
        // verification at startup. Logged loudly elsewhere; unit-asserted here.
        let svc = WinNoticeGateService::new(dummy_pool(), cfg(true, ""));
        let msg = WinNoticeGateService::signing_message("r", "i", 1, 2, 0);
        assert!(svc.verify_token(&msg, None));
    }

    #[test]
    fn verify_rejects_non_hex_token() {
        let svc = WinNoticeGateService::new(dummy_pool(), cfg(true, "secret"));
        let msg = WinNoticeGateService::signing_message("r", "i", 1, 2, 0);
        assert!(!svc.verify_token(&msg, Some("not-hex-token!!")));
    }

    fn cfg_with_base(secret: &str, base: &str) -> WinNoticeConfig {
        WinNoticeConfig {
            require_auth: !secret.is_empty(),
            secret: secret.to_string(),
            dedup_ttl_secs: 3600,
            notice_base_url: base.to_string(),
        }
    }

    #[test]
    fn notice_url_omitted_when_base_empty() {
        let svc = WinNoticeGateService::new(dummy_pool(), cfg_with_base("secret", ""));
        let r = WinNoticeRequest {
            request_id: "r1",
            imp_id: "i1",
            campaign_id: 1,
            creative_id: 2,
            clearing_price_micros: 100,
            user_id: "u1",
        };
        assert!(svc.build(&r).is_none());
    }

    #[test]
    fn notice_url_signed_and_round_trips() {
        let svc = WinNoticeGateService::new(
            dummy_pool(),
            cfg_with_base("shared-secret", "https://bid.example.com/rtb/win"),
        );
        let r = WinNoticeRequest {
            request_id: "r1",
            imp_id: "i1",
            campaign_id: 42,
            creative_id: 7,
            clearing_price_micros: 1_500_000,
            user_id: "user-9",
        };
        let url = svc.build(&r).expect("nurl built when base url set");
        // Token must be present and re-verifiable
        let tok = url
            .split("token=")
            .nth(1)
            .expect("token query param present");
        let msg = WinNoticeGateService::signing_message(
            r.request_id,
            r.imp_id,
            r.campaign_id,
            r.creative_id,
            r.clearing_price_micros,
        );
        assert!(svc.verify_token(&msg, Some(tok)));
    }

    #[test]
    fn percent_encode_handles_unsafe_chars() {
        // & in a request_id must be encoded so the SSP can't inject params.
        let encoded = percent_encode("a&b=c");
        assert_eq!(encoded, "a%26b%3Dc");
        // Tilde and dot are RFC3986 unreserved — pass through.
        assert_eq!(percent_encode("foo.bar~baz"), "foo.bar~baz");
    }

    #[test]
    fn signature_is_message_dependent() {
        let svc = WinNoticeGateService::new(dummy_pool(), cfg(true, "secret"));
        let m1 = WinNoticeGateService::signing_message("r", "i", 1, 2, 0);
        let m2 = WinNoticeGateService::signing_message("r", "i", 1, 2, 1);
        let t1 = svc.sign(&m1).unwrap();
        let t2 = svc.sign(&m2).unwrap();
        assert_ne!(
            t1, t2,
            "different clearing price must produce different token"
        );
        assert!(!svc.verify_token(&m2, Some(&t1)));
    }
}

use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::info;

/// Circuit breaker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// Configuration for a single circuit breaker instance.
#[derive(Debug, Clone)]
pub struct BreakerConfig {
    /// Name for metrics / tracing labels.
    pub name: &'static str,
    /// Minimum calls in the sliding window before thresholds are evaluated.
    pub min_calls: u64,
    /// Error-rate threshold [0, 1] that opens the breaker.
    pub error_rate_threshold: f64,
    /// Slow-call ratio threshold [0, 1] that opens the breaker.
    pub slow_call_ratio_threshold: f64,
    /// A call is "slow" if its duration exceeds this value.
    pub slow_call_duration: Duration,
    /// How long the breaker stays open before transitioning to half-open.
    pub open_duration: Duration,
    /// Rolling window size (number of calls tracked).
    pub window_size: u64,
}

impl BreakerConfig {
    /// Sensible defaults for a Redis dependency with an 8 ms p99 budget.
    pub fn redis(name: &'static str) -> Self {
        Self {
            name,
            min_calls: 20,
            error_rate_threshold: 0.5,
            slow_call_ratio_threshold: 0.5,
            // Slow = p99 budget × 2
            slow_call_duration: Duration::from_millis(16),
            open_duration: Duration::from_secs(10),
            window_size: 100,
        }
    }

    pub fn kafka(name: &'static str) -> Self {
        Self {
            name,
            min_calls: 10,
            error_rate_threshold: 0.5,
            slow_call_ratio_threshold: 0.8,
            slow_call_duration: Duration::from_millis(100),
            open_duration: Duration::from_secs(30),
            window_size: 100,
        }
    }
}

/// Fixed/tumbling-window call outcome counters.
/// Counts reset when `calls` reaches `window_size`; not a true sliding window.
struct Window {
    calls: AtomicU64,
    errors: AtomicU64,
    slow_calls: AtomicU64,
}

impl Window {
    fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            slow_calls: AtomicU64::new(0),
        }
    }

    fn record(&self, error: bool, slow: bool) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        if slow {
            self.slow_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.slow_calls.store(0, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.calls.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.slow_calls.load(Ordering::Relaxed),
        )
    }
}

struct Inner {
    state: BreakerState,
    opened_at: Option<Instant>,
    half_open_probe_in_flight: bool,
}

/// Hand-rolled circuit breaker with error-rate + slow-call-ratio open conditions.
///
/// State transitions:
///   Closed  → Open      when error_rate > threshold OR slow_call_ratio > threshold (min_calls met)
///   Open    → HalfOpen  after open_duration elapses
///   HalfOpen → Closed   on one successful probe
///   HalfOpen → Open     on one failed or slow probe
pub struct CircuitBreaker {
    cfg: BreakerConfig,
    window: Arc<Window>,
    inner: RwLock<Inner>,
}

impl CircuitBreaker {
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            cfg,
            window: Arc::new(Window::new()),
            inner: RwLock::new(Inner {
                state: BreakerState::Closed,
                opened_at: None,
                half_open_probe_in_flight: false,
            }),
        }
    }

    /// Returns `true` if the breaker allows a call through.
    /// Half-open allows exactly one probe at a time; the probe slot is claimed
    /// atomically under a write lock to prevent TOCTOU races.
    pub async fn allow_request(&self) -> bool {
        // Fast path: read lock is sufficient for Closed (common case).
        {
            let inner = self.inner.read().await;
            if inner.state == BreakerState::Closed {
                return true;
            }
            if inner.state == BreakerState::Open {
                let elapsed = inner.opened_at.map(|t| t.elapsed()).unwrap_or_default();
                if elapsed < self.cfg.open_duration {
                    return false;
                }
                // Fall through to write-lock path to transition → HalfOpen.
            }
            // HalfOpen: fall through to write-lock path to claim probe slot.
        }
        // Write-lock path handles Open→HalfOpen transition and HalfOpen probe claim.
        self.try_enter_half_open().await
    }

    async fn try_enter_half_open(&self) -> bool {
        let mut inner = self.inner.write().await;
        // Re-check after acquiring write lock (another thread may have beaten us).
        if inner.state == BreakerState::Open {
            let elapsed = inner.opened_at.map(|t| t.elapsed()).unwrap_or_default();
            if elapsed >= self.cfg.open_duration {
                inner.state = BreakerState::HalfOpen;
                inner.half_open_probe_in_flight = false;
                self.window.reset();
                info!(name = self.cfg.name, "circuit breaker → half-open");
            }
        }
        if inner.state == BreakerState::HalfOpen && !inner.half_open_probe_in_flight {
            inner.half_open_probe_in_flight = true;
            true
        } else {
            false
        }
    }

    /// Record the outcome of a call.
    pub async fn record_outcome(&self, error: bool, duration: Duration) {
        let slow = duration >= self.cfg.slow_call_duration;
        self.window.record(error, slow);

        let (calls, errors, slows) = self.window.snapshot();
        let mut inner = self.inner.write().await;

        match inner.state {
            BreakerState::Closed => {
                if calls >= self.cfg.window_size {
                    self.window.reset();
                }
                if calls >= self.cfg.min_calls {
                    let error_rate = errors as f64 / calls as f64;
                    let slow_rate = slows as f64 / calls as f64;
                    if error_rate >= self.cfg.error_rate_threshold
                        || slow_rate >= self.cfg.slow_call_ratio_threshold
                    {
                        inner.state = BreakerState::Open;
                        inner.opened_at = Some(Instant::now());
                        self.window.reset();
                        metrics::counter!(
                            "bidder.circuit_breaker.opened",
                            "name" => self.cfg.name
                        )
                        .increment(1);
                        info!(
                            name = self.cfg.name,
                            error_rate, slow_rate, "circuit breaker → open"
                        );
                    }
                }
            }
            BreakerState::HalfOpen => {
                inner.half_open_probe_in_flight = false;
                if error || slow {
                    inner.state = BreakerState::Open;
                    inner.opened_at = Some(Instant::now());
                    self.window.reset();
                    metrics::counter!(
                        "bidder.circuit_breaker.opened",
                        "name" => self.cfg.name
                    )
                    .increment(1);
                    info!(
                        name = self.cfg.name,
                        error, slow, "circuit breaker → open (probe failed)"
                    );
                } else {
                    inner.state = BreakerState::Closed;
                    self.window.reset();
                    metrics::counter!(
                        "bidder.circuit_breaker.closed",
                        "name" => self.cfg.name
                    )
                    .increment(1);
                    info!(name = self.cfg.name, "circuit breaker → closed");
                }
            }
            BreakerState::Open => {}
        }
    }

    pub async fn state(&self) -> BreakerState {
        self.inner.read().await.state.clone()
    }

    pub fn is_closed_sync(&self) -> bool {
        // Best-effort non-blocking check used by hedging guardrail.
        matches!(
            self.inner.try_read().map(|g| g.state.clone()),
            Ok(BreakerState::Closed)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_cfg() -> BreakerConfig {
        BreakerConfig {
            name: "test",
            min_calls: 3,
            error_rate_threshold: 0.5,
            slow_call_ratio_threshold: 0.5,
            slow_call_duration: Duration::from_millis(100),
            open_duration: Duration::from_millis(50),
            window_size: 10,
        }
    }

    #[tokio::test]
    async fn starts_closed_and_allows_requests() {
        let cb = CircuitBreaker::new(fast_cfg());
        assert_eq!(cb.state().await, BreakerState::Closed);
        assert!(cb.allow_request().await);
    }

    #[tokio::test]
    async fn opens_after_error_rate_threshold() {
        let cb = CircuitBreaker::new(fast_cfg());
        // 3 errors in 3 calls = 100% error rate, threshold is 50%.
        for _ in 0..3 {
            cb.record_outcome(true, Duration::from_millis(1)).await;
        }
        assert_eq!(cb.state().await, BreakerState::Open);
        assert!(!cb.allow_request().await);
    }

    #[tokio::test]
    async fn opens_after_slow_call_ratio_threshold() {
        let cb = CircuitBreaker::new(fast_cfg());
        // 3 slow calls (>= 100 ms) = 100% slow rate.
        for _ in 0..3 {
            cb.record_outcome(false, Duration::from_millis(200)).await;
        }
        assert_eq!(cb.state().await, BreakerState::Open);
    }

    #[tokio::test]
    async fn does_not_open_below_min_calls() {
        let cb = CircuitBreaker::new(fast_cfg());
        // Only 2 errors — below min_calls=3, breaker should stay Closed.
        for _ in 0..2 {
            cb.record_outcome(true, Duration::from_millis(1)).await;
        }
        assert_eq!(cb.state().await, BreakerState::Closed);
    }

    #[tokio::test]
    async fn transitions_open_to_half_open_after_duration() {
        let cb = CircuitBreaker::new(fast_cfg());
        for _ in 0..3 {
            cb.record_outcome(true, Duration::from_millis(1)).await;
        }
        assert_eq!(cb.state().await, BreakerState::Open);

        // Wait for open_duration (50 ms) to elapse.
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Next allow_request should trigger Open → HalfOpen transition.
        assert!(cb.allow_request().await);
        assert_eq!(cb.state().await, BreakerState::HalfOpen);
    }

    #[tokio::test]
    async fn half_open_allows_only_one_probe() {
        let cb = CircuitBreaker::new(fast_cfg());
        for _ in 0..3 {
            cb.record_outcome(true, Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;

        // First call gets the probe slot.
        assert!(cb.allow_request().await);
        // Second concurrent call must be rejected.
        assert!(!cb.allow_request().await);
    }

    #[tokio::test]
    async fn closes_after_successful_probe() {
        let cb = CircuitBreaker::new(fast_cfg());
        for _ in 0..3 {
            cb.record_outcome(true, Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        cb.allow_request().await; // enter HalfOpen

        cb.record_outcome(false, Duration::from_millis(1)).await;
        assert_eq!(cb.state().await, BreakerState::Closed);
    }

    #[tokio::test]
    async fn reopens_after_failed_probe() {
        let cb = CircuitBreaker::new(fast_cfg());
        for _ in 0..3 {
            cb.record_outcome(true, Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        cb.allow_request().await; // enter HalfOpen

        cb.record_outcome(true, Duration::from_millis(1)).await;
        assert_eq!(cb.state().await, BreakerState::Open);
    }
}

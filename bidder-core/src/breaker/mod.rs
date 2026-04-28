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

/// Sliding-window call outcome counters.
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
    /// Half-open allows exactly one probe at a time.
    pub async fn allow_request(&self) -> bool {
        let inner = self.inner.read().await;
        match inner.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                let elapsed = inner.opened_at.map(|t| t.elapsed()).unwrap_or_default();
                if elapsed >= self.cfg.open_duration {
                    // Need write lock to transition — drop read first.
                    drop(inner);
                    self.try_enter_half_open().await
                } else {
                    false
                }
            }
            BreakerState::HalfOpen => !inner.half_open_probe_in_flight,
        }
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

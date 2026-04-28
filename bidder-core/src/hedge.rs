use crate::breaker::CircuitBreaker;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::timeout;

/// Atomic token bucket granting hedge eligibility.
///
/// Refilled at a rate proportional to the configured budget (default 10%).
/// The budget contracts when the HTTP layer sheds load:
///   - load_shed_rate > 1%  → drops to 2%
///   - load_shed_rate > 5%  → drops to 0%  (hedging fully disabled)
///
/// The refill is driven by the caller via `try_consume_token`.
pub struct HedgeBudget {
    tokens: AtomicU64,
    capacity: AtomicU64,
}

impl HedgeBudget {
    pub fn new(capacity: u64) -> Self {
        Self {
            tokens: AtomicU64::new(capacity),
            capacity: AtomicU64::new(capacity),
        }
    }

    /// Attempt to consume one hedge token.  Returns `true` if the hedge is allowed.
    pub fn try_consume(&self) -> bool {
        let mut current = self.tokens.load(Ordering::Relaxed);
        loop {
            if current == 0 {
                return false;
            }
            match self.tokens.compare_exchange_weak(
                current,
                current - 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Restore one token (call after the hedged request completes).
    pub fn restore(&self) {
        let cap = self.capacity.load(Ordering::Relaxed);
        self.tokens.fetch_min(cap, Ordering::Relaxed);
        self.tokens.fetch_add(1, Ordering::Relaxed);
    }

    /// Adjust the effective capacity based on the HTTP load-shed rate.
    /// Called periodically from the server's health monitoring loop.
    pub fn set_load_shed_rate(&self, rate: f64) {
        let nominal = self.capacity.load(Ordering::Relaxed);
        let effective = if rate > 0.05 {
            0
        } else if rate > 0.01 {
            (nominal as f64 * 0.2) as u64
        } else {
            nominal
        };
        // Clamp current tokens to new effective cap.
        let _ = self.tokens.fetch_min(effective, Ordering::Relaxed);
        self.capacity.store(effective, Ordering::Relaxed);
    }
}

/// Execute `op` and, if it hasn't returned by `trigger`, fire a second
/// parallel attempt (the hedge).  Return whichever completes first.
///
/// Guardrails (all four must hold for a hedge to fire):
///   1. Caller must use this function only for idempotent reads.
///   2. `trigger` — computed externally as `max(p95, 8 ms floor)`.
///   3. `breaker.is_closed_sync()` — health gate.
///   4. `budget.try_consume()` — adaptive token bucket.
///
/// If the hedge fires, the token is restored when either future completes.
pub async fn hedged_call<F, Fut, T>(
    op: F,
    trigger: Duration,
    breaker: &CircuitBreaker,
    budget: &HedgeBudget,
) -> T
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let first = op();
    // Fast path: if the first call returns before the trigger, no hedge needed.
    match timeout(trigger, first).await {
        Ok(result) => result,
        Err(_trigger_elapsed) => {
            // Guardrails 3 + 4
            let hedge_allowed = breaker.is_closed_sync() && budget.try_consume();
            if !hedge_allowed {
                // No hedge — wait for the original request.
                metrics::counter!("bidder.redis.hedge_blocked").increment(1);
                op().await
            } else {
                metrics::counter!("bidder.redis.hedge_fired").increment(1);
                let second = op();
                // Race first vs second; first has been pending since before trigger.
                tokio::select! {
                    result = op() => {
                        budget.restore();
                        result
                    }
                    result = second => {
                        budget.restore();
                        result
                    }
                }
            }
        }
    }
}

/// Shared hedge state for the Redis dependency.
/// One instance per `AppState`, referenced by all Redis-calling components.
pub struct RedisHedgeState {
    pub budget: Arc<HedgeBudget>,
    /// Current p95 latency of Redis calls, used to compute the trigger.
    p95_ms: AtomicU64,
}

impl RedisHedgeState {
    /// `nominal_budget_tokens` — how many requests per refill window can hedge
    /// under normal load (e.g., 10% of expected RPS × window_secs).
    pub fn new(nominal_budget_tokens: u64) -> Self {
        Self {
            budget: Arc::new(HedgeBudget::new(nominal_budget_tokens)),
            p95_ms: AtomicU64::new(0),
        }
    }

    /// Update the rolling p95 latency estimate.
    pub fn update_p95(&self, ms: u64) {
        self.p95_ms.store(ms, Ordering::Relaxed);
    }

    /// Returns the hedge trigger: `max(p95, 8 ms floor)`.
    pub fn trigger(&self) -> Duration {
        let p95 = self.p95_ms.load(Ordering::Relaxed);
        Duration::from_millis(p95.max(8))
    }
}

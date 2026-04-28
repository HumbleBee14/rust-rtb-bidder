use crate::breaker::CircuitBreaker;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::sleep;

/// Atomic token bucket granting hedge eligibility.
///
/// Budget contracts when the HTTP layer sheds load:
///   load_shed_rate > 1%  → 20% of nominal capacity
///   load_shed_rate > 5%  → 0%  (hedging fully disabled)
pub struct HedgeBudget {
    tokens: AtomicU64,
    /// Nominal (full-load) capacity; mutated by set_load_shed_rate.
    nominal: u64,
    /// Effective capacity currently in use; may be < nominal under load-shed.
    effective_cap: AtomicU64,
}

impl HedgeBudget {
    pub fn new(capacity: u64) -> Self {
        Self {
            tokens: AtomicU64::new(capacity),
            nominal: capacity,
            effective_cap: AtomicU64::new(capacity),
        }
    }

    /// Attempt to consume one hedge token. Returns `true` if the hedge is allowed.
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

    /// Restore one token after a hedged request completes.
    pub fn restore(&self) {
        // Add first, then clamp to effective_cap so we never exceed cap.
        let new = self.tokens.fetch_add(1, Ordering::Relaxed) + 1;
        let cap = self.effective_cap.load(Ordering::Relaxed);
        if new > cap {
            // Best-effort clamp — slight overshoot under very high concurrency is harmless.
            let _ = self.tokens.fetch_min(cap, Ordering::Relaxed);
        }
    }

    /// Adjust effective capacity based on the HTTP load-shed rate.
    /// Called periodically from the server's health monitoring loop.
    pub fn set_load_shed_rate(&self, rate: f64) {
        let effective = if rate > 0.05 {
            0
        } else if rate > 0.01 {
            (self.nominal as f64 * 0.2) as u64
        } else {
            self.nominal
        };
        self.effective_cap.store(effective, Ordering::Relaxed);
        // Clamp current tokens down if cap shrank.
        let _ = self.tokens.fetch_min(effective, Ordering::Relaxed);
    }
}

/// Execute `op` and, if it hasn't returned by `trigger`, fire a second
/// parallel attempt (the hedge).  Race the **original** call against the hedge;
/// return whichever finishes first.
///
/// Guardrails — all four must hold for a hedge to fire:
///   1. Caller must only pass idempotent reads (GET / MGET / SMEMBERS).
///   2. `trigger` — `max(p95, 8 ms floor)`, computed by `RedisHedgeState::trigger()`.
///   3. `breaker.is_closed_sync()` — health gate; no hedging while breaker is open.
///   4. `budget.try_consume()` — adaptive token bucket.
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
    // Pin the first future so it survives the trigger boundary and can be
    // raced against the hedge without being cancelled or restarted.
    let first = op();
    tokio::pin!(first);

    tokio::select! {
        biased;
        result = &mut first => {
            // Fast path: first call returned before the trigger.
            result
        }
        _ = sleep(trigger) => {
            // Trigger elapsed. Evaluate guardrails 3 + 4.
            let hedge_allowed = breaker.is_closed_sync() && budget.try_consume();
            if !hedge_allowed {
                metrics::counter!("bidder.redis.hedge_blocked").increment(1);
                // No hedge — wait for the original in-flight call.
                first.await
            } else {
                metrics::counter!("bidder.redis.hedge_fired").increment(1);
                let second = op();
                tokio::pin!(second);
                // Race the original (still in flight) against the new hedge call.
                tokio::select! {
                    result = &mut first => {
                        budget.restore();
                        result
                    }
                    result = &mut second => {
                        budget.restore();
                        result
                    }
                }
            }
        }
    }
}

/// Shared hedge state for the Redis dependency.
/// One instance owned by `AppState`, shared via `Arc` into both Redis call sites.
pub struct RedisHedgeState {
    pub budget: Arc<HedgeBudget>,
    /// Rolling p95 latency of Redis calls in milliseconds.
    p95_ms: AtomicU64,
}

impl RedisHedgeState {
    /// `nominal_budget_tokens` — token bucket capacity under normal load.
    /// A good starting value: 10% of expected RPS.
    pub fn new(nominal_budget_tokens: u64) -> Self {
        Self {
            budget: Arc::new(HedgeBudget::new(nominal_budget_tokens)),
            p95_ms: AtomicU64::new(0),
        }
    }

    /// Update the rolling p95 latency estimate (call from a metrics-read loop).
    pub fn update_p95(&self, ms: u64) {
        self.p95_ms.store(ms, Ordering::Relaxed);
    }

    /// Returns the hedge trigger: `max(p95, 8 ms floor)`.
    pub fn trigger(&self) -> Duration {
        let p95 = self.p95_ms.load(Ordering::Relaxed);
        Duration::from_millis(p95.max(8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::{BreakerConfig, CircuitBreaker};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn closed_breaker() -> CircuitBreaker {
        CircuitBreaker::new(BreakerConfig::redis("test"))
    }

    #[tokio::test]
    async fn fast_path_returns_without_hedge() {
        let breaker = closed_breaker();
        let budget = HedgeBudget::new(100);
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let result = hedged_call(
            move || {
                let c = Arc::clone(&calls2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    42u32
                }
            },
            Duration::from_secs(10), // trigger far in the future
            &breaker,
            &budget,
        )
        .await;

        assert_eq!(result, 42);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fast path must issue exactly one call"
        );
    }

    #[tokio::test]
    async fn hedge_fires_exactly_two_calls() {
        let breaker = closed_breaker();
        let budget = HedgeBudget::new(100);
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let result = hedged_call(
            move || {
                let c = Arc::clone(&calls2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    // First call sleeps 50 ms; second (hedge) returns immediately.
                    // Both complete, select picks whichever is first.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    99u32
                }
            },
            Duration::from_millis(5), // trigger fires almost immediately
            &breaker,
            &budget,
        )
        .await;

        assert_eq!(result, 99);
        // Both the original and the hedge were issued — exactly 2 calls.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "hedge path must issue exactly two calls"
        );
    }

    #[tokio::test]
    async fn hedge_blocked_when_budget_exhausted() {
        let breaker = closed_breaker();
        let budget = HedgeBudget::new(0); // empty budget
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        hedged_call(
            move || {
                let c = Arc::clone(&calls2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Duration::from_millis(5),
            &breaker,
            &budget,
        )
        .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no hedge when budget is zero"
        );
    }

    #[tokio::test]
    async fn restore_does_not_exceed_capacity() {
        let budget = HedgeBudget::new(10);
        // Drain all tokens.
        for _ in 0..10 {
            assert!(budget.try_consume());
        }
        assert!(!budget.try_consume());
        // Restore 20 times — tokens must not exceed capacity.
        for _ in 0..20 {
            budget.restore();
        }
        let tokens = budget.tokens.load(Ordering::Relaxed);
        assert!(tokens <= 10, "tokens={tokens} must not exceed capacity 10");
    }

    #[tokio::test]
    async fn set_load_shed_rate_contracts_budget() {
        let budget = HedgeBudget::new(100);
        budget.set_load_shed_rate(0.02); // > 1% → 20%
        assert_eq!(budget.effective_cap.load(Ordering::Relaxed), 20);

        budget.set_load_shed_rate(0.06); // > 5% → 0%
        assert_eq!(budget.effective_cap.load(Ordering::Relaxed), 0);
        assert!(!budget.try_consume());

        budget.set_load_shed_rate(0.0); // healthy → full capacity
        assert_eq!(budget.effective_cap.load(Ordering::Relaxed), 100);
    }
}

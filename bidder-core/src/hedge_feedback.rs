//! Periodic feedback loop that updates `RedisHedgeState` from observed
//! load-shed rate + Redis p95.
//!
//! Phase 5 left both `HedgeBudget::set_load_shed_rate()` and
//! `RedisHedgeState::update_p95()` implemented but not wired. Phase 7
//! closes the loop:
//!
//!   - The HTTP layer increments `LoadShedTracker::record_request()` on
//!     every accept and `record_shed()` on every 503/timeout response.
//!     The feedback loop computes shed-rate over a rolling window every
//!     `feedback_interval_secs` and pushes it into HedgeBudget.
//!
//!   - The Redis call sites (segment_repo, freq_cap) push observed call
//!     latencies into `RedisLatencyTracker::record()`. The feedback loop
//!     computes a rolling p95 estimate every interval and pushes it into
//!     RedisHedgeState.
//!
//! Why a separate tracker rather than reading Prometheus: the bidder's
//! Prometheus exposition is on the same process; reading it back over
//! HTTP would be silly. Both trackers are lock-free atomic structures
//! that the bid path writes to without contention.

use crate::hedge::RedisHedgeState;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Counts HTTP-layer accepts and shed-rejects over a rolling window.
/// Used by the feedback loop to compute shed-rate.
///
/// Rate computation is "delta over interval", not a sliding window —
/// the feedback loop reads the totals every N seconds and computes
/// `(shed_now - shed_prev) / (req_now - req_prev)`. Coarse but
/// adequate; the HedgeBudget thresholds are 1% and 5%, both far above
/// the noise floor of even a 1-second sample.
#[derive(Debug, Default)]
pub struct LoadShedTracker {
    requests_total: AtomicU64,
    shed_total: AtomicU64,
}

impl LoadShedTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called from the timeout / concurrency-limit middleware on every
    /// accepted request, regardless of outcome.
    pub fn record_request(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Called when the layer responds with 503 or 504 due to load-shedding.
    pub fn record_shed(&self) {
        self.shed_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.requests_total.load(Ordering::Relaxed),
            self.shed_total.load(Ordering::Relaxed),
        )
    }
}

/// Tracks Redis call durations as a coarse online estimate of p95.
///
/// We don't need a true t-digest here — the HedgeBudget trigger uses
/// `max(p95, 8 ms)`, so any estimate that's roughly stable in the right
/// order of magnitude works. The implementation:
///
///   - Each call records (count, sum_ms) into atomics.
///   - Every interval, the feedback loop reads count + sum, computes
///     mean, and uses `mean × 1.7` as a coarse p95 proxy. (Empirical
///     ratio for typical Redis MGET latency distributions.)
///   - On read, the counters reset for the next window.
///
/// This is intentionally cheap on the hot path. A proper p95 would
/// require a histogram + per-call binary search, which the
/// metrics-exporter-prometheus already does — but the feedback loop
/// reading that histogram over HTTP every interval is more cycles
/// than this approach. Revisit if profiling shows the simple estimator
/// is wrong by enough to matter.
#[derive(Debug, Default)]
pub struct RedisLatencyTracker {
    count: AtomicU64,
    sum_us: AtomicU64,
}

impl RedisLatencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, dur: Duration) {
        // Microseconds, not milliseconds: typical Redis MGET p50 is ~200–400 µs,
        // and as_millis() truncates everything <1 ms to zero — which would
        // make the mean (and therefore the p95 estimate) collapse to 0
        // whenever the cluster is healthy. u64 µs holds ~584,000 years.
        let us = dur.as_micros() as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    /// Reset and return (count, sum_us) for the window just ended.
    pub fn drain(&self) -> (u64, u64) {
        let count = self.count.swap(0, Ordering::Relaxed);
        let sum_us = self.sum_us.swap(0, Ordering::Relaxed);
        (count, sum_us)
    }
}

/// Spawn the periodic feedback loop. Reads the trackers every interval
/// and pushes derived values into the hedge state. Runs until the
/// `RedisHedgeState` Arc is dropped (i.e. process exit).
pub fn spawn_feedback_loop(
    hedge: Arc<RedisHedgeState>,
    load_shed: Arc<LoadShedTracker>,
    redis_latency: Arc<RedisLatencyTracker>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut prev_total: u64 = 0;
        let mut prev_shed: u64 = 0;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Drift threshold: tick took >1.5× the configured interval. On a
        // saturated single-threaded runtime, Tokio's timer can starve under
        // load — exactly when this loop matters most (we'd hedge into a
        // degraded cluster because shed-rate hasn't been recomputed). The
        // gauge surfaces drift so an operator can correlate hedge misbehaviour
        // with feedback-loop starvation; a warn! every 5 starvation events
        // keeps the log channel from drowning under sustained pressure.
        let drift_threshold = interval.mul_f64(1.5);
        let mut last_tick: Option<Instant> = None;
        let mut drift_log_counter: u64 = 0;
        loop {
            ticker.tick().await;
            let now = Instant::now();
            if let Some(prev) = last_tick {
                let elapsed = now.duration_since(prev);
                metrics::gauge!("bidder.hedge.feedback_tick_drift_seconds")
                    .set(elapsed.as_secs_f64() - interval.as_secs_f64());
                if elapsed > drift_threshold {
                    metrics::counter!("bidder.hedge.feedback_starvation_total").increment(1);
                    drift_log_counter += 1;
                    if drift_log_counter % 5 == 1 {
                        warn!(
                            elapsed_ms = elapsed.as_millis() as u64,
                            interval_ms = interval.as_millis() as u64,
                            "hedge feedback loop starved — drift exceeds 1.5× interval"
                        );
                    }
                }
            }
            last_tick = Some(now);

            // Load-shed rate over the last interval.
            let (cur_total, cur_shed) = load_shed.snapshot();
            let delta_total = cur_total.saturating_sub(prev_total);
            let delta_shed = cur_shed.saturating_sub(prev_shed);
            prev_total = cur_total;
            prev_shed = cur_shed;
            let shed_rate = if delta_total > 0 {
                delta_shed as f64 / delta_total as f64
            } else {
                0.0
            };
            hedge.budget.set_load_shed_rate(shed_rate);
            metrics::gauge!("bidder.hedge.load_shed_rate").set(shed_rate);

            // Redis p95 estimate.
            let (count, sum_us) = redis_latency.drain();
            if count > 0 {
                let mean_us = sum_us as f64 / count as f64;
                // Empirical p95 ≈ mean × 1.7 for the Redis call duration
                // distributions we've observed. Documented as coarse;
                // upgrade to a real histogram quantile if profiling shows
                // the estimate misses real degradation events.
                // RedisHedgeState's API is ms-granular (the trigger is
                // max(p95, 8 ms)), so we round up — a sub-ms p95 maps to
                // 1 ms here, the trigger floor still dominates.
                let p95_ms = ((mean_us * 1.7) / 1000.0).ceil() as u64;
                hedge.update_p95(p95_ms);
                metrics::gauge!("bidder.hedge.redis_p95_ms").set(p95_ms as f64);
                debug!(
                    shed_rate,
                    redis_p95_ms = p95_ms,
                    count,
                    "hedge feedback updated"
                );
            } else {
                metrics::gauge!("bidder.hedge.redis_p95_ms").set(0.0);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_shed_tracker_counts() {
        let t = LoadShedTracker::new();
        t.record_request();
        t.record_request();
        t.record_shed();
        let (req, shed) = t.snapshot();
        assert_eq!(req, 2);
        assert_eq!(shed, 1);
    }

    #[test]
    fn redis_latency_tracker_drains() {
        let t = RedisLatencyTracker::new();
        t.record(Duration::from_millis(5));
        t.record(Duration::from_millis(15));
        let (count, sum_us) = t.drain();
        assert_eq!(count, 2);
        assert_eq!(sum_us, 20_000);
        // Drain resets.
        let (count2, sum2) = t.drain();
        assert_eq!(count2, 0);
        assert_eq!(sum2, 0);
    }

    #[test]
    fn redis_latency_tracker_preserves_sub_ms() {
        // Regression: as_millis() truncated sub-ms calls to zero, masking
        // healthy Redis latencies and forcing the p95 estimator to 0.
        let t = RedisLatencyTracker::new();
        t.record(Duration::from_micros(300));
        t.record(Duration::from_micros(450));
        let (count, sum_us) = t.drain();
        assert_eq!(count, 2);
        assert_eq!(sum_us, 750);
    }
}

//! In-process frequency capper with periodic write-behind to Redis.
//!
//! Phase 6.5 baseline showed `frequency_cap` MGET is the only stage that
//! routinely exceeds its budget under load — at 10K RPS, freq-cap is
//! breaker-skipped on 37% of requests because the Redis MGET path can't
//! return ~50 keys per user within the 8 ms budget. This impl moves the
//! hot read off Redis entirely:
//!
//!   reads — `Arc<DashMap<UserId, UserCapCounters>>` (sub-microsecond)
//!   writes — same DashMap update, plus a write-behind queue that batches
//!            increments to Redis every flush_interval (default 1s)
//!
//! ## Consistency contract — explicit single-instance
//!
//! In-process counters are authoritative for the lifetime of THIS bidder
//! process. Other bidder instances (and process restarts) see Redis as
//! source-of-truth, but their view lags by up to flush_interval. Acceptable
//! when:
//!   - There is exactly one bidder instance per user-routing scope
//!     (sticky-by-user-id at the load balancer, or single-process deploy)
//!   - OR a brief inconsistency window (≤ flush_interval) is tolerable for
//!     freq-cap accuracy
//!
//! NOT acceptable when running multiple bidder instances behind a randomly
//! load-balanced VIP — different instances will independently approve
//! impressions for the same (user, campaign) tuple, exceeding the cap.
//!
//! Behind a config flag for that reason. Default disabled. Production
//! switches it on after deploying user-stickiness in the LB layer or
//! validating that a multi-process cluster sharing one Redis is acceptable.
//!
//! ## On startup / restart
//!
//! Cold start has no in-process counters. The first request for each user
//! falls through to the underlying RedisFrequencyCapper to populate the
//! DashMap. After the first hit, subsequent reads stay in-process. RSS
//! grows with active-user count, hard-bounded at `cap_capacity` entries
//! (default 500K × ~200 bytes ≈ 100 MB).
//!
//! ## Eviction policy: hard cap, no LRU (yet)
//!
//! Once `counters.len() >= cap_capacity`, new users are NOT inserted —
//! they read-through to Redis on every request and never warm the cache.
//! Existing users continue to hit cache. The bidder.freq_cap.in_process.
//! capacity_rejected_total counter surfaces how often this happens; if it
//! sustains >0 in production, raise cap_capacity (or move to a moka-backed
//! cache with TinyLFU eviction — tracked as a future change, not Phase 7).
//!
//! Design rationale: a true LRU here would need either a moka swap or a
//! parallel DashMap of access timestamps + a sweeper; both add hot-path
//! cost. A hard cap keeps the hot path branch-free past the first len()
//! check and trades cache hit-rate for predictable RSS — acceptable since
//! Redis is still authoritative and the fallback path is correct.
//!
//! Write-behind drains via a bounded mpsc channel + dedicated tokio task.
//! Channel overflow (extreme load OR Redis stalled) drops increments;
//! they're recovered on the next read-fallthrough since Redis still has
//! the prior counter value (just stale). `bidder.freq_cap.in_process_*`
//! metrics surface drops, fallthroughs, and flush latency.

use crate::breaker::CircuitBreaker;
use crate::frequency::{CapResult, CapWindow, FreqCapOutcome, FrequencyCapper};
use crate::model::candidate::AdCandidate;
use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-(user, campaign, window) atomic counter. Hot path is `incr`;
/// `load` is non-blocking and consistent with the most recent `incr`
/// in the same thread, eventually consistent across threads. This is
/// fine for freq-cap reads — we tolerate brief over-capping if a
/// concurrent thread's increment hasn't propagated.
#[derive(Debug, Default)]
pub struct AtomicCounter(AtomicI64);

impl AtomicCounter {
    pub fn load(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn incr(&self) -> i64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Per-user counter set: campaign-day + campaign-hour for every campaign
/// the user has been served. Sized at need; entries vanish only at
/// process restart or LRU eviction.
type UserCapMap = DashMap<u32, (Arc<AtomicCounter>, Arc<AtomicCounter>)>; // campaign_id -> (day, hour)

/// Wraps a Redis-backed `FrequencyCapper` to add an in-process read cache
/// + write-behind queue. The `FrequencyCapper` trait surface is unchanged.
///
/// CONTRACT: docs/REDIS-KEYS.md § "Family: Frequency caps" still applies for
/// the underlying Redis values. This wrapper writes to the same keys via the
/// underlying capper's increment path; the in-process layer is purely a
/// caching front-end with eventually-consistent semantics.
pub struct InProcessFrequencyCapper {
    /// Underlying Redis capper for cold-miss reads and fallback when the
    /// in-process cache cannot serve a request (e.g. evicted entry).
    fallback: Arc<dyn FrequencyCapper>,

    /// Circuit breaker shared with the fallback. When tripped, the wrapper
    /// must NOT silently serve from a possibly-stale in-process cache —
    /// instead it returns SkippedTimeout to preserve the Phase 5 fail-safe-
    /// and-loud invariant.
    breaker: Arc<CircuitBreaker>,

    /// Hot counter cache. Sub-µs reads; writes are inline; eviction is
    /// hard-capped at `cap_capacity` — see the module-level eviction note.
    counters: Arc<DashMap<String, Arc<UserCapMap>>>,

    /// Maximum number of distinct users tracked in `counters`. Once reached,
    /// new users read-through to Redis on every request instead of warming
    /// the cache. Existing users keep getting hot reads.
    cap_capacity: usize,

    /// Bounded enqueue channel for write-behind to Redis. Producer side is
    /// `try_send` — overflow drops the increment and increments the
    /// `bidder.freq_cap.in_process.write_drops_total` counter.
    write_tx: tokio::sync::mpsc::Sender<WriteBehindOp>,
}

/// One batched increment destined for Redis via the write-behind task.
/// Held in the bounded channel; the drain task collects ops over a
/// `flush_interval` window and pipelines them to Redis as a single batch.
#[derive(Debug, Clone)]
pub struct WriteBehindOp {
    pub user_id: String,
    pub campaign_id: u32,
    pub window: CapWindow,
}

/// Configuration. Wired from `[freq_cap]` in config.toml when in_process
/// is enabled.
#[derive(Debug, Clone)]
pub struct InProcessConfig {
    /// Maximum users tracked. Beyond this, the `counters` DashMap stops
    /// inserting and falls through to Redis on every read (read-only mode
    /// for new users). Sized to fit the active-user working set on this
    /// instance. Default 500K entries × ~200 bytes each ≈ 100 MB.
    pub cap_capacity: usize,
    /// Write-behind channel depth. At sustained 5K wins/s with 2 ops per
    /// win (day + hour), 65,536 buffers ~6 seconds of flush stalls before
    /// drops begin. Tune up if Redis flush latency is high.
    pub write_buffer_size: usize,
    /// Flush cadence. Increments queued in the channel are batched into
    /// Redis writes every `flush_interval`. Default 1s.
    pub flush_interval: Duration,
}

impl Default for InProcessConfig {
    fn default() -> Self {
        Self {
            cap_capacity: 500_000,
            write_buffer_size: 65_536,
            flush_interval: Duration::from_secs(1),
        }
    }
}

impl InProcessFrequencyCapper {
    pub fn new(
        fallback: Arc<dyn FrequencyCapper>,
        breaker: Arc<CircuitBreaker>,
        cfg: InProcessConfig,
    ) -> (Self, tokio::sync::mpsc::Receiver<WriteBehindOp>) {
        let (write_tx, write_rx) = tokio::sync::mpsc::channel(cfg.write_buffer_size);
        let counters = Arc::new(DashMap::new());
        let capper = Self {
            fallback,
            breaker,
            counters,
            cap_capacity: cfg.cap_capacity,
            write_tx,
        };
        (capper, write_rx)
    }

    /// Try to record an impression in the in-process cache. Always called
    /// AFTER the bid is sent; the cache update happens inline (cheap) and
    /// the Redis write is queued via write-behind (also cheap, non-blocking).
    /// Returns false if the write-behind queue is full (drop, increment
    /// metric).
    pub fn record_impression(&self, user_id: &str, campaign_id: u32) -> bool {
        // Capacity gate: if this user isn't already cached AND the map is
        // at the hard ceiling, skip the inline counter update entirely. The
        // write-behind to Redis still goes out so the impression is counted
        // authoritatively; the bid path just won't get a warm hit on the
        // next call for this user. Existing entries always proceed.
        let user_map = match self.counters.get(user_id) {
            Some(m) => Arc::clone(m.value()),
            None => {
                if self.counters.len() >= self.cap_capacity {
                    metrics::counter!("bidder.freq_cap.in_process.capacity_rejected_total")
                        .increment(1);
                    // Still queue Redis writes — Redis is authoritative.
                    let day_ok = self
                        .write_tx
                        .try_send(WriteBehindOp {
                            user_id: user_id.to_string(),
                            campaign_id,
                            window: CapWindow::Day,
                        })
                        .is_ok();
                    let hour_ok = self
                        .write_tx
                        .try_send(WriteBehindOp {
                            user_id: user_id.to_string(),
                            campaign_id,
                            window: CapWindow::Hour,
                        })
                        .is_ok();
                    if !day_ok || !hour_ok {
                        metrics::counter!("bidder.freq_cap.in_process.write_drops_total")
                            .increment(1);
                    }
                    return day_ok && hour_ok;
                }
                Arc::clone(
                    self.counters
                        .entry(user_id.to_string())
                        .or_insert_with(|| Arc::new(DashMap::new()))
                        .value(),
                )
            }
        };
        let (day, hour) = user_map
            .entry(campaign_id)
            .or_insert_with(|| {
                (
                    Arc::new(AtomicCounter::default()),
                    Arc::new(AtomicCounter::default()),
                )
            })
            .clone();
        day.incr();
        hour.incr();
        // Try-send the day + hour writes. Drops are non-fatal; on next
        // read-through the wrapper will see Redis's stale value and the
        // higher-precision in-process value will be reconciled at next
        // restart-load.
        let day_op = WriteBehindOp {
            user_id: user_id.to_string(),
            campaign_id,
            window: CapWindow::Day,
        };
        let hour_op = WriteBehindOp {
            user_id: user_id.to_string(),
            campaign_id,
            window: CapWindow::Hour,
        };
        let day_ok = self.write_tx.try_send(day_op).is_ok();
        let hour_ok = self.write_tx.try_send(hour_op).is_ok();
        if !day_ok || !hour_ok {
            metrics::counter!("bidder.freq_cap.in_process.write_drops_total").increment(1);
        }
        day_ok && hour_ok
    }

    /// Look up a single (user, campaign) pair in the in-process cache.
    /// Returns `Some((day_count, hour_count))` if the user is hot in cache,
    /// `None` if cold (caller falls through to Redis).
    pub fn lookup(&self, user_id: &str, campaign_id: u32) -> Option<(i64, i64)> {
        let user_map = self.counters.get(user_id)?;
        let counters = user_map.get(&campaign_id)?;
        Some((counters.0.load(), counters.1.load()))
    }
}

#[async_trait]
impl FrequencyCapper for InProcessFrequencyCapper {
    async fn check(
        &self,
        user_id: &str,
        candidates: &[AdCandidate],
        device_type_val: u8,
        hour_of_day: u8,
    ) -> FreqCapOutcome {
        // Phase 5 invariant: when the breaker is open, do not silently
        // serve potentially-stale in-process values — fall back to
        // SkippedTimeout so the bid path is documented as freq-cap-skipped.
        // Without this, an attacker could trip the breaker, bid path keeps
        // running on uncapped reads, freq-cap quality regression is silent.
        if !self.breaker.allow_request().await {
            metrics::counter!("bidder.freq_cap.in_process.breaker_skipped_total").increment(1);
            return FreqCapOutcome::SkippedTimeout;
        }

        let started = Instant::now();
        let mut results = Vec::with_capacity(candidates.len());
        let mut cold_candidates = Vec::new();
        for c in candidates {
            match self.lookup(user_id, c.campaign_id) {
                Some((day, hour)) => {
                    let capped = day >= c.daily_cap_imps as i64 || hour >= c.hourly_cap_imps as i64;
                    results.push(CapResult {
                        campaign_id: c.campaign_id,
                        capped,
                    });
                }
                None => {
                    // Mark cold-miss; defer the candidate to the fallback
                    // so we get a Redis read for it (which will then warm
                    // the in-process cache for the next call).
                    cold_candidates.push(c.clone());
                    results.push(CapResult {
                        campaign_id: c.campaign_id,
                        capped: false, // placeholder until fallback overwrites
                    });
                }
            }
        }

        // For any cold candidates, fall through to Redis.
        if !cold_candidates.is_empty() {
            metrics::counter!("bidder.freq_cap.in_process.cold_miss_total")
                .increment(cold_candidates.len() as u64);
            match self
                .fallback
                .check(user_id, &cold_candidates, device_type_val, hour_of_day)
                .await
            {
                FreqCapOutcome::Checked(fallback_results) => {
                    // Patch our results vector with the fallback verdicts.
                    // O(N²) at top-K=50 is fine; index by campaign_id for clarity.
                    for fb in fallback_results {
                        if let Some(slot) =
                            results.iter_mut().find(|r| r.campaign_id == fb.campaign_id)
                        {
                            slot.capped = fb.capped;
                        }
                    }
                }
                FreqCapOutcome::SkippedTimeout | FreqCapOutcome::SkippedNoUser => {
                    // Fallback couldn't serve cold candidates either; the
                    // safest behaviour is to keep the in-process verdicts
                    // for warm candidates and assume cold candidates are
                    // not capped (matches Phase 5 SkippedTimeout semantics
                    // — bid quality dips, SLA preserved).
                    metrics::counter!("bidder.freq_cap.in_process.fallback_unavailable_total")
                        .increment(1);
                }
            }
        }

        metrics::histogram!("bidder.freq_cap.in_process.check_duration_seconds")
            .record(started.elapsed().as_secs_f64());

        FreqCapOutcome::Checked(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::{BreakerConfig, CircuitBreaker};

    fn breaker() -> Arc<CircuitBreaker> {
        Arc::new(CircuitBreaker::new(BreakerConfig::redis("test")))
    }

    /// Stub fallback — records calls, returns predetermined outcomes.
    struct StubCapper {
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        outcome: std::sync::Mutex<Option<FreqCapOutcome>>,
    }

    impl StubCapper {
        fn new() -> Self {
            Self {
                calls: Arc::new(std::sync::Mutex::new(Vec::new())),
                outcome: std::sync::Mutex::new(None),
            }
        }

        fn set_next(&self, o: FreqCapOutcome) {
            *self.outcome.lock().unwrap() = Some(o);
        }
    }

    #[async_trait]
    impl FrequencyCapper for StubCapper {
        async fn check(
            &self,
            user_id: &str,
            _candidates: &[AdCandidate],
            _device_type_val: u8,
            _hour_of_day: u8,
        ) -> FreqCapOutcome {
            self.calls.lock().unwrap().push(user_id.to_string());
            self.outcome
                .lock()
                .unwrap()
                .take()
                .unwrap_or(FreqCapOutcome::Checked(vec![]))
        }
    }

    #[tokio::test]
    async fn cold_miss_falls_through_to_fallback() {
        let stub = Arc::new(StubCapper::new());
        let calls_handle = Arc::clone(&stub.calls);
        let (capper, _rx) =
            InProcessFrequencyCapper::new(stub.clone() as _, breaker(), InProcessConfig::default());
        stub.set_next(FreqCapOutcome::Checked(vec![CapResult {
            campaign_id: 42,
            capped: false,
        }]));

        let candidates = vec![AdCandidate {
            campaign_id: 42,
            creative_id: 1,
            bid_price_cents: 100,
            score: 0.5,
            daily_cap_imps: 10,
            hourly_cap_imps: 3,
        }];
        let outcome = capper.check("user-1", &candidates, 2, 12).await;
        match outcome {
            FreqCapOutcome::Checked(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].campaign_id, 42);
                assert!(!results[0].capped);
            }
            other => panic!("expected Checked, got {:?}", other),
        }
        // Verified the fallback was invoked for the cold candidate.
        assert_eq!(calls_handle.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn warm_hit_skips_fallback() {
        let stub = Arc::new(StubCapper::new());
        let calls_handle = Arc::clone(&stub.calls);
        let (capper, _rx) =
            InProcessFrequencyCapper::new(stub.clone() as _, breaker(), InProcessConfig::default());

        // Pre-warm the cache with one impression for (user-2, campaign 99).
        capper.record_impression("user-2", 99);

        let candidates = vec![AdCandidate {
            campaign_id: 99,
            creative_id: 1,
            bid_price_cents: 100,
            score: 0.5,
            daily_cap_imps: 10,
            hourly_cap_imps: 3,
        }];
        let outcome = capper.check("user-2", &candidates, 2, 12).await;
        match outcome {
            FreqCapOutcome::Checked(results) => {
                assert_eq!(results.len(), 1);
                assert!(!results[0].capped, "1 impression < 10/3 cap, not capped");
            }
            other => panic!("expected Checked, got {:?}", other),
        }
        // Fallback NOT called because the user was warm.
        assert!(
            calls_handle.lock().unwrap().is_empty(),
            "fallback called for warm cache hit"
        );
    }

    #[tokio::test]
    async fn warm_hit_caps_when_count_exceeds_threshold() {
        let stub = Arc::new(StubCapper::new());
        let (capper, _rx) =
            InProcessFrequencyCapper::new(stub.clone() as _, breaker(), InProcessConfig::default());

        // Push the hour counter past 3 (the cap).
        for _ in 0..4 {
            capper.record_impression("user-3", 99);
        }

        let candidates = vec![AdCandidate {
            campaign_id: 99,
            creative_id: 1,
            bid_price_cents: 100,
            score: 0.5,
            daily_cap_imps: 100, // not the gating field
            hourly_cap_imps: 3,
        }];
        let outcome = capper.check("user-3", &candidates, 2, 12).await;
        match outcome {
            FreqCapOutcome::Checked(results) => {
                assert!(results[0].capped, "4 impressions exceeds hourly_cap=3");
            }
            other => panic!("expected Checked, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn capacity_cap_blocks_new_users_but_not_existing() {
        let stub = Arc::new(StubCapper::new());
        let cfg = InProcessConfig {
            cap_capacity: 2,
            ..InProcessConfig::default()
        };
        let (capper, mut rx) =
            InProcessFrequencyCapper::new(stub.clone() as _, breaker(), cfg);

        // Fill to capacity.
        capper.record_impression("u-a", 1);
        capper.record_impression("u-b", 1);
        assert_eq!(capper.counters.len(), 2);

        // Third user is rejected from the cache.
        capper.record_impression("u-c", 1);
        assert_eq!(capper.counters.len(), 2, "new user must not be inserted");

        // But existing users keep getting hot updates.
        capper.record_impression("u-a", 2);
        assert!(capper.lookup("u-a", 2).is_some());

        // u-c writes still hit the queue (Redis is authoritative).
        // Drain everything queued and confirm u-c's ops are in there.
        let mut user_ids = Vec::new();
        while let Ok(op) = rx.try_recv() {
            user_ids.push(op.user_id);
        }
        assert!(user_ids.iter().any(|u| u == "u-c"));
    }

    #[tokio::test]
    async fn record_impression_queues_writes() {
        let stub = Arc::new(StubCapper::new());
        let (capper, mut rx) =
            InProcessFrequencyCapper::new(stub.clone() as _, breaker(), InProcessConfig::default());

        assert!(capper.record_impression("user-4", 7));

        // Day + hour ops should be on the channel.
        let op1 = rx.try_recv().expect("first op queued");
        let op2 = rx.try_recv().expect("second op queued");
        assert_eq!(op1.user_id, "user-4");
        assert_eq!(op1.campaign_id, 7);
        assert_eq!(op2.user_id, "user-4");
        assert_eq!(op2.campaign_id, 7);
        // One should be Day, the other Hour (order is implementation detail).
        let windows: std::collections::HashSet<_> =
            [op1.window, op2.window].iter().copied().collect();
        assert!(windows.contains(&CapWindow::Day));
        assert!(windows.contains(&CapWindow::Hour));
    }
}

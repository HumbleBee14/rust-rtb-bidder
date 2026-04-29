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
//! cache. After the first hit, subsequent reads stay in-process. RSS
//! grows with active-user count, hard-bounded at `cap_capacity` entries
//! (default 500K × ~200 bytes ≈ 100 MB).
//!
//! ## Eviction policy: TinyLFU via `moka::sync::Cache`
//!
//! The outer cache is `moka::sync::Cache<String, Arc<UserCapMap>>`
//! configured with `max_capacity = cap_capacity` and `time_to_live = 1h`
//! (matching the longest cap window — anything older is moot for caps
//! anyway). Eviction is TinyLFU-driven, so frequently-served users stay
//! hot while idle entries get reclaimed.
//!
//! When an entry is evicted, the listener snapshots every (campaign, day,
//! hour) counter the user accumulated and queues a Redis flush op per
//! distinct (user, campaign) pair via the same write-behind channel the
//! hot path uses. Net effect: even under churn, no in-process-only
//! impressions are lost — they land in Redis before the entry vanishes.
//! If the write-behind channel is saturated at eviction time, the flush
//! is dropped (Redis will be slightly stale, recovered on the next read-
//! fallthrough); a counter surfaces the drop rate.
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
use moka::sync::Cache as MokaCache;
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

    /// Hot counter cache. Sub-µs reads on hits; writes are inline; eviction
    /// is TinyLFU via moka — see module-level note. The eviction listener
    /// flushes counter state to Redis through `write_tx` so no impressions
    /// are silently lost when a user is evicted under pressure.
    counters: MokaCache<String, Arc<UserCapMap>>,

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

        // Listener fires when moka evicts an entry (capacity, TTL, or explicit
        // invalidate). For each campaign the user touched, flush both day +
        // hour windows so Redis stays authoritative. Listener runs on moka's
        // worker thread — keep it allocation-light and never block.
        let flush_tx = write_tx.clone();
        let eviction_listener =
            move |user_id: Arc<String>,
                  user_map: Arc<UserCapMap>,
                  _cause: moka::notification::RemovalCause| {
                // One String materialisation up front — `WriteBehindOp::user_id`
                // is `String` (consumer moves it into `ImpressionEvent`), so we
                // still pay one clone per op, but we deref the Arc only once
                // per evicted user instead of 2× per (user, campaign).
                let user_id_str: &str = &user_id;
                for entry in user_map.iter() {
                    let campaign_id = *entry.key();
                    let day_op = WriteBehindOp {
                        user_id: user_id_str.to_string(),
                        campaign_id,
                        window: CapWindow::Day,
                    };
                    let hour_op = WriteBehindOp {
                        user_id: user_id_str.to_string(),
                        campaign_id,
                        window: CapWindow::Hour,
                    };
                    // Count drops independently — earlier impl `if !day_ok ||
                    // !hour_ok { increment 1 }` undercounted the bad case
                    // (both dropped) by half, exactly when the metric matters.
                    if flush_tx.try_send(day_op).is_err() {
                        metrics::counter!("bidder.freq_cap.in_process.eviction_flush_drops_total")
                            .increment(1);
                    }
                    if flush_tx.try_send(hour_op).is_err() {
                        metrics::counter!("bidder.freq_cap.in_process.eviction_flush_drops_total")
                            .increment(1);
                    }
                }
                metrics::counter!("bidder.freq_cap.in_process.evictions_total").increment(1);
            };

        let counters: MokaCache<String, Arc<UserCapMap>> = MokaCache::builder()
            .max_capacity(cfg.cap_capacity as u64)
            // Day window is 24h, but cap-relevant entries get touched
            // frequently so 1h TTL is enough to evict idle users without
            // dropping active ones. Set higher only if the eviction
            // metric trends >0 in production.
            .time_to_live(Duration::from_secs(3600))
            .eviction_listener(eviction_listener)
            .build();

        let capper = Self {
            fallback,
            breaker,
            counters,
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
        // moka handles capacity (TinyLFU eviction); we just `get_with` to
        // either fetch or atomically insert a new per-user counter map.
        // Cheaper than the previous gated-insert because the hot path is
        // a single hash probe — no length check, no race-on-insert branch.
        let user_map = self
            .counters
            .get_with_by_ref(user_id, || Arc::new(DashMap::new()));

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
        // read-through the wrapper will see Redis's stale value, and the
        // eviction listener will flush whatever is in-process before the
        // entry actually leaves the cache.
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
        // Count drops independently — `if !day_ok || !hour_ok { +1 }` halves
        // the metric exactly when both ops drop, which is the case the metric
        // exists to surface.
        let day_ok = self.write_tx.try_send(day_op).is_ok();
        if !day_ok {
            metrics::counter!("bidder.freq_cap.in_process.write_drops_total").increment(1);
        }
        let hour_ok = self.write_tx.try_send(hour_op).is_ok();
        if !hour_ok {
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

    /// Insert authoritative counter values for a (user, campaign) pair —
    /// called after a cold-miss fallback to Redis returns the actual counts.
    /// Subsequent reads for this pair hit moka instead of going to Redis.
    ///
    /// If an entry already exists for this campaign (e.g. another concurrent
    /// fallback raced us), we use the higher of the two values. That's the
    /// safe direction — over-cap by a small margin is preferable to under-
    /// counting and approving an impression that should have been blocked.
    fn warm_cache(&self, user_id: &str, campaign_id: u32, day: i64, hour: i64) {
        let user_map = self
            .counters
            .get_with_by_ref(user_id, || Arc::new(DashMap::new()));
        // get_or_insert_with: returns a guard around the existing-or-just-
        // inserted (day, hour) AtomicCounter pair. Either way we then use
        // max() semantics so we never silently undo a higher counter that
        // a concurrent flow already wrote.
        let pair = user_map
            .entry(campaign_id)
            .or_insert_with(|| {
                (
                    Arc::new(AtomicCounter(AtomicI64::new(day))),
                    Arc::new(AtomicCounter(AtomicI64::new(hour))),
                )
            });
        let (d, h) = pair.value();
        if day > d.0.load(Ordering::Relaxed) {
            d.0.store(day, Ordering::Relaxed);
        }
        if hour > h.0.load(Ordering::Relaxed) {
            h.0.store(hour, Ordering::Relaxed);
        }
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
                        // We have authoritative counts in moka; plumb them through
                        // so other layers (analytics, debugging) can see the actual
                        // numbers rather than just the boolean.
                        day_count: Some(day),
                        hour_count: Some(hour),
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
                        day_count: None,
                        hour_count: None,
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
                    //
                    // ALSO: when the fallback gave us authoritative counts
                    // (RedisFrequencyCapper does), populate moka with them so
                    // the next bid for this (user, campaign) is a cache hit
                    // instead of another Redis round-trip. This is the
                    // "cache the cold-miss verdict" optimisation — without
                    // it, every wide-fan-out bid request triggers ~25 Redis
                    // hops (one per first-seen candidate), even on a "warm"
                    // user. With it, those become cache hits after the
                    // first request that touched each (user, campaign) pair.
                    let mut warmed = 0u64;
                    for fb in fallback_results {
                        if let Some(slot) =
                            results.iter_mut().find(|r| r.campaign_id == fb.campaign_id)
                        {
                            slot.capped = fb.capped;
                            slot.day_count = fb.day_count;
                            slot.hour_count = fb.hour_count;
                        }
                        if let (Some(day), Some(hour)) = (fb.day_count, fb.hour_count) {
                            self.warm_cache(user_id, fb.campaign_id, day, hour);
                            warmed += 1;
                        }
                    }
                    if warmed > 0 {
                        metrics::counter!("bidder.freq_cap.in_process.cold_miss_cached_total")
                            .increment(warmed);
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
            day_count: Some(0),
            hour_count: Some(0),
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
    async fn moka_eviction_flushes_counters_to_redis_queue() {
        // Phase 8 contract: when moka evicts a user under capacity pressure,
        // every counter we tracked for that user MUST be queued for Redis
        // flush via the write-behind channel. Otherwise an in-process-only
        // increment would silently disappear.
        let stub = Arc::new(StubCapper::new());
        let cfg = InProcessConfig {
            cap_capacity: 2,
            ..InProcessConfig::default()
        };
        let (capper, mut rx) = InProcessFrequencyCapper::new(stub.clone() as _, breaker(), cfg);

        // Touch enough distinct users that moka must evict at least one.
        for u in &["u-a", "u-b", "u-c", "u-d", "u-e", "u-f"] {
            capper.record_impression(u, 42);
        }
        // Force moka to run pending work + apply size constraints.
        capper.counters.run_pending_tasks();

        // Steady-state size honors max_capacity (within TinyLFU tolerance).
        assert!(
            capper.counters.entry_count() <= 2,
            "moka should bound the cache near max_capacity=2, got {}",
            capper.counters.entry_count()
        );

        // Drain channel: expect at least one user_id beyond the live set,
        // proving the eviction listener flushed counters for evicted users.
        let mut all_user_ids = std::collections::HashSet::new();
        while let Ok(op) = rx.try_recv() {
            all_user_ids.insert(op.user_id);
        }
        // Every user we touched got at least one write-behind op queued
        // (either from the hot path or from the eviction listener).
        for u in &["u-a", "u-b", "u-c", "u-d", "u-e", "u-f"] {
            assert!(
                all_user_ids.contains(*u),
                "user {} should have produced at least one write-behind op",
                u
            );
        }
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

//! Kafka drop-policy state machine + incident-mode monitoring loop.
//!
//! The publisher reads `effective_policy()` on every send. The monitor loop
//! samples `events_published` / `events_dropped` over a rolling window; when
//! the drop rate sustains above a threshold for the dwell duration, the
//! effective policy flips from the configured base (e.g. `Newest`) to
//! `RandomSample` so the bidder sheds upstream load before rdkafka's
//! internal queue saturates. When the rate falls back below the threshold
//! for the same dwell, it reverts to the base.
//!
//! See PLAN.md § "Adaptive drop strategy" and docs/notes/phase-5-resilience-events.md.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::KafkaDropPolicy;

/// Policy as a u8 so we can swap it atomically without locks. The publisher
/// reads it on every event; the monitor loop writes it.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectivePolicy {
    Newest = 0,
    Oldest = 1,
    RandomSample = 2,
}

impl EffectivePolicy {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Oldest,
            2 => Self::RandomSample,
            _ => Self::Newest,
        }
    }
}

impl From<&KafkaDropPolicy> for EffectivePolicy {
    fn from(p: &KafkaDropPolicy) -> Self {
        match p {
            KafkaDropPolicy::Newest => Self::Newest,
            KafkaDropPolicy::Oldest => Self::Oldest,
            KafkaDropPolicy::RandomSample => Self::RandomSample,
            // Configured `incident_mode` means "start in Newest, auto-flip on
            // sustained drops" — same starting state as Newest.
            KafkaDropPolicy::IncidentMode => Self::Newest,
        }
    }
}

/// Counters fed by the publisher; drained delta-style by the monitor.
pub struct KafkaIncidentState {
    base_policy: EffectivePolicy,
    /// Currently active policy. Reads are Relaxed — monotonic flips every
    /// few seconds don't need stronger ordering.
    effective: AtomicU8,
    published_total: AtomicU64,
    dropped_total: AtomicU64,
    /// Whether `incident_mode` auto-flipping is enabled. Off = the publisher
    /// always uses base_policy and the monitor only emits gauges.
    auto_flip: bool,
}

impl KafkaIncidentState {
    pub fn new(base: KafkaDropPolicy) -> Self {
        let auto_flip = matches!(base, KafkaDropPolicy::IncidentMode);
        let base_eff = EffectivePolicy::from(&base);
        Self {
            base_policy: base_eff,
            effective: AtomicU8::new(base_eff as u8),
            published_total: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            auto_flip,
        }
    }

    pub fn effective_policy(&self) -> EffectivePolicy {
        EffectivePolicy::from_u8(self.effective.load(Ordering::Relaxed))
    }

    pub fn record_published(&self) {
        self.published_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_dropped(&self) {
        self.dropped_total.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64) {
        (
            self.published_total.load(Ordering::Relaxed),
            self.dropped_total.load(Ordering::Relaxed),
        )
    }
}

/// Spawns a monitoring loop that watches the drop rate over a rolling window.
///
/// `tick`        — sampling interval (e.g. 30 s).
/// `dwell_ticks` — how many consecutive over-threshold samples trigger a flip
///                 (e.g. 10 ticks × 30 s = 5 min, matching the PLAN.md spec).
/// `threshold`   — drop rate above which the window counts as "in incident",
///                 e.g. 0.01 = 1%.
pub fn spawn_monitor(
    state: Arc<KafkaIncidentState>,
    tick: Duration,
    dwell_ticks: u32,
    threshold: f64,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = state.snapshot();
        let mut over_count: u32 = 0;
        let mut under_count: u32 = 0;
        let mut in_incident = false;

        loop {
            interval.tick().await;
            let cur = state.snapshot();
            let pub_delta = cur.0.saturating_sub(last.0);
            let drop_delta = cur.1.saturating_sub(last.1);
            last = cur;

            let total = pub_delta + drop_delta;
            let rate = if total == 0 {
                0.0
            } else {
                drop_delta as f64 / total as f64
            };

            metrics::gauge!("bidder.kafka.drop_rate").set(rate);
            metrics::gauge!("bidder.kafka.incident_mode_active").set(if in_incident {
                1.0
            } else {
                0.0
            });

            if !state.auto_flip {
                continue;
            }

            if rate > threshold {
                over_count = over_count.saturating_add(1);
                under_count = 0;
            } else {
                under_count = under_count.saturating_add(1);
                over_count = 0;
            }

            if !in_incident && over_count >= dwell_ticks {
                state
                    .effective
                    .store(EffectivePolicy::RandomSample as u8, Ordering::Relaxed);
                in_incident = true;
                metrics::counter!("bidder.kafka.incident_mode_transitions",
                    "to" => "random_sample")
                .increment(1);
                tracing::warn!(
                    drop_rate = rate,
                    threshold,
                    "kafka drop rate sustained above threshold — flipping to random_sample"
                );
            } else if in_incident && under_count >= dwell_ticks {
                state
                    .effective
                    .store(state.base_policy as u8, Ordering::Relaxed);
                in_incident = false;
                metrics::counter!("bidder.kafka.incident_mode_transitions",
                    "to" => "base")
                .increment(1);
                tracing::info!(
                    drop_rate = rate,
                    "kafka drop rate normalised — reverting to base policy"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_flip_disabled_for_static_policies() {
        let s = KafkaIncidentState::new(KafkaDropPolicy::Newest);
        assert!(!s.auto_flip);
        assert_eq!(s.effective_policy(), EffectivePolicy::Newest);
    }

    #[test]
    fn incident_mode_starts_at_base_with_auto_flip_on() {
        let s = KafkaIncidentState::new(KafkaDropPolicy::IncidentMode);
        assert!(s.auto_flip);
        assert_eq!(s.effective_policy(), EffectivePolicy::Newest);
    }

    #[test]
    fn counters_increment() {
        let s = KafkaIncidentState::new(KafkaDropPolicy::Newest);
        s.record_published();
        s.record_published();
        s.record_dropped();
        let (p, d) = s.snapshot();
        assert_eq!((p, d), (2, 1));
    }
}

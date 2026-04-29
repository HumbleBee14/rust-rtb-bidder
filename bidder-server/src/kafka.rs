//! Kafka event publisher — Phase 8 BaseProducer + delivery-callback thread.
//!
//! Phase 5–7 used `FutureProducer::send().await` inside `tokio::spawn`. That
//! works but pays one tokio task allocation per published event and routes
//! backpressure through the spawn boundary instead of through rdkafka's own
//! bounded queue. Phase 8 collapses both:
//!
//!   - `BaseProducer` exposes `send()` as a synchronous, non-blocking call
//!     that just enqueues to librdkafka's internal C-level buffer and
//!     returns `Result<(), (KafkaError, BaseRecord)>` immediately. With
//!     `queue.full.behavior=error` this is the only place backpressure
//!     surfaces — exactly what the architecture promised.
//!
//!   - A single dedicated OS thread drives `producer.poll()` to fire the
//!     delivery callbacks (success/error) into our `ProducerContext`. The
//!     hot path never touches the broker; it's pure enqueue-then-return.
//!
//! Net effect at 50K RPS × 2 winners: zero tokio task allocations on the
//! event path (was ~100K/s of `tokio::spawn`); per-event overhead drops
//! to "build payload + push to bounded ring buffer".

use bidder_core::{
    config::KafkaConfig,
    events::EventPublisher,
    kafka_incident::{EffectivePolicy, KafkaIncidentState},
};
use bidder_protos::events::AdEvent;
use prost::Message;
use rdkafka::{
    error::KafkaError,
    producer::{BaseProducer, BaseRecord, DeliveryResult, Producer, ProducerContext},
    types::RDKafkaErrorCode,
    ClientConfig, ClientContext,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::warn;

/// In RandomSample mode we keep 1-in-N events. Counter-based (deterministic
/// and lock-free); avoids pulling in the `rand` crate just for this.
const RANDOM_SAMPLE_DENOMINATOR: u64 = 10;

/// Poll cadence for the delivery-callback thread. 100 ms is a balance:
/// short enough that delivery reports surface in the incident metrics
/// promptly (drop-rate calc runs on 30 s windows), long enough that the
/// thread stays cheap when traffic is idle.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Producer context captured by every produced record. rdkafka invokes
/// `delivery()` on the poll thread once the broker (or queue) reports the
/// message's outcome — that's where we feed the incident state.
pub struct PublisherContext {
    incident: Arc<KafkaIncidentState>,
}

impl ClientContext for PublisherContext {}

impl ProducerContext for PublisherContext {
    type DeliveryOpaque = ();

    fn delivery(&self, result: &DeliveryResult, _opaque: Self::DeliveryOpaque) {
        match result {
            Ok(_) => {
                self.incident.record_published();
            }
            Err((e, _)) => {
                self.incident.record_dropped();
                metrics::counter!("bidder.kafka.events_dropped",
                    "reason" => "kafka_error")
                .increment(1);
                warn!(error = %e, "kafka delivery failed");
            }
        }
    }
}

pub struct KafkaEventPublisher {
    producer: Arc<BaseProducer<PublisherContext>>,
    incident: Arc<KafkaIncidentState>,
    sample_counter: AtomicU64,
    shutdown: Arc<AtomicBool>,
    poll_thread: Option<JoinHandle<()>>,
}

impl KafkaEventPublisher {
    pub fn new(cfg: &KafkaConfig, incident: Arc<KafkaIncidentState>) -> anyhow::Result<Self> {
        // Empty brokers is a deliberate "disable Kafka" signal. Used by the
        // baseline harness and dev runs that don't have a broker. Returning an
        // error here lets `main.rs` fall through to NoOpEventPublisher without
        // librdkafka spinning up retry threads on a dead address.
        if cfg.brokers.trim().is_empty() {
            anyhow::bail!("kafka.brokers is empty — Kafka publisher disabled by config");
        }

        let context = PublisherContext {
            incident: Arc::clone(&incident),
        };
        let producer: BaseProducer<PublisherContext> = ClientConfig::new()
            .set("bootstrap.servers", &cfg.brokers)
            .set("message.timeout.ms", cfg.send_timeout_ms.to_string())
            // Internal queue bounded to queue_capacity messages; overflow drops newest.
            .set(
                "queue.buffering.max.messages",
                cfg.queue_capacity.to_string(),
            )
            .set("queue.buffering.max.ms", "5")
            // CRITICAL: librdkafka defaults to "block" when the producer queue
            // fills. With BaseProducer + "error", `send()` returns
            // QueueFull(MessageProduction) immediately so the publisher records
            // a drop and the bid path doesn't pay any backpressure cost.
            .set("queue.full.behavior", "error")
            .create_with_context(context)?;
        let producer = Arc::new(producer);

        // Dedicated OS thread drives delivery callbacks via poll(). Std
        // thread (not tokio) because the only thing it does is hand control
        // to librdkafka — there's no async work, and we don't want it
        // sharing the runtime workers that serve bid traffic.
        let shutdown = Arc::new(AtomicBool::new(false));
        let poll_producer = Arc::clone(&producer);
        let poll_shutdown = Arc::clone(&shutdown);
        let poll_thread = thread::Builder::new()
            .name("kafka-poll".to_string())
            .spawn(move || {
                while !poll_shutdown.load(Ordering::Relaxed) {
                    poll_producer.poll(POLL_INTERVAL);
                }
                // Final drain on shutdown so any in-flight messages get
                // their delivery callbacks before the producer drops.
                poll_producer.flush(Duration::from_secs(5)).ok();
            })?;

        Ok(Self {
            producer,
            incident,
            sample_counter: AtomicU64::new(0),
            shutdown,
            poll_thread: Some(poll_thread),
        })
    }
}

impl Drop for KafkaEventPublisher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.poll_thread.take() {
            // Best-effort join — if the thread is wedged on librdkafka, we
            // don't want process shutdown to hang on it.
            let _ = handle.join();
        }
    }
}

impl EventPublisher for KafkaEventPublisher {
    #[tracing::instrument(
        name = "kafka.produce",
        skip(self, key, event),
        fields(
            messaging.system = "kafka",
            messaging.destination = topic,
        )
    )]
    fn publish(&self, topic: &str, key: &[u8], event: AdEvent) {
        // RandomSample mode (set by the incident monitor when sustained drop
        // rate breaches the threshold): drop 9 of every 10 events upstream
        // before they hit rdkafka's queue. Reduces downstream pressure by an
        // order of magnitude while preserving incident visibility.
        if matches!(
            self.incident.effective_policy(),
            EffectivePolicy::RandomSample
        ) {
            let n = self.sample_counter.fetch_add(1, Ordering::Relaxed);
            if n % RANDOM_SAMPLE_DENOMINATOR != 0 {
                self.incident.record_dropped();
                metrics::counter!("bidder.kafka.events_dropped",
                    "reason" => "incident_sample")
                .increment(1);
                return;
            }
        }

        let payload = event.encode_to_vec();
        let record: BaseRecord<'_, [u8], [u8], ()> =
            BaseRecord::to(topic).key(key).payload(&payload);

        // BaseProducer::send is non-blocking: pushes onto the internal queue
        // and returns. Delivery success/error surfaces later via the
        // delivery() callback on the poll thread.
        match self.producer.send(record) {
            Ok(()) => {
                // Publish-success accounting happens in the delivery callback
                // (i.e. the actual broker ack), not here. record_published()
                // is intentionally NOT called on enqueue success.
            }
            Err((KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull), _record)) => {
                self.incident.record_dropped();
                metrics::counter!("bidder.kafka.events_dropped",
                    "reason" => "queue_full")
                .increment(1);
            }
            Err((e, _record)) => {
                self.incident.record_dropped();
                metrics::counter!("bidder.kafka.events_dropped",
                    "reason" => "enqueue_error")
                .increment(1);
                warn!(error = %e, topic, "kafka enqueue failed");
            }
        }
    }
}

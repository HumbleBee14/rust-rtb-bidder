use async_trait::async_trait;
use bidder_core::{
    config::KafkaConfig,
    events::EventPublisher,
    kafka_incident::{EffectivePolicy, KafkaIncidentState},
};
use bidder_protos::events::AdEvent;
use prost::Message;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    ClientConfig,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// In RandomSample mode we keep 1-in-N events. Counter-based (deterministic
/// and lock-free); avoids pulling in the `rand` crate just for this.
const RANDOM_SAMPLE_DENOMINATOR: u64 = 10;

pub struct KafkaEventPublisher {
    producer: FutureProducer,
    send_timeout: Duration,
    incident: Arc<KafkaIncidentState>,
    sample_counter: AtomicU64,
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
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &cfg.brokers)
            .set("message.timeout.ms", cfg.send_timeout_ms.to_string())
            // Internal queue bounded to queue_capacity messages; overflow drops newest.
            .set(
                "queue.buffering.max.messages",
                cfg.queue_capacity.to_string(),
            )
            .set("queue.buffering.max.ms", "5")
            // CRITICAL: librdkafka defaults to "block" when the producer queue
            // fills, which would stall the tokio task awaiting send() and —
            // through backpressure on the spawn site — risk leaking into bid
            // latency. "error" makes send() return QueueFull immediately so
            // the publisher records a drop and moves on. Phase 5 invariant:
            // Kafka slowness must never propagate to the bid path.
            .set("queue.full.behavior", "error")
            .create()?;

        Ok(Self {
            producer,
            send_timeout: Duration::from_millis(cfg.send_timeout_ms),
            incident,
            sample_counter: AtomicU64::new(0),
        })
    }
}

#[async_trait]
impl EventPublisher for KafkaEventPublisher {
    #[tracing::instrument(
        name = "kafka.produce",
        skip(self, key, event),
        fields(
            messaging.system = "kafka",
            messaging.destination = topic,
        )
    )]
    // publish awaits the rdkafka delivery future up to send_timeout, but callers always
    // invoke this inside tokio::spawn — the bid path itself never awaits it.
    async fn publish(&self, topic: &str, key: &[u8], event: AdEvent) {
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
        let record = FutureRecord::to(topic).key(key).payload(&payload);

        match self.producer.send(record, self.send_timeout).await {
            Ok(_) => {
                self.incident.record_published();
            }
            Err((e, _)) => {
                self.incident.record_dropped();
                metrics::counter!("bidder.kafka.events_dropped",
                    "reason" => "kafka_error")
                .increment(1);
                warn!(error = %e, topic, "kafka event dropped");
            }
        }
    }
}

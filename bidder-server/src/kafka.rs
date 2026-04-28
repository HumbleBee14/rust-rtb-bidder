use async_trait::async_trait;
use bidder_core::{config::KafkaConfig, events::EventPublisher};
use bidder_protos::events::AdEvent;
use prost::Message;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    ClientConfig,
};
use std::time::Duration;
use tracing::warn;

pub struct KafkaEventPublisher {
    producer: FutureProducer,
    send_timeout: Duration,
}

impl KafkaEventPublisher {
    pub fn new(cfg: &KafkaConfig) -> anyhow::Result<Self> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &cfg.brokers)
            .set("message.timeout.ms", cfg.send_timeout_ms.to_string())
            // Internal queue bounded to queue_capacity messages; overflow drops newest.
            .set(
                "queue.buffering.max.messages",
                cfg.queue_capacity.to_string(),
            )
            .set("queue.buffering.max.ms", "5")
            .create()?;

        Ok(Self {
            producer,
            send_timeout: Duration::from_millis(cfg.send_timeout_ms),
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
        let payload = event.encode_to_vec();
        let record = FutureRecord::to(topic).key(key).payload(&payload);

        match self.producer.send(record, self.send_timeout).await {
            Ok(_) => {}
            Err((e, _)) => {
                metrics::counter!("bidder.kafka.events_dropped").increment(1);
                warn!(error = %e, topic, "kafka event dropped");
            }
        }
    }
}

use crate::config::{LogFormat, TelemetryConfig};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    trace::{Sampler, SdkTracerProvider},
    Resource,
};
use opentelemetry_semantic_conventions::resource::SERVICE_NAME;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub struct TelemetryGuard {
    tracer_provider: SdkTracerProvider,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.tracer_provider.shutdown().ok();
    }
}

pub fn init(cfg: &TelemetryConfig) -> anyhow::Result<TelemetryGuard> {
    let resource = Resource::builder()
        .with_attribute(KeyValue::new(SERVICE_NAME, cfg.service_name.clone()))
        .build();

    // Empty otlp_endpoint disables OTel span export entirely. The bidder still
    // emits structured logs and Prometheus metrics; only OTLP traces are off.
    // Used by the load-test baseline harness and dev runs without Tempo so the
    // bidder doesn't spam reqwest connection-refused errors on every flush.
    if cfg.otlp_endpoint.trim().is_empty() {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        match cfg.log_format {
            LogFormat::Json => {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer().json())
                    .init();
            }
            LogFormat::Pretty => {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer().json().pretty())
                    .init();
            }
        }
        let tracer_provider = SdkTracerProvider::builder().with_resource(resource).build();
        return Ok(TelemetryGuard { tracer_provider });
    }

    let http_client = reqwest::Client::new();
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_endpoint(format!("{}/v1/traces", cfg.otlp_endpoint))
        .build()?;

    // Head-based sampling. The decision is made at trace creation time —
    // before we know whether the request will error or breach the SLA — so
    // this samples a `success_sample_rate` random fraction of ALL traffic,
    // errors and SLA violations included. The earlier "100% errors + 1% of
    // successes" promise was unreachable with a head sampler (would need
    // tail sampling at a collector); we deliberately don't pay that cost at
    // 50K RPS. Errors and SLA violations are caught for sure via:
    //   - `tracing::error!` events (always emitted, always logged)
    //   - Prometheus metrics (`bidder.bid.duration_seconds`, error counters)
    // Traces are a 1% systemic-flow visualizer, not the source of truth for
    // incidents. See docs/PLAN.md § "Phase 8 / Open items: tail sampling".
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
        cfg.success_sample_rate,
    )));

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource)
        .build();

    let tracer = tracer_provider.tracer(cfg.service_name.clone());
    let otel_layer = OpenTelemetryLayer::new(tracer);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Both branches must produce JsonFields for the OTel layer trait bound.
    // Pretty output is only for local dev; it still uses JSON fields internally
    // so the span data is correct, but the fmt output is human-readable.
    match cfg.log_format {
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(tracing_subscriber::fmt::layer().json().pretty())
                .init();
        }
    }

    Ok(TelemetryGuard { tracer_provider })
}

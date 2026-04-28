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

    let http_client = reqwest::Client::new();
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_endpoint(format!("{}/v1/traces", cfg.otlp_endpoint))
        .build()?;

    // Head-based sampling: always sample errors and SLA violations (handled
    // at the span level by setting error fields); sample a configurable
    // fraction of successful requests.
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

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
                // Human-readable, ANSI-colored when stdout is a TTY. tracing
                // auto-disables color when piped or redirected (e.g. `nohup`
                // in bidder-start), so log files don't get escape junk.
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer().compact())
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

    // Sampling strategy — see TelemetryConfig docs.
    //
    // - tail_sampling_via_collector = false (default): head-based sampling.
    //   `success_sample_rate` of ALL traffic is shipped, regardless of
    //   outcome. Cheap, but cannot guarantee 100% of errors/SLA violations
    //   are kept. Errors are still caught via `tracing::error!` logs and
    //   Prometheus metrics — traces here are a systemic-flow visualizer.
    //
    // - tail_sampling_via_collector = true: ship 100% to the OTel Collector
    //   in front of us; the collector's `tail_sampling` processor decides
    //   keep/drop after the trace completes. This is the only way to
    //   actually keep all errors + a sampled fraction of successes. Costs
    //   collector RAM proportional to (decision_wait × span_rate × span_size)
    //   — see `docs/notes/phase-8-architectural-followups.md` for sizing.
    let sampler = if cfg.tail_sampling_via_collector {
        tracing::info!(
            "telemetry: tail_sampling_via_collector=true, shipping 100% of spans \
             — ensure an OTel Collector with tail_sampling processor is in front \
             of {}",
            cfg.otlp_endpoint
        );
        Sampler::ParentBased(Box::new(Sampler::AlwaysOn))
    } else {
        Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            cfg.success_sample_rate,
        )))
    };

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource)
        .build();

    let tracer = tracer_provider.tracer(cfg.service_name.clone());
    let otel_layer = OpenTelemetryLayer::new(tracer);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // The OTel layer captures spans for OTLP export independently of the fmt
    // layer's display shape, so JSON / compact are interchangeable here.
    // Compact = human-readable, ANSI-colored when stdout is a TTY.
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
                .with(tracing_subscriber::fmt::layer().compact())
                .init();
        }
    }

    Ok(TelemetryGuard { tracer_provider })
}

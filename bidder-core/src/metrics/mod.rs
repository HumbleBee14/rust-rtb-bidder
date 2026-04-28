use crate::config::MetricsConfig;
use metrics_exporter_prometheus::PrometheusBuilder;

pub fn init(cfg: &MetricsConfig) -> anyhow::Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(cfg.bind)
        .install()?;
    Ok(())
}

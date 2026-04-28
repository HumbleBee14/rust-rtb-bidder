use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub telemetry: TelemetryConfig,
    pub metrics: MetricsConfig,
    pub latency_budget: LatencyBudgetConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub max_concurrency: usize,
    pub warmup_enabled: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelemetryConfig {
    pub otlp_endpoint: String,
    pub success_sample_rate: f64,
    pub log_format: LogFormat,
    pub service_name: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MetricsConfig {
    pub bind: SocketAddr,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LatencyBudgetConfig {
    pub http_parse_ms: u64,
    pub request_validate_ms: u64,
    pub user_enrichment_ms: u64,
    pub candidate_retrieval_ms: u64,
    pub candidate_limit_ms: u64,
    pub scoring_ms: u64,
    pub frequency_cap_ms: u64,
    pub ranking_ms: u64,
    pub budget_pacing_ms: u64,
    pub response_build_ms: u64,
    pub pipeline_deadline_ms: u64,
    pub http_timeout_ms: u64,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let config = Figment::new()
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("BIDDER__").split("__"))
            .extract()?;
        Ok(config)
    }
}

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
    pub postgres: PostgresConfig,
    pub redis: RedisConfig,
    pub catalog: CatalogConfig,
    pub pipeline: PipelineConfig,
    pub freq_cap: FreqCapConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PostgresConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout_ms: u64,
    pub idle_timeout_secs: u64,
    pub max_lifetime_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RedisConfig {
    pub url: String,
    /// Number of connections in the round-robin pool.
    /// Start with num_cpus; tune down if profiling shows pool overhead > decode win.
    pub pool_size: usize,
    /// User-segment cache: capacity in entries.
    pub segment_cache_capacity: u64,
    /// User-segment cache: TTL in seconds.
    pub segment_cache_ttl_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CatalogConfig {
    /// Full catalog rebuild interval in seconds.
    pub refresh_interval_secs: u64,
    /// Max consecutive rebuild failures before the circuit opens and an alert fires.
    pub max_consecutive_failures: u32,
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

#[derive(Debug, Deserialize, Clone)]
pub struct PipelineConfig {
    /// Maximum candidates kept per impression after CandidateLimitStage.
    pub max_candidates_per_imp: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FreqCapConfig {
    /// Number of concurrent Redis EVAL workers for impression counter writes.
    pub impression_workers: usize,
}

impl LatencyBudgetConfig {
    /// Returns the declared budget (ms) for a named pipeline stage, or None
    /// if the stage name doesn't map to a known budget entry.
    pub fn budget_for_stage(&self, name: &str) -> Option<u64> {
        match name {
            "request_validation" => Some(self.request_validate_ms),
            "user_enrichment" => Some(self.user_enrichment_ms),
            "candidate_retrieval" => Some(self.candidate_retrieval_ms),
            "candidate_limit" => Some(self.candidate_limit_ms),
            "scoring" => Some(self.scoring_ms),
            "frequency_cap" => Some(self.frequency_cap_ms),
            "ranking" => Some(self.ranking_ms),
            "budget_pacing" => Some(self.budget_pacing_ms),
            "response_build" => Some(self.response_build_ms),
            _ => None,
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from("config.toml")
    }

    pub fn load_from(path: &str) -> anyhow::Result<Self> {
        let config = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("BIDDER__").split("__"))
            .extract()?;
        Ok(config)
    }
}

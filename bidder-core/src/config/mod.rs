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
    pub kafka: KafkaConfig,
    pub win_notice: WinNoticeConfig,
    pub scoring: ScoringConfig,
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
    /// Timeout for the /rtb/win notice endpoint. Longer than bid path; win-notices
    /// are not latency-critical and should not be dropped under Kafka backpressure.
    pub win_timeout_ms: u64,
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

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum KafkaDropPolicy {
    Newest,
    Oldest,
    RandomSample,
    IncidentMode,
}

/// CONTRACT: docs/SCORING-FEATURES.md § Appendix A.
///
/// Selects which scorer the pipeline runs. Each variant's sub-section is parsed
/// only when the matching `kind` is active. Adding a new scorer is one new
/// enum variant + one new struct + one wire-up in main.rs.
#[derive(Debug, Deserialize, Clone)]
pub struct ScoringConfig {
    pub kind: ScoringKind,
    /// Loaded only when `kind = "ml"` or any cascade/ab_test that nests ml.
    #[serde(default)]
    pub ml: Option<MLConfig>,
    /// Loaded only when `kind = "cascade"`.
    #[serde(default)]
    pub cascade: Option<CascadeConfig>,
    /// Loaded only when `kind = "ab_test"`.
    #[serde(default)]
    pub ab_test: Option<ABTestConfig>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScoringKind {
    FeatureWeighted,
    Ml,
    Cascade,
    AbTest,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MLConfig {
    pub model_path: String,
    #[serde(default)]
    pub parity_path: Option<String>,
    #[serde(default = "default_input_tensor_name")]
    pub input_tensor_name: String,
    #[serde(default = "default_output_tensor_name")]
    pub output_tensor_name: String,
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,
    #[serde(default = "default_batch_pad_to")]
    pub batch_pad_to: usize,
}

fn default_input_tensor_name() -> String {
    "features".to_string()
}
fn default_output_tensor_name() -> String {
    "pctr".to_string()
}
fn default_pool_size() -> usize {
    2
}
fn default_max_batch() -> usize {
    64
}
fn default_batch_pad_to() -> usize {
    8
}

#[derive(Debug, Deserialize, Clone)]
pub struct CascadeConfig {
    /// Names of registered scorers, e.g. `"feature_weighted"` and `"ml"`.
    pub stage1: ScoringKind,
    pub stage2: ScoringKind,
    /// Top-K survivors of stage 1 advance to stage 2. 0 disables the cap.
    #[serde(default)]
    pub top_k: usize,
    /// Minimum stage-1 score required for stage 2 advancement. 0 disables.
    #[serde(default)]
    pub threshold: f32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ABTestConfig {
    pub control: ScoringKind,
    pub treatment: ScoringKind,
    /// Treatment share in [0.0, 1.0]; 0.10 = 10% routed to treatment.
    pub treatment_share: f32,
    /// Disambiguates concurrent experiments; prefixes the user-id hash input.
    pub hash_seed: String,
}

/// Win-notice authentication and deduplication.
///
/// Authentication: every `nurl` we emit carries `token=HMAC-SHA256(message, secret)`
/// where `message = request_id|imp_id|campaign_id|creative_id|clearing_price_micros`.
/// The handler recomputes the HMAC and rejects with 401 on mismatch.
///
/// Deduplication: before any side effect (freq-cap INCR, Kafka publish), the handler
/// runs `SET v1:winx:<request_id>:<imp_id> 1 NX EX <dedup_ttl_secs>`. If the key
/// already exists (duplicate notice from the SSP, CDN retry, or replay), we return
/// 200 OK without re-incrementing.
///
/// Secret rotation: the HMAC secret is supplied via env var `BIDDER__WIN_NOTICE__SECRET`
/// and read at startup. Rotation is a redeploy. Per-SSP secrets are deferred to the
/// multi-exchange phase (Phase 7).
#[derive(Debug, Deserialize, Clone)]
pub struct WinNoticeConfig {
    /// Whether HMAC verification is enforced. Set false only for early-stage
    /// SSP integrations that are still wiring up token generation.
    pub require_auth: bool,
    /// HMAC-SHA256 shared secret. Loaded from env, never from TOML in production.
    /// Empty string disables auth even when require_auth=true (logged at startup).
    pub secret: String,
    /// Dedup window in seconds. 3600 (1h) covers the SSP retry window plus the
    /// shortest freq-cap window so duplicates can't double-count within a cap period.
    pub dedup_ttl_secs: u64,
    /// Base URL the bidder embeds in each bid's `nurl`. The win-notice path and
    /// query string are appended by the bidder. Empty disables nurl emission
    /// (useful for integration tests). Example: `https://bid.example.com/rtb/win`.
    #[serde(default)]
    pub notice_base_url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KafkaConfig {
    pub brokers: String,
    /// Topic for all AdEvent messages.
    pub events_topic: String,
    /// rdkafka internal producer queue size (number of messages).
    pub queue_capacity: usize,
    /// Producer send timeout in milliseconds (background; never blocks bid path).
    pub send_timeout_ms: u64,
    pub drop_policy: KafkaDropPolicy,
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

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BidderError {
    #[error("configuration error: {0}")]
    Config(#[from] figment::Error),

    #[error("telemetry init error: {0}")]
    Telemetry(#[from] opentelemetry_sdk::trace::TraceError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

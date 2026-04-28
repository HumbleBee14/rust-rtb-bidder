mod impression_recorder;
pub use impression_recorder::{ImpressionEvent, ImpressionRecorder};

use crate::model::candidate::AdCandidate;
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapWindow {
    Hour,
    Day,
    Week,
}

impl CapWindow {
    pub fn suffix(&self) -> &'static str {
        match self {
            Self::Hour => "h",
            Self::Day => "d",
            Self::Week => "w",
        }
    }
}

/// Result of checking a single candidate against freq caps.
#[derive(Debug, Clone)]
pub struct CapResult {
    pub campaign_id: u32,
    /// true = this candidate is capped and should be filtered out.
    pub capped: bool,
}

/// Reads frequency cap counters for a user and filters candidates.
///
/// All cap reads for one user are issued as a single MGET (one round-trip).
/// If the MGET exceeds the timeout budget, returns `SkippedTimeout` — the
/// caller should proceed without freq enforcement rather than blocking the bid.
#[async_trait]
pub trait FrequencyCapper: Send + Sync + 'static {
    /// Check freq caps for all candidates for this user.
    /// Returns per-candidate results, or None if freq-cap was skipped due to timeout.
    async fn check(
        &self,
        user_id: &str,
        candidates: &[AdCandidate],
        device_type_val: u8,
        hour_of_day: u8,
    ) -> FreqCapOutcome;
}

#[derive(Debug)]
pub enum FreqCapOutcome {
    /// Freq cap check completed; per-candidate results.
    Checked(Vec<CapResult>),
    /// MGET timed out — caller should proceed without filtering.
    SkippedTimeout,
    /// No user ID available — skip silently.
    SkippedNoUser,
}

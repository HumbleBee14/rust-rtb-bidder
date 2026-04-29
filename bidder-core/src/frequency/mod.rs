mod impression_recorder;
mod in_process;
pub use impression_recorder::{ImpressionEvent, ImpressionRecorder};
pub use in_process::{InProcessConfig, InProcessFrequencyCapper, WriteBehindOp};

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
///
/// `day_count` / `hour_count` are populated when the verdict came from a
/// source that knows the underlying counter values (e.g. RedisFrequencyCapper
/// reads them as part of computing `capped`). When populated they let an
/// upstream layer (InProcessFrequencyCapper) cache the verdict at full
/// fidelity instead of just remembering the boolean. `None` when the source
/// only knows the boolean (e.g. in-process direct hit, where the counts
/// already live in moka and weren't separately surfaced).
#[derive(Debug, Clone)]
pub struct CapResult {
    pub campaign_id: u32,
    /// true = this candidate is capped and should be filtered out.
    pub capped: bool,
    pub day_count: Option<i64>,
    pub hour_count: Option<i64>,
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

mod local;
pub use local::LocalBudgetPacer;

use crate::catalog::CampaignId;
use async_trait::async_trait;

/// Budget pacing decision for a single candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceDecision {
    /// Campaign has budget; bid price in cents.
    HasBudget,
    /// Campaign is exhausted for this pacing window.
    Exhausted,
}

/// Controls whether a campaign is eligible to bid based on remaining budget.
///
/// `LocalBudgetPacer` — single-instance in-process `AtomicI64` per campaign.
/// `DistributedBudgetPacer` (Phase 5+) — Redis DECRBY against `v1:bud:{c:<id>}`.
#[async_trait]
pub trait BudgetPacer: Send + Sync + 'static {
    /// Check if campaign has remaining budget and reserve it.
    /// Atomic: if HasBudget, the budget has already been decremented.
    async fn check_and_reserve(&self, campaign_id: CampaignId, bid_cents: i32) -> PaceDecision;

    /// Release a previously reserved budget (on a lost bid or error).
    async fn release(&self, campaign_id: CampaignId, bid_cents: i32);

    /// Reload budget values from source of truth (called by catalog refresh task).
    async fn reload(&self, budgets: Vec<(CampaignId, i64)>);
}

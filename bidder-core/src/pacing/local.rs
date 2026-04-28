use super::{BudgetPacer, PaceDecision};
use crate::catalog::CampaignId;
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

/// In-process budget pacer using `i64` counters (in cents).
///
/// Single-instance deployment only. For multi-instance, DistributedBudgetPacer
/// uses Redis DECRBY against `v1:bud:{c:<id>}:d`.
///
/// Budgets start at `daily_budget_cents` from the campaign catalog and are
/// decremented atomically. A campaign with a missing entry is treated as
/// having no budget (conservative — prevents accidental over-spend on
/// misconfiguration). Reload is called by the catalog refresh task.
///
/// Thread-safety: `RwLock<HashMap>` — reads are concurrent, reloads are rare.
/// The hot path (check_and_reserve) uses a read lock to get the Arc<AtomicI64>
/// then does an atomic fetch_sub — no write contention on the map during bidding.
pub struct LocalBudgetPacer {
    // campaign_id → remaining budget cents (can go negative = overspent by in-flight bids)
    counters: RwLock<HashMap<CampaignId, Arc<std::sync::atomic::AtomicI64>>>,
}

impl LocalBudgetPacer {
    pub fn new() -> Self {
        Self {
            counters: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for LocalBudgetPacer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BudgetPacer for LocalBudgetPacer {
    async fn check_and_reserve(&self, campaign_id: CampaignId, bid_cents: i32) -> PaceDecision {
        let counters = self.counters.read().await;
        match counters.get(&campaign_id) {
            None => {
                metrics::counter!("bidder.budget.missing_key").increment(1);
                tracing::warn!(
                    campaign_id,
                    "no budget entry for campaign — treating as exhausted"
                );
                PaceDecision::Exhausted
            }
            Some(counter) => {
                use std::sync::atomic::Ordering;
                let prev = counter.fetch_sub(bid_cents as i64, Ordering::AcqRel);
                if prev <= 0 {
                    // Already exhausted before this decrement; restore.
                    counter.fetch_add(bid_cents as i64, Ordering::AcqRel);
                    PaceDecision::Exhausted
                } else {
                    PaceDecision::HasBudget
                }
            }
        }
    }

    async fn release(&self, campaign_id: CampaignId, bid_cents: i32) {
        use std::sync::atomic::Ordering;
        let counters = self.counters.read().await;
        if let Some(counter) = counters.get(&campaign_id) {
            counter.fetch_add(bid_cents as i64, Ordering::AcqRel);
        }
    }

    async fn reload(&self, budgets: Vec<(CampaignId, i64)>) {
        use std::sync::atomic::Ordering;
        let mut counters = self.counters.write().await;
        for (id, budget_cents) in budgets {
            counters
                .entry(id)
                .and_modify(|c| c.store(budget_cents, Ordering::Release))
                .or_insert_with(|| Arc::new(std::sync::atomic::AtomicI64::new(budget_cents)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pacer_with(budgets: Vec<(CampaignId, i64)>) -> LocalBudgetPacer {
        let p = LocalBudgetPacer::new();
        p.reload(budgets).await;
        p
    }

    #[tokio::test]
    async fn has_budget_decrements_counter() {
        let p = pacer_with(vec![(1, 500)]).await;
        assert_eq!(p.check_and_reserve(1, 100).await, PaceDecision::HasBudget);
        assert_eq!(p.check_and_reserve(1, 100).await, PaceDecision::HasBudget);
        assert_eq!(p.check_and_reserve(1, 300).await, PaceDecision::HasBudget);
    }

    #[tokio::test]
    async fn exhausted_when_budget_zero() {
        let p = pacer_with(vec![(1, 0)]).await;
        assert_eq!(p.check_and_reserve(1, 1).await, PaceDecision::Exhausted);
    }

    #[tokio::test]
    async fn allows_one_overshoot_then_blocks() {
        // Design: pacer checks prev > 0 (was there budget?), not prev >= bid_cents.
        // One bid can overshoot; next bid when counter is negative is blocked.
        let p = pacer_with(vec![(1, 50)]).await;
        assert_eq!(p.check_and_reserve(1, 100).await, PaceDecision::HasBudget);
        assert_eq!(p.check_and_reserve(1, 1).await, PaceDecision::Exhausted);
    }

    #[tokio::test]
    async fn missing_campaign_is_exhausted() {
        let p = LocalBudgetPacer::new();
        assert_eq!(p.check_and_reserve(99, 10).await, PaceDecision::Exhausted);
    }

    #[tokio::test]
    async fn release_restores_budget() {
        // Reserve 100 from budget=100 → counter=0; next bid sees prev=0 → Exhausted
        let p = pacer_with(vec![(1, 100)]).await;
        p.check_and_reserve(1, 100).await;
        assert_eq!(p.check_and_reserve(1, 1).await, PaceDecision::Exhausted);
        p.release(1, 100).await;
        assert_eq!(p.check_and_reserve(1, 100).await, PaceDecision::HasBudget);
    }

    #[tokio::test]
    async fn reload_updates_existing_budget() {
        let p = pacer_with(vec![(1, 100)]).await;
        p.check_and_reserve(1, 100).await;
        p.reload(vec![(1, 500)]).await;
        assert_eq!(p.check_and_reserve(1, 500).await, PaceDecision::HasBudget);
    }
}

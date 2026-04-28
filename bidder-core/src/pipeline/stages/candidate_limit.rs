use crate::{
    model::{candidate::AdCandidate, context::BidContext},
    pipeline::stage::Stage,
};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use tracing::instrument;

/// Trims each impression's candidate list to at most `top_k` entries.
///
/// Uses a min-heap keyed by bid_price_cents so the heap ejects the cheapest
/// candidate when full — keeping the K highest-priced candidates for scoring.
/// Score is 0.0 at this point; pre-filtering by price avoids scoring cheap
/// campaigns that can't win. The final ranking uses score, not price.
///
/// Why BinaryHeap<Reverse<_>>: Rust's BinaryHeap is a max-heap. Wrapping in
/// Reverse flips the ordering so `pop()` returns the minimum — the standard
/// Rust idiom for a min-heap.
pub struct CandidateLimitStage {
    pub top_k: usize,
}

impl Stage for CandidateLimitStage {
    fn name(&self) -> &'static str {
        "candidate_limit"
    }

    #[instrument(name = "stage.candidate_limit", skip(self, ctx), fields(request_id = %ctx.request.id, top_k = self.top_k))]
    async fn execute<'a>(&'a self, ctx: &'a mut BidContext) -> anyhow::Result<()> {
        for candidates in ctx.candidates.iter_mut() {
            if candidates.len() <= self.top_k {
                continue;
            }

            // Build a min-heap of capacity top_k.
            // Keyed by (bid_price_cents, campaign_id) for stable ordering.
            let mut heap: BinaryHeap<Reverse<HeapEntry>> =
                BinaryHeap::with_capacity(self.top_k + 1);

            for candidate in candidates.drain(..) {
                let entry = HeapEntry {
                    price: candidate.bid_price_cents,
                    campaign_id: candidate.campaign_id,
                    candidate,
                };
                heap.push(Reverse(entry));
                if heap.len() > self.top_k {
                    heap.pop(); // remove the cheapest
                }
            }

            *candidates = heap.into_iter().map(|Reverse(e)| e.candidate).collect();
        }
        Ok(())
    }
}

/// Heap entry that orders by (price desc, campaign_id asc) for deterministic
/// tie-breaking. Reverse wrapping in the heap makes this a min-heap by price.
struct HeapEntry {
    price: i32,
    campaign_id: u32,
    candidate: AdCandidate,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.price == other.price && self.campaign_id == other.campaign_id
    }
}
impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher price = greater. Tie-break by campaign_id ascending.
        self.price
            .cmp(&other.price)
            .then(other.campaign_id.cmp(&self.campaign_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::openrtb::BidRequest, pipeline::stage::Stage};

    fn make_ctx(candidates: Vec<AdCandidate>) -> crate::model::context::BidContext {
        let req: BidRequest = serde_json::from_str(r#"{"id":"t","imp":[{"id":"i1"}]}"#).unwrap();
        let mut ctx = crate::model::context::BidContext::new(req);
        ctx.candidates = vec![candidates];
        ctx
    }

    fn candidate(campaign_id: u32, price: i32) -> AdCandidate {
        AdCandidate {
            campaign_id,
            creative_id: 1,
            bid_price_cents: price,
            score: 0.0,
        }
    }

    #[tokio::test]
    async fn keeps_top_k_by_price() {
        let stage = CandidateLimitStage { top_k: 3 };
        let mut ctx = make_ctx(vec![
            candidate(1, 10),
            candidate(2, 50),
            candidate(3, 30),
            candidate(4, 80),
            candidate(5, 20),
        ]);
        stage.execute(&mut ctx).await.unwrap();
        let mut prices: Vec<i32> = ctx.candidates[0]
            .iter()
            .map(|c| c.bid_price_cents)
            .collect();
        prices.sort_unstable();
        assert_eq!(prices, vec![30, 50, 80]);
    }

    #[tokio::test]
    async fn noop_when_under_limit() {
        let stage = CandidateLimitStage { top_k: 10 };
        let mut ctx = make_ctx(vec![candidate(1, 100), candidate(2, 200)]);
        stage.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.candidates[0].len(), 2);
    }

    #[tokio::test]
    async fn noop_on_empty() {
        let stage = CandidateLimitStage { top_k: 5 };
        let mut ctx = make_ctx(vec![]);
        stage.execute(&mut ctx).await.unwrap();
        assert_eq!(ctx.candidates[0].len(), 0);
    }
}

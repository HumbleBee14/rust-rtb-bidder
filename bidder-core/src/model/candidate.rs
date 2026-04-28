use crate::catalog::{CampaignId, CreativeId};

/// A campaign+creative pair that has passed candidate retrieval.
/// Carried through scoring, freq-cap, and ranking stages.
#[derive(Debug, Clone)]
pub struct AdCandidate {
    pub campaign_id: CampaignId,
    pub creative_id: CreativeId,
    /// Bid price in cents (from campaign.bid_floor_cents, adjusted by pacer).
    pub bid_price_cents: i32,
    /// Score assigned by the configured Scorer (FeatureWeighted, ML, or Cascade).
    /// 0.0 until scored.
    pub score: f32,
    /// 24h per-user impression cap loaded from catalog.
    /// 0 = block after first impression. Populated at candidate retrieval.
    pub daily_cap_imps: u32,
    /// 1h per-user impression cap loaded from catalog. Same semantics.
    pub hourly_cap_imps: u32,
}

/// The single winner selected per impression after ranking.
#[derive(Debug, Clone)]
pub struct ImpWinner {
    pub imp_id: String,
    pub campaign_id: CampaignId,
    pub creative_id: CreativeId,
    pub bid_price_cents: i32,
    pub score: f32,
}

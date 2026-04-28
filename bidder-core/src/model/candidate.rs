use crate::catalog::{CampaignId, CreativeId};

/// A campaign+creative pair that has passed candidate retrieval.
/// Carried through scoring, freq-cap, and ranking stages.
#[derive(Debug, Clone)]
pub struct AdCandidate {
    pub campaign_id: CampaignId,
    pub creative_id: CreativeId,
    /// Bid price in cents (from campaign.bid_floor_cents, adjusted by pacer).
    pub bid_price_cents: i32,
    /// Feature-weighted score assigned by ScoringStage. 0.0 until scored.
    pub score: f32,
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

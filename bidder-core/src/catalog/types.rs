use crate::model::openrtb::AdFormat;
use serde::{Deserialize, Serialize};

pub type CampaignId = u32;
pub type SegmentId = u32;
pub type CreativeId = u32;

/// Geo targeting dimension key: (geo_kind, geo_code).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GeoKey {
    pub kind: GeoKind,
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeoKind {
    Country,
    Metro,
}

/// Device type targeting dimension — mirrors Postgres `device_type` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceTargetType {
    Desktop,
    Mobile,
    Tablet,
    Ctv,
    Other,
}

impl DeviceTargetType {
    /// Map from OpenRTB `device.devicetype` u8 to the targeting enum.
    pub fn from_openrtb(v: u8) -> Self {
        match v {
            1 | 4 | 5 => Self::Mobile,
            2 => Self::Desktop,
            3 | 7 => Self::Ctv,
            6 => Self::Ctv,
            _ => Self::Other,
        }
    }
}

/// In-memory campaign record (stripped to fields used on the hot path).
#[derive(Debug, Clone)]
pub struct Campaign {
    pub id: CampaignId,
    pub advertiser_id: u64,
    pub bid_floor_cents: i32,
    pub daily_budget_cents: i64,
    pub hourly_budget_cents: i64,
}

/// Creative variant loaded alongside the campaign catalog.
#[derive(Debug, Clone)]
pub struct Creative {
    pub id: CreativeId,
    pub campaign_id: CampaignId,
    pub ad_format: AdFormat,
    pub click_url: String,
    pub image_url: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

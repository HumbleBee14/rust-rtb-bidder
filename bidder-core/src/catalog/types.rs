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
            1 | 4 => Self::Mobile,
            2 => Self::Desktop,
            3 | 6 | 7 => Self::Ctv,
            5 => Self::Tablet,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrtb_device_mapping() {
        assert_eq!(DeviceTargetType::from_openrtb(1), DeviceTargetType::Mobile);
        assert_eq!(DeviceTargetType::from_openrtb(4), DeviceTargetType::Mobile);
        assert_eq!(DeviceTargetType::from_openrtb(2), DeviceTargetType::Desktop);
        assert_eq!(DeviceTargetType::from_openrtb(5), DeviceTargetType::Tablet);
        assert_eq!(DeviceTargetType::from_openrtb(3), DeviceTargetType::Ctv);
        assert_eq!(DeviceTargetType::from_openrtb(6), DeviceTargetType::Ctv);
        assert_eq!(DeviceTargetType::from_openrtb(7), DeviceTargetType::Ctv);
        assert_eq!(DeviceTargetType::from_openrtb(0), DeviceTargetType::Other);
        assert_eq!(DeviceTargetType::from_openrtb(255), DeviceTargetType::Other);
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
    /// 24h per-user impression cap. 0 = block after first impression.
    pub daily_cap_imps: u32,
    /// 1h per-user impression cap. 0 = block after first impression.
    pub hourly_cap_imps: u32,
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

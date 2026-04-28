mod campaign_catalog;
pub mod loader;
pub mod types;

pub use campaign_catalog::{CampaignCatalog, CandidateRequest};
pub use loader::{start, SharedCatalog};
pub use types::{
    Campaign, CampaignId, Creative, CreativeId, DeviceTargetType, GeoKey, GeoKind, SegmentId,
};

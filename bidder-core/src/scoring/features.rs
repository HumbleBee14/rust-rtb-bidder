//! Typed scoring-feature struct and packing logic.
//!
//! CONTRACT: docs/SCORING-FEATURES.md § 2.
//!
//! When the data-science team confirms the feature schema, this struct + the
//! transforms below are the SINGLE place that updates. Every other scorer
//! consumes `ScoringFeatures` already packed.

use crate::{
    catalog::DeviceTargetType, model::candidate::AdCandidate, model::openrtb::AdFormat,
    scoring::ScoringContext,
};

/// CONTRACT: docs/SCORING-FEATURES.md § 2 — feature_count.
///
/// Adding/removing a field requires updating the contract doc, regenerating the
/// test fixture (`make regen-test-model`), bumping the schema version, and
/// rolling the data-science training pipeline.
pub const FEATURE_COUNT: usize = 13;

/// CONTRACT: docs/SCORING-FEATURES.md § 2 — `bid_price_norm` upper bound.
///
/// Until the DS team supplies their training-time min/max constants, the bidder
/// uses this as the divisor for min-max normalization. `1000.0 cents = $10.00`
/// matches the `FeatureWeightedScorer` default.
pub const BID_PRICE_MAX_CENTS: f32 = 1000.0;

/// Top-K metro geos that flip `geo_top_market = 1`. Placeholder until the DS
/// team confirms the actual list. CONTRACT: docs/SCORING-FEATURES.md § 2.3.
#[allow(dead_code)]
pub const TOP_MARKETS_GEO_PLACEHOLDER: usize = 10;

/// Typed feature vector matching the bidder's default ONNX input.
///
/// Field order and layout MUST match `tools/gen-test-model/src/main.rs` and
/// `docs/SCORING-FEATURES.md` § 2 exactly. Any divergence corrupts scores
/// silently — the boot-time parity check exists to catch this.
#[derive(Debug, Clone, Copy)]
pub struct ScoringFeatures {
    // index 0
    pub bid_price_norm: f32,
    // index 1
    pub segment_overlap_ratio: f32,
    // index 2
    pub segment_count_log1p: f32,
    // indices 3..7 — one-hot device type
    pub device_desktop: f32,
    pub device_mobile: f32,
    pub device_tablet: f32,
    pub device_ctv: f32,
    // index 7
    pub hour_of_day_norm: f32,
    // index 8
    pub is_weekend: f32,
    // index 9
    pub geo_top_market: f32,
    // indices 10..13 — one-hot ad format (audio = all-zeros sentinel)
    pub format_banner: f32,
    pub format_video: f32,
    pub format_native: f32,
}

impl ScoringFeatures {
    /// Extract per-candidate features. Called once per candidate per scoring pass.
    ///
    /// CONTRACT: docs/SCORING-FEATURES.md § 2.2 — every transform here must match
    /// the training pipeline. When the DS team commits the real schema, replace
    /// the placeholder transforms below.
    pub fn extract(candidate: &AdCandidate, ctx: &ScoringContext<'_>) -> Self {
        // Bid price normalization. CONTRACT § 2.2 minmax_train.
        let bid_price_norm =
            (candidate.bid_price_cents as f32 / BID_PRICE_MAX_CENTS).clamp(0.0, 1.0);

        // Segment overlap. Phase 6 placeholder: until per-campaign segment
        // lists are threaded into the scorer, derive a deterministic value from
        // campaign_id so the parity test is stable. CONTRACT § 2.2 identity.
        // TODO(phase-7): compute real overlap from catalog.segment_to_campaigns.
        let n_segments = ctx.segment_ids.len();
        let segment_overlap_ratio = if n_segments == 0 {
            0.0
        } else {
            ((candidate.campaign_id % 10) as f32) / 10.0
        };

        // CONTRACT § 2.2 log1p.
        let segment_count_log1p = (n_segments as f32 + 1.0).ln();

        // CONTRACT § 2.3 one-hot device.
        let (desktop, mobile, tablet, ctv) = match ctx.device_type {
            Some(DeviceTargetType::Desktop) => (1.0, 0.0, 0.0, 0.0),
            Some(DeviceTargetType::Mobile) => (0.0, 1.0, 0.0, 0.0),
            Some(DeviceTargetType::Tablet) => (0.0, 0.0, 1.0, 0.0),
            Some(DeviceTargetType::Ctv) => (0.0, 0.0, 0.0, 1.0),
            // OTHER and missing both encode as all-zeros — matches CONTRACT § 2.3 "other = sentinel".
            _ => (0.0, 0.0, 0.0, 0.0),
        };

        // CONTRACT § 2.2 minmax_train. 24 hours → /23.
        let hour_of_day_norm = (ctx.hour_of_day.min(23) as f32) / 23.0;

        // is_weekend comes from the clock (UTC) via ScoringStage — it's an
        // infrastructure-level fact, not a DS decision. CONTRACT § 2.
        let is_weekend = if ctx.is_weekend { 1.0 } else { 0.0 };

        let geo_top_market = if ctx.is_top_market { 1.0 } else { 0.0 };

        // CONTRACT § 2.3 one-hot ad format.
        let (banner, video, native) = match ctx.ad_format {
            Some(AdFormat::Banner) => (1.0, 0.0, 0.0),
            Some(AdFormat::Video) => (0.0, 1.0, 0.0),
            Some(AdFormat::Native) => (0.0, 0.0, 1.0),
            // Audio and missing both encode as all-zeros.
            _ => (0.0, 0.0, 0.0),
        };

        Self {
            bid_price_norm,
            segment_overlap_ratio,
            segment_count_log1p,
            device_desktop: desktop,
            device_mobile: mobile,
            device_tablet: tablet,
            device_ctv: ctv,
            hour_of_day_norm,
            is_weekend,
            geo_top_market,
            format_banner: banner,
            format_video: video,
            format_native: native,
        }
    }

    /// Pack the typed feature struct into a fixed-order f32 row matching
    /// `docs/SCORING-FEATURES.md` § 2 indices 0..12. The order must never change
    /// without updating the contract, the generator, and the parity fixture.
    pub fn pack_into(&self, out: &mut [f32]) {
        debug_assert!(
            out.len() >= FEATURE_COUNT,
            "pack_into target slice must be >= FEATURE_COUNT"
        );
        out[0] = self.bid_price_norm;
        out[1] = self.segment_overlap_ratio;
        out[2] = self.segment_count_log1p;
        out[3] = self.device_desktop;
        out[4] = self.device_mobile;
        out[5] = self.device_tablet;
        out[6] = self.device_ctv;
        out[7] = self.hour_of_day_norm;
        out[8] = self.is_weekend;
        out[9] = self.geo_top_market;
        out[10] = self.format_banner;
        out[11] = self.format_video;
        out[12] = self.format_native;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ScoringContext<'static> {
        ScoringContext {
            segment_ids: &[],
            device_type: None,
            ad_format: None,
            hour_of_day: 12,
            is_weekend: false,
            user_id: "",
            is_top_market: false,
        }
    }

    fn candidate() -> AdCandidate {
        AdCandidate {
            campaign_id: 5,
            creative_id: 1,
            bid_price_cents: 500,
            score: 0.0,
            daily_cap_imps: u32::MAX,
            hourly_cap_imps: u32::MAX,
        }
    }

    #[test]
    fn pack_layout_matches_feature_count() {
        let f = ScoringFeatures::extract(&candidate(), &ctx());
        let mut row = [0.0f32; FEATURE_COUNT];
        f.pack_into(&mut row);
        assert_eq!(row.len(), FEATURE_COUNT);
        // bid_price_norm = 500 / 1000 = 0.5
        assert!((row[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn is_weekend_propagates_into_feature_vector() {
        let mut c = ctx();
        c.is_weekend = true;
        let f = ScoringFeatures::extract(&candidate(), &c);
        assert_eq!(f.is_weekend, 1.0);
        c.is_weekend = false;
        let f = ScoringFeatures::extract(&candidate(), &c);
        assert_eq!(f.is_weekend, 0.0);
    }

    #[test]
    fn device_one_hot_exclusive() {
        let mut c = ctx();
        c.device_type = Some(DeviceTargetType::Mobile);
        let f = ScoringFeatures::extract(&candidate(), &c);
        assert_eq!(f.device_desktop, 0.0);
        assert_eq!(f.device_mobile, 1.0);
        assert_eq!(f.device_tablet, 0.0);
        assert_eq!(f.device_ctv, 0.0);
    }
}

/// Full-pipeline integration test: assembles every stage against an in-memory
/// catalog seeded from the golden bid request fixture and asserts a Bid outcome.
///
/// Skips UserEnrichmentStage (requires Redis) and FreqCapStage (requires Redis)
/// by pre-seeding ctx.catalog and ctx.segment_ids directly.
use bidder_core::{
    catalog::{
        types::{Campaign, Creative, DeviceTargetType},
        CampaignCatalog, GeoKey, GeoKind,
    },
    config::LatencyBudgetConfig,
    model::{context::BidContext, openrtb::AdFormat, PipelineOutcome},
    pacing::{BudgetPacer, LocalBudgetPacer},
    pipeline::{
        stages::{
            BudgetPacingStage, CandidateLimitStage, CandidateRetrievalStage, RankingStage,
            RequestValidationStage, ResponseBuildStage, ScoringStage,
        },
        Pipeline,
    },
    scoring::FeatureWeightedScorer,
};
use roaring::RoaringBitmap;
use std::{collections::HashMap, sync::Arc};

const GOLDEN: &str = include_str!("../../tests/fixtures/golden-bid-request.json");

fn make_catalog() -> Arc<CampaignCatalog> {
    // Golden fixture: devicetype=2 (Desktop), imp[0]=video, imp[1]=banner.
    let campaign_id: u32 = 1;

    let mut campaigns = HashMap::new();
    campaigns.insert(
        campaign_id,
        Campaign {
            id: campaign_id,
            advertiser_id: 1,
            bid_floor_cents: 5,
            daily_budget_cents: 100_000,
            hourly_budget_cents: 10_000,
            daily_cap_imps: 10,
            hourly_cap_imps: 3,
        },
    );

    let mut creatives = HashMap::new();
    creatives.insert(
        campaign_id,
        vec![
            Creative {
                id: 10,
                campaign_id,
                ad_format: AdFormat::Banner,
                click_url: "https://example.com".to_string(),
                image_url: None,
                width: Some(300),
                height: Some(250),
            },
            Creative {
                id: 11,
                campaign_id,
                ad_format: AdFormat::Video,
                click_url: "https://example.com".to_string(),
                image_url: None,
                width: Some(640),
                height: Some(480),
            },
        ],
    );

    // Golden fixture: geo.country="USA", geo.metro="501".
    // Populate both so the geo intersection doesn't zero out the result.
    let mut geo_to_campaigns: HashMap<GeoKey, RoaringBitmap> = HashMap::new();
    for code in ["USA", "501"] {
        let mut bm = RoaringBitmap::new();
        bm.insert(campaign_id);
        let kind = if code.chars().all(|c| c.is_ascii_digit()) {
            GeoKind::Metro
        } else {
            GeoKind::Country
        };
        geo_to_campaigns.insert(
            GeoKey {
                kind,
                code: code.to_string(),
            },
            bm,
        );
    }

    let mut device_to_campaigns: HashMap<DeviceTargetType, RoaringBitmap> = HashMap::new();
    let mut dev_bm = RoaringBitmap::new();
    dev_bm.insert(campaign_id);
    device_to_campaigns.insert(DeviceTargetType::Desktop, dev_bm);

    let mut format_to_campaigns: HashMap<AdFormat, RoaringBitmap> = HashMap::new();
    for fmt in [AdFormat::Banner, AdFormat::Video] {
        let mut bm = RoaringBitmap::new();
        bm.insert(campaign_id);
        format_to_campaigns.insert(fmt, bm);
    }

    let mut all_campaigns = RoaringBitmap::new();
    all_campaigns.insert(campaign_id);

    Arc::new(CampaignCatalog::new_for_test(
        campaigns,
        creatives,
        HashMap::new(),
        geo_to_campaigns,
        device_to_campaigns,
        format_to_campaigns,
        RoaringBitmap::new(),
        all_campaigns,
    ))
}

fn latency_config() -> LatencyBudgetConfig {
    LatencyBudgetConfig {
        http_parse_ms: 2,
        request_validate_ms: 1,
        user_enrichment_ms: 5,
        candidate_retrieval_ms: 5,
        candidate_limit_ms: 1,
        scoring_ms: 2,
        frequency_cap_ms: 5,
        ranking_ms: 1,
        budget_pacing_ms: 2,
        response_build_ms: 2,
        pipeline_deadline_ms: 50,
        http_timeout_ms: 50,
        win_timeout_ms: 500,
    }
}

#[tokio::test]
async fn golden_request_produces_bid() {
    let pacer: Arc<dyn BudgetPacer> = Arc::new(LocalBudgetPacer::new());
    pacer.reload(vec![(1, 100_000)]).await;

    let pipeline = Pipeline::new(latency_config())
        .add_stage(RequestValidationStage)
        .add_stage(CandidateRetrievalStage)
        .add_stage(CandidateLimitStage { top_k: 20 })
        .add_stage(ScoringStage {
            scorer: Arc::new(FeatureWeightedScorer::default()),
        })
        .add_stage(BudgetPacingStage {
            pacer: Arc::clone(&pacer),
        })
        .add_stage(RankingStage {
            pacer: Arc::clone(&pacer),
        })
        .add_stage(ResponseBuildStage {
            notice_url_builder: std::sync::Arc::new(bidder_core::notice::NoNoticeUrl),
        });

    let request = serde_json::from_str(GOLDEN).expect("parse golden fixture");
    let mut ctx = BidContext::new(request);

    // Skip UserEnrichmentStage — seed catalog and segments directly.
    ctx.catalog = Some(make_catalog());
    ctx.segment_ids = vec![];

    pipeline.execute(&mut ctx).await.expect("pipeline error");

    assert_eq!(
        ctx.outcome,
        PipelineOutcome::Bid,
        "expected Bid, got {:?}",
        ctx.outcome
    );
    assert!(!ctx.winners.is_empty(), "winners should be non-empty");
    assert!(
        ctx.bid_response.is_some(),
        "bid_response should be populated"
    );
}

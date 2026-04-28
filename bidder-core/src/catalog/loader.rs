use crate::{
    catalog::{
        campaign_catalog::CampaignCatalog,
        types::{Campaign, CampaignId, Creative, DeviceTargetType, GeoKey, GeoKind, SegmentId},
    },
    config::CatalogConfig,
    model::openrtb::AdFormat,
    targeting::SegmentRegistry,
};
use arc_swap::ArcSwap;
use roaring::RoaringBitmap;
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tracing::{error, info, instrument, warn};

pub type SharedCatalog = Arc<ArcSwap<CampaignCatalog>>;
pub type SharedRegistry = Arc<ArcSwap<SegmentRegistry>>;

/// Builds and returns the initial catalog, then spawns a background task
/// that rebuilds it every `cfg.refresh_interval_secs`.
///
/// Both the catalog and segment registry are wrapped in `ArcSwap` so the
/// background task can swap them atomically. Hot-path readers call
/// `load_full()` to get an `Arc` snapshot valid for the duration of the request.
pub async fn start(
    pool: PgPool,
    cfg: CatalogConfig,
) -> anyhow::Result<(SharedCatalog, SharedRegistry)> {
    let (catalog, registry) = build(&pool).await?;
    info!(
        campaigns = catalog.len(),
        segments = registry.len(),
        "initial catalog loaded"
    );

    let shared_catalog = Arc::new(ArcSwap::from_pointee(catalog));
    let shared_registry = Arc::new(ArcSwap::from_pointee(registry));

    let catalog_clone = Arc::clone(&shared_catalog);
    let registry_clone = Arc::clone(&shared_registry);
    let interval = Duration::from_secs(cfg.refresh_interval_secs);
    let max_failures = cfg.max_consecutive_failures;

    tokio::spawn(async move {
        refresh_loop(pool, catalog_clone, registry_clone, interval, max_failures).await;
    });

    Ok((shared_catalog, shared_registry))
}

async fn refresh_loop(
    pool: PgPool,
    shared: SharedCatalog,
    registry: SharedRegistry,
    interval: Duration,
    max_failures: u32,
) {
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::time::sleep(interval).await;

        match build(&pool).await {
            Ok((new_catalog, new_registry)) => {
                let count = new_catalog.len();
                shared.store(Arc::new(new_catalog));
                registry.store(Arc::new(new_registry));
                consecutive_failures = 0;
                metrics::gauge!("bidder.catalog.campaign_count").set(count as f64);
                metrics::counter!("bidder.catalog.refresh_total", "result" => "ok").increment(1);
                info!(campaigns = count, "catalog refreshed");
            }
            Err(e) => {
                consecutive_failures += 1;
                metrics::counter!("bidder.catalog.refresh_total", "result" => "error").increment(1);
                metrics::gauge!("bidder.catalog.consecutive_failures")
                    .set(consecutive_failures as f64);

                if consecutive_failures >= max_failures {
                    // Old catalog stays live; alert so SRE knows.
                    error!(
                        consecutive_failures,
                        error = %e,
                        "catalog refresh circuit open — running on stale catalog"
                    );
                } else {
                    warn!(
                        consecutive_failures,
                        error = %e,
                        "catalog refresh failed"
                    );
                }
            }
        }
    }
}

/// Builds a fresh `CampaignCatalog` and `SegmentRegistry` from Postgres.
/// Runs entirely off the hot path; the hot path holds the old `Arc` until
/// `ArcSwap::store` completes.
#[instrument(skip(pool))]
pub(crate) async fn build(pool: &PgPool) -> anyhow::Result<(CampaignCatalog, SegmentRegistry)> {
    let start = std::time::Instant::now();

    // Run all queries concurrently across the pool.
    let (
        campaigns_rows,
        creatives_rows,
        segments_rows,
        seg_idx,
        geo_idx,
        device_idx,
        format_idx,
        daypart_active_now,
    ) = tokio::try_join!(
        query_campaigns(pool),
        query_creatives(pool),
        query_segments(pool),
        query_segment_index(pool),
        query_geo_index(pool),
        query_device_index(pool),
        query_format_index(pool),
        query_daypart_active_now(pool),
    )?;

    // Build segment registry.
    let mut registry = SegmentRegistry::default();
    for (id, name) in segments_rows {
        registry.insert(name, id as SegmentId);
    }

    // Build campaign map.
    let mut campaigns: HashMap<CampaignId, Campaign> = HashMap::with_capacity(campaigns_rows.len());
    let mut all_campaigns = RoaringBitmap::new();
    for row in campaigns_rows {
        let id = row.id as CampaignId;
        all_campaigns.insert(id);
        campaigns.insert(
            id,
            Campaign {
                id,
                advertiser_id: row.advertiser_id as u64,
                bid_floor_cents: row.bid_floor_cents,
                daily_budget_cents: row.daily_budget_cents,
                hourly_budget_cents: row.hourly_budget_cents,
            },
        );
    }

    // Build creatives map.
    let mut creatives: HashMap<CampaignId, Vec<Creative>> = HashMap::new();
    for row in creatives_rows {
        let cid = row.campaign_id as CampaignId;
        let Some(format) = parse_ad_format(&row.ad_format) else {
            continue;
        };
        creatives.entry(cid).or_default().push(Creative {
            id: row.id as u32,
            campaign_id: cid,
            ad_format: format,
            click_url: row.click_url,
            image_url: row.image_url,
            width: row.width,
            height: row.height,
        });
    }

    // Build segment → campaigns inverted index.
    let mut segment_to_campaigns: HashMap<SegmentId, RoaringBitmap> =
        HashMap::with_capacity(seg_idx.len());
    for (seg_id, campaign_ids) in seg_idx {
        let mut bm = RoaringBitmap::new();
        for cid in campaign_ids {
            bm.insert(cid as u32);
        }
        segment_to_campaigns.insert(seg_id as SegmentId, bm);
    }

    // Build geo → campaigns inverted index.
    let mut geo_to_campaigns: HashMap<GeoKey, RoaringBitmap> =
        HashMap::with_capacity(geo_idx.len());
    for (kind_str, code, campaign_ids) in geo_idx {
        let kind = if kind_str == "country" {
            GeoKind::Country
        } else {
            GeoKind::Metro
        };
        let key = GeoKey { kind, code };
        let mut bm = RoaringBitmap::new();
        for cid in campaign_ids {
            bm.insert(cid as u32);
        }
        geo_to_campaigns.insert(key, bm);
    }

    // Build device → campaigns inverted index.
    let mut device_to_campaigns: HashMap<DeviceTargetType, RoaringBitmap> =
        HashMap::with_capacity(5);
    for (device_str, campaign_ids) in device_idx {
        let Some(device) = parse_device_type(&device_str) else {
            continue;
        };
        let mut bm = RoaringBitmap::new();
        for cid in campaign_ids {
            bm.insert(cid as u32);
        }
        device_to_campaigns.entry(device).or_default().extend(bm);
    }

    // Build format → campaigns inverted index.
    let mut format_to_campaigns: HashMap<AdFormat, RoaringBitmap> = HashMap::with_capacity(4);
    for (format_str, campaign_ids) in format_idx {
        let Some(format) = parse_ad_format(&format_str) else {
            continue;
        };
        let mut bm = RoaringBitmap::new();
        for cid in campaign_ids {
            bm.insert(cid as u32);
        }
        format_to_campaigns.entry(format).or_default().extend(bm);
    }

    let elapsed = start.elapsed();
    metrics::histogram!("bidder.catalog.build_duration_seconds").record(elapsed.as_secs_f64());
    let elapsed_ms = elapsed.as_millis();

    if elapsed_ms > 5000 {
        warn!(
            elapsed_ms,
            "catalog build exceeded 5s budget — review index strategy"
        );
    } else {
        info!(
            elapsed_ms,
            campaigns = campaigns.len(),
            "catalog build complete"
        );
    }

    Ok((
        CampaignCatalog {
            campaigns,
            creatives,
            segment_to_campaigns,
            geo_to_campaigns,
            device_to_campaigns,
            format_to_campaigns,
            daypart_active_now,
            all_campaigns,
        },
        registry,
    ))
}

// ── Query helpers ─────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct CampaignRow {
    id: i64,
    advertiser_id: i64,
    bid_floor_cents: i32,
    daily_budget_cents: i64,
    hourly_budget_cents: i64,
}

#[derive(sqlx::FromRow)]
struct CreativeRow {
    id: i64,
    campaign_id: i64,
    ad_format: String,
    click_url: String,
    image_url: Option<String>,
    width: Option<i32>,
    height: Option<i32>,
}

#[derive(sqlx::FromRow)]
struct SegmentRow {
    id: i32,
    name: String,
}

#[derive(sqlx::FromRow)]
struct SegmentIndexRow {
    segment_id: i32,
    campaign_ids: Vec<i64>,
}

#[derive(sqlx::FromRow)]
struct GeoIndexRow {
    geo_kind: String,
    geo_code: String,
    campaign_ids: Vec<i64>,
}

#[derive(sqlx::FromRow)]
struct DeviceIndexRow {
    device_type: String,
    campaign_ids: Vec<i64>,
}

#[derive(sqlx::FromRow)]
struct FormatIndexRow {
    ad_format: String,
    campaign_ids: Vec<i64>,
}

#[derive(sqlx::FromRow)]
struct DaypartRow {
    campaign_id: i64,
}

async fn query_campaigns(pool: &PgPool) -> anyhow::Result<Vec<CampaignRow>> {
    Ok(sqlx::query_as::<_, CampaignRow>(
        r#"
        SELECT id, advertiser_id, bid_floor_cents, daily_budget_cents, hourly_budget_cents
          FROM campaign
         WHERE status = 'active'
        "#,
    )
    .fetch_all(pool)
    .await?)
}

async fn query_creatives(pool: &PgPool) -> anyhow::Result<Vec<CreativeRow>> {
    Ok(sqlx::query_as::<_, CreativeRow>(
        r#"
        SELECT cr.id, cr.campaign_id, cr.ad_format::text AS ad_format,
               cr.click_url, cr.image_url, cr.width, cr.height
          FROM creative cr
          JOIN campaign c ON c.id = cr.campaign_id
         WHERE c.status = 'active'
        "#,
    )
    .fetch_all(pool)
    .await?)
}

async fn query_segments(pool: &PgPool) -> anyhow::Result<Vec<(i32, String)>> {
    let rows =
        sqlx::query_as::<_, SegmentRow>("SELECT id, name FROM segment WHERE status = 'active'")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.name)).collect())
}

async fn query_segment_index(pool: &PgPool) -> anyhow::Result<Vec<(i32, Vec<i64>)>> {
    let rows = sqlx::query_as::<_, SegmentIndexRow>(
        r#"
        SELECT cts.segment_id,
               array_agg(cts.campaign_id ORDER BY cts.campaign_id) AS campaign_ids
          FROM campaign_targeting_segment cts
          JOIN campaign c ON c.id = cts.campaign_id
         WHERE c.status = 'active'
         GROUP BY cts.segment_id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.segment_id, r.campaign_ids))
        .collect())
}

async fn query_geo_index(pool: &PgPool) -> anyhow::Result<Vec<(String, String, Vec<i64>)>> {
    let rows = sqlx::query_as::<_, GeoIndexRow>(
        r#"
        SELECT ctg.geo_kind::text AS geo_kind,
               ctg.geo_code,
               array_agg(ctg.campaign_id ORDER BY ctg.campaign_id) AS campaign_ids
          FROM campaign_targeting_geo ctg
          JOIN campaign c ON c.id = ctg.campaign_id
         WHERE c.status = 'active'
         GROUP BY ctg.geo_kind, ctg.geo_code
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.geo_kind, r.geo_code, r.campaign_ids))
        .collect())
}

async fn query_device_index(pool: &PgPool) -> anyhow::Result<Vec<(String, Vec<i64>)>> {
    let rows = sqlx::query_as::<_, DeviceIndexRow>(
        r#"
        SELECT ctd.device_type::text AS device_type,
               array_agg(ctd.campaign_id ORDER BY ctd.campaign_id) AS campaign_ids
          FROM campaign_targeting_device ctd
          JOIN campaign c ON c.id = ctd.campaign_id
         WHERE c.status = 'active'
         GROUP BY ctd.device_type
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.device_type, r.campaign_ids))
        .collect())
}

async fn query_format_index(pool: &PgPool) -> anyhow::Result<Vec<(String, Vec<i64>)>> {
    let rows = sqlx::query_as::<_, FormatIndexRow>(
        r#"
        SELECT ctf.ad_format::text AS ad_format,
               array_agg(ctf.campaign_id ORDER BY ctf.campaign_id) AS campaign_ids
          FROM campaign_targeting_format ctf
          JOIN campaign c ON c.id = ctf.campaign_id
         WHERE c.status = 'active'
         GROUP BY ctf.ad_format
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.ad_format, r.campaign_ids))
        .collect())
}

async fn query_daypart_active_now(pool: &PgPool) -> anyhow::Result<RoaringBitmap> {
    let hour_of_week = current_hour_of_week();
    let rows = sqlx::query_as::<_, DaypartRow>(
        r#"
        SELECT cd.campaign_id
          FROM campaign_daypart cd
          JOIN campaign c ON c.id = cd.campaign_id
         WHERE c.status = 'active'
           AND get_bit(cd.hours_active, $1) = 1
        "#,
    )
    .bind(hour_of_week)
    .fetch_all(pool)
    .await?;

    let mut bm = RoaringBitmap::new();
    for r in rows {
        bm.insert(r.campaign_id as u32);
    }
    Ok(bm)
}

fn current_hour_of_week() -> i32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Monday = 0, Sunday = 6; hour 0-23. Total: 0-167.
    let day_of_week = ((secs / 86400) + 3) % 7; // Unix epoch was Thursday; +3 shifts to Monday=0
    let hour_of_day = (secs % 86400) / 3600;
    (day_of_week * 24 + hour_of_day) as i32
}

// ── Enum parsers ──────────────────────────────────────────────────────────────

fn parse_ad_format(s: &str) -> Option<AdFormat> {
    match s {
        "BANNER" | "banner" => Some(AdFormat::Banner),
        "VIDEO" | "video" => Some(AdFormat::Video),
        "AUDIO" | "audio" => Some(AdFormat::Audio),
        "NATIVE" | "native" => Some(AdFormat::Native),
        _ => {
            warn!(
                ad_format = s,
                "unknown ad_format value in Postgres — row skipped"
            );
            None
        }
    }
}

fn parse_device_type(s: &str) -> Option<DeviceTargetType> {
    match s {
        "DESKTOP" | "desktop" => Some(DeviceTargetType::Desktop),
        "MOBILE" | "mobile" => Some(DeviceTargetType::Mobile),
        "TABLET" | "tablet" => Some(DeviceTargetType::Tablet),
        "CTV" | "ctv" => Some(DeviceTargetType::Ctv),
        "OTHER" | "other" => Some(DeviceTargetType::Other),
        _ => {
            warn!(
                device_type = s,
                "unknown device_type value in Postgres — row skipped"
            );
            None
        }
    }
}

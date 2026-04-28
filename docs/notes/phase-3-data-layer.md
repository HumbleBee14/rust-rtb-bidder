# Phase 3 — Data layer

## What this phase delivers

The bidder now loads real campaigns from Postgres, builds inverted indices for sub-millisecond candidate retrieval at 50K–100K campaign scale, fetches user segments from Redis with a local moka cache in front, and refreshes the catalog atomically in the background every 60 seconds.

---

## Why inverted indices — the core architectural decision

The Java predecessor benchmarked against 1,000 campaigns. Linear scan over a `Vec<Campaign>` was fast enough at that scale. At 50K–100K campaigns it is not:

```
Linear scan, 100K campaigns, 10 μs per check:  100,000 × 10 μs = 1,000 ms  ← unusable
Inverted-index intersection, same workload:     ~5 segments × ~500 matching
                                                campaigns each = 2,500 bitmap
                                                ops ≈ 0.1–0.5 ms            ← fine
```

Every real DSP uses inverted indices for candidate retrieval (Criteo "Cuttle", Moloco, AppNexus "Bonsai"). This is the single largest architectural improvement over the Java baseline.

---

## Component map

```
Startup
────────────────────────────────────────────────────────────────────────────
  main()
    │
    ├── PgPoolOptions::connect()          Postgres pool (min 2, max 8 conns)
    │
    ├── catalog::start(pg_pool, cfg)
    │    │
    │    ├── build(&pool)                 7 concurrent queries via try_join!
    │    │    ├── query_campaigns         all active campaigns
    │    │    ├── query_creatives         creatives for active campaigns
    │    │    ├── query_segments          segment id→name registry
    │    │    ├── query_segment_index     segment_id → [campaign_ids]
    │    │    ├── query_geo_index         (geo_kind, geo_code) → [campaign_ids]
    │    │    ├── query_device_index      device_type → [campaign_ids]
    │    │    └── query_format_index      ad_format → [campaign_ids]
    │    │
    │    ├── assemble CampaignCatalog     RoaringBitmaps + HashMaps
    │    ├── query_daypart_active_now     168-bit week mask → active bitmap
    │    │
    │    ├── Arc::new(ArcSwap::from(catalog))   SharedCatalog
    │    └── tokio::spawn(refresh_loop)          background task
    │
    ├── Pool::new(FredConfig, ...)        Redis round-robin pool (N = num_cpus)
    │    └── wait_for_connect()
    │
    ├── SegmentCache::new(500K, 60s)      moka W-TinyLFU async cache
    │
    └── Pipeline::new(budget)
         .add_stage(RequestValidationStage)
         .add_stage(UserEnrichmentStage { catalog, cache, repo })
         .add_stage(ResponseBuildStage)


Hot path (per request)
────────────────────────────────────────────────────────────────────────────

  BidRequest arrives
        │
        ▼
  UserEnrichmentStage
        │
        ├── ctx.catalog = shared_catalog.load_full()    Arc clone, ~5 ns
        │
        └── if user.id present:
              SegmentCache::get_or_fetch(user_id)
                │
                ├── cache hit  → Vec<SegmentId>         no Redis call
                └── cache miss → RedisSegmentRepo::segments_for(user_id)
                                    SMEMBERS user_segments:{user_id}
                                    resolve names → IDs via SegmentRegistry
                                    insert into cache


Background catalog refresh (every 60 s)
────────────────────────────────────────────────────────────────────────────

  refresh_loop
        │
        ├── build()                    new CampaignCatalog allocated
        │    └── (same 7 queries + daypart)
        │
        ├── ArcSwap::store(Arc::new(new_catalog))
        │    └── in-flight requests holding old Arc finish + drop it
        │        new requests see new catalog
        │        zero coordination, no RwLock, no pause
        │
        ├── SegmentRegistry::merge(new_registry)   append-only, no eviction
        │
        └── on failure:
              consecutive_failures++
              if >= max_failures: error!("circuit open")   old catalog stays live
              else:               warn!("refresh failed")
```

---

## CampaignCatalog — what's inside

```
CampaignCatalog {
    campaigns:            HashMap<CampaignId, Campaign>
    creatives:            HashMap<CampaignId, Vec<Creative>>

    // Inverted indices — read-only after construction
    segment_to_campaigns: HashMap<SegmentId,       RoaringBitmap>
    geo_to_campaigns:     HashMap<GeoKey,           RoaringBitmap>
    device_to_campaigns:  HashMap<DeviceTargetType, RoaringBitmap>
    format_to_campaigns:  HashMap<AdFormat,         RoaringBitmap>
    daypart_active_now:   RoaringBitmap   // recomputed each refresh

    all_campaigns:        RoaringBitmap   // universe for "no restriction" dims
}
```

`candidates_for(req)` does bitmap intersection across present dimensions:

```
result = segment_union(req.segment_ids)   // OR across user's segments
result &= geo_union(req.geo_keys)         // AND if geo present
result &= device_bitmap(req.device)       // AND if device present
result &= format_bitmap(req.format)       // AND if format present
result &= daypart_active_now              // AND if non-empty
return result                             // owned RoaringBitmap of candidate IDs
```

**Mutation safety:** indices are `pub(crate)` and only accessible through `candidates_for`, which always returns a new owned bitmap. No caller ever gets a mutable reference to an index bitmap. This prevents silent catalog corruption from in-place bitmap ops on shared data.

---

## Bitmap-mutation safety — why it matters

`RoaringBitmap::or_inplace` and `and_inplace` mutate the receiver. If you call them directly on a bitmap from the catalog index (even through an `Arc`), you corrupt the index for every request that sees the same catalog snapshot. The bug is silent: Roaring doesn't panic, results just become wrong.

The contract enforced in this codebase:
- Raw index fields are `pub(crate)`, not `pub`
- `candidates_for()` is the only entry point; it clones before any in-place operation
- The doc comment on `CampaignCatalog` states the invariant explicitly

---

## ArcSwap — atomic catalog refresh with zero coordination

```
Phase 3 does NOT use:                    Phase 3 DOES use:
  RwLock<CampaignCatalog>                  Arc<ArcSwap<CampaignCatalog>>
  ↳ write lock blocks all readers           ↳ store() is one atomic pointer swap
  ↳ read lock is contended at 30K RPS       ↳ readers hold an Arc; never block
  ↳ partial-state reads possible            ↳ snapshot is immutable after swap
```

`ArcSwap::load_full()` gives each request an `Arc<CampaignCatalog>` snapshot. The background task calls `store(Arc::new(new_catalog))`. In-flight requests finish against the old snapshot and drop it; the new snapshot is visible to all subsequent requests.

---

## Redis pool — why round-robin from day one

A single multiplexed Redis connection has one decode thread. At 30K+ RPS it saturates at 100% CPU and silently spikes p99 — the Java predecessor hit this exactly and had to retrofit a pool. Starting with N = num_cpus connections means the decode work is parallelised across connections from the first request.

```
fred::clients::Pool   (fred 10.x, dynamic-pool feature)
  │
  ├── connection 0  ─►  Redis
  ├── connection 1  ─►  Redis
  ├── ...                        round-robin per request
  └── connection N  ─►  Redis

N = cfg.redis.pool_size (0 = num_cpus at runtime)
```

---

## Moka cache — two-level lookup

```
user_segments:{user_id}  Redis key  (SMEMBERS → set of segment name strings)

Per-request lookup order:
  1. moka cache (in-process, W-TinyLFU, 500K capacity, 60s TTL)
       hit  → Vec<SegmentId>   no network call
       miss ↓
  2. Redis SMEMBERS user_segments:{user_id}
       → Vec<String> (segment names)
       → SegmentRegistry::resolve(names) → Vec<SegmentId>
       → insert into moka
```

`SegmentRegistry` maps name strings to `u32` IDs. Roaring bitmaps store IDs, not strings, so string-to-ID resolution happens once at cache-miss time, not on every bitmap lookup.

---

## sqlx without compile-time query verification

`sqlx::query!` macros verify SQL against a live database at compile time (or a prepared cache). That's ideal but requires either a `DATABASE_URL` in the build environment or running `cargo sqlx prepare` to snapshot query metadata. For a project that should build cleanly from a `git clone` with no external services, this is the wrong tradeoff.

We use `sqlx::query_as::<_, RowType>(sql)` instead. Runtime type checking (column names and types validated at first execution) with `#[derive(sqlx::FromRow)]` structs. The SQL is still explicit; mistakes surface on first test run, not silently at runtime in prod.

---

## Seed script

`docker/seed-postgres.py` seeds the database with realistic Zipfian-distributed data:

- **Segment popularity follows Zipf α=1.1** — a small number of "auto-intender", "millennial", "in-market-travel" type segments appear in most campaigns; long tail of niche segments. Real ad-tech segment data is heavily skewed.
- **Geo skewed to top-10 US metros** — 60% of campaigns target New York, LA, Chicago, etc. Matches real DSP traffic patterns.
- **Batched inserts** — 1,000 campaigns per batch via `execute_values`, not row-by-row. 10K campaigns seeds in under 30 seconds.

Seeding with uniform random data would produce bitmaps of medium density that don't exercise Roaring's actual hot paths. Zipfian data produces the mix of dense (popular segments) and sparse (niche) bitmaps that match production.

---

## What was deferred and why

| Deferred | Why | Phase |
|---|---|---|
| `CandidateRetrievalStage` (bitmap intersection on hot path) | `candidates_for()` exists; stage wiring is Phase 4 with scoring | 4 |
| Hedged Redis reads | Requires circuit breaker + hedge budget tracking | 4/5 |
| Frequency cap MGET | Phase 4 pipeline stage | 4 |
| Distributed budget pacing | Phase 4 | 4 |
| Aerospike segment repo | Rust client is sync-only; wrap in spawn_blocking if needed | future |
| Redis pool size benchmark sweep | Requires real load; document chosen N after Phase 4 k6 run | 4 |

---

## PLAN.md audit — Phase 3

| Deliverable | Status |
|---|---|
| sqlx Postgres → Vec<Campaign> at startup | ✓ |
| `Arc<ArcSwap<CampaignCatalog>>` atomic refresh | ✓ |
| Background refresh every 60s | ✓ |
| Consecutive-failure circuit open + alert | ✓ |
| Inverted indices: segment, geo, device, format, daypart | ✓ |
| `candidates_for()` bitmap intersection API | ✓ |
| Bitmap-mutation safety contract (pub(crate) indices) | ✓ |
| fred round-robin pool (N = num_cpus) | ✓ |
| moka async cache (500K, 60s TTL) | ✓ |
| `UserSegmentRepository` trait | ✓ |
| `RedisSegmentRepo` impl | ✓ |
| `SegmentRegistry` name→ID map | ✓ |
| `UserEnrichmentStage` in pipeline | ✓ |
| `BidContext.segment_ids` + `catalog` populated | ✓ |
| `docker/seed-postgres.py` (Zipfian) | ✓ |
| `[postgres]` + `[redis]` + `[catalog]` in config.toml | ✓ |

# Postgres Schema — Campaign Catalog

Source of truth for the campaign catalog. The bidder loads this schema into in-memory inverted indices (`Arc<ArcSwap<CampaignCatalog>>`, RoaringBitmap-based) at startup and refreshes every 60s. See `PLAN.md` Phase 3 for the rebuild contract and the bitmap-mutation safety contract.

This schema is shaped by one constraint: **the full catalog load + index build must complete in under 5s for 100K campaigns.** Every targeting dimension lives in its own indexed join table so each inverted-index build is a single `GROUP BY` scan, not a JSONB GIN traversal.

## Postgres version

Pinned to **Postgres 16.x** (current stable major). Specifically uses `GENERATED ALWAYS AS IDENTITY`, `CREATE TYPE ... AS ENUM`, partial indexes, and `bigint` PKs — all stable since 12+, but 16 gets the planner improvements for parallel `array_agg` and the `pg_stat_io` view used in load-time profiling. No 17-only features used; upgrade path is open.

## Migration tool

**`sqlx migrate`** — chosen over `refinery`.

Reasons:
1. The bidder already pulls `sqlx` for the runtime catalog load (Phase 3). Adding `refinery` is a second migration mental model and a second dependency surface for no benefit.
2. `sqlx migrate` reads plain `.sql` files from `migrations/` ordered by numeric prefix, applies them in a single transaction, and records each in a `_sqlx_migrations` table. No DSL, no embedded macros required at deploy time.
3. The CLI (`sqlx migrate run`) is the same binary used in CI smoke tests — one tool covers dev, CI, and production deploy.

Convention:
- File names: `NNNN_description.sql` (zero-padded 4-digit prefix, snake_case description).
- Migrations are **append-only**. Once merged to `main`, a migration file is never edited. Schema fixes ship as a new migration that alters the prior state.
- Down migrations are **not** maintained — production rollback is forward-fix or restore-from-backup, not `sqlx migrate revert`. Stated explicitly to avoid the false safety of a half-tested down path.
- The `_sqlx_migrations` table sqlx auto-creates is the de-facto migration ledger; we do not maintain a separate `schema_migrations` table.

## Connection pool sizing

The Postgres pool is an **admin-load tool**, not a hot-path tool. The hot path never touches Postgres. The pool is used for:
- Startup: one bulk catalog load — campaigns, segments, creatives, plus one query per targeting dimension. Typically 7+ queries total (see "Inverted-index build queries" below for the full set).
- Background refresh: same query set every 60s.
- Operator tooling (segment registration UPSERTs from the DMP ingestion side, campaign CRUD from the admin API in later phases).

Recommended `sqlx::PgPoolOptions`:

| Setting | Value | Reason |
|---|---|---|
| `max_connections` | `8` | Background refresh runs queries concurrently to keep total load time < 5s; 8 covers the catalog query set with modest headroom. |
| `min_connections` | `2` | Keep two warm so the 60s refresh doesn't pay TCP+TLS handshake every cycle. |
| `acquire_timeout` | `2s` | Refresh is non-blocking (Phase 3 contract); a stuck pool fails fast and the old catalog stays live. |
| `idle_timeout` | `10 min` | Long enough to span the 60s refresh cycle; short enough that Postgres-side connection limits aren't held hostage. |
| `max_lifetime` | `30 min` | Defense against long-lived connections accumulating planner cache bloat. |

The hot-path Redis pool is sized separately (Phase 3, Redis throughput discipline) — do not conflate.

## Tables

### `campaign`

Core campaign entity. One row per active or historical campaign.

| Column | Type | Constraints | Notes |
|---|---|---|---|
| `id` | `bigint` | PK, `GENERATED ALWAYS AS IDENTITY` | Stable bidder-side identifier; used as the `u32`/`u64` campaign ID inside RoaringBitmap inverted indices. `bigint` chosen over `int` to outlast the lifetime of a single deployment without an ID-rollover migration. |
| `name` | `text` | `NOT NULL` | Human-readable; not in the hot path. |
| `advertiser_id` | `bigint` | `NOT NULL` | FK target deferred until an `advertiser` table exists (Phase 4+); kept as raw `bigint` now so the column type is locked in. |
| `status` | `campaign_status` enum | `NOT NULL`, default `'draft'` | Inverted indices only include rows where `status = 'active'`. Partial indexes lean on this. |
| `bid_floor_cents` | `integer` | `NOT NULL`, `CHECK >= 0` | Per-campaign floor in USD cents. Cents (integer) chosen over `numeric(12,4)` to avoid arbitrary-precision math on the hot path. |
| `daily_budget_cents` | `bigint` | `NOT NULL`, `CHECK >= 0` | Pacing target. Atomic budget consumption is Redis-backed (Phase 4); Postgres holds the configured target only. |
| `hourly_budget_cents` | `bigint` | `NOT NULL`, `CHECK >= 0` | Same shape as daily; pacing logic in Phase 4 selects which to enforce. |
| `created_at` | `timestamptz` | `NOT NULL`, default `now()` | |
| `updated_at` | `timestamptz` | `NOT NULL`, default `now()` | Bumped by trigger on UPDATE (see migration). |
| `version` | `integer` | `NOT NULL`, default `1` | Optimistic-concurrency token. Admin writes do `UPDATE ... WHERE id = $1 AND version = $2`; conflict surfaces as zero rows affected. |

Rationale: status as a typed enum (not text) so the hot-path filter is a 4-byte tag compare in Postgres and an exhaustive match in Rust. `version` is unrelated to schema migrations — it's per-row and only matters on writes.

### `segment`

Global segment registry. Single source of truth for `segment_name → segment_id`. Populated by seed data, by the DMP ingestion side (UPSERT on first sight), and by operator tools.

| Column | Type | Constraints | Notes |
|---|---|---|---|
| `id` | `integer` | PK, `GENERATED ALWAYS AS IDENTITY` | 32-bit by design — RoaringBitmap stores `u32` segment IDs (see `SEGMENT-IDS.md`, Phase 0.3). |
| `name` | `text` | `NOT NULL`, `UNIQUE` | Wire-format segment string from the DMP. Compared as-is, no normalization. |
| `category` | `text` | `NOT NULL`, default `'uncategorized'` | Loose grouping (`auto`, `travel`, `demographic`, etc.). Not indexed; informational only for now. |
| `status` | `segment_status` enum | `NOT NULL`, default `'active'` | Lifecycle. `'retired'` segments stop being included in new campaigns; old bitmaps that reference the ID remain valid (deletion would orphan them). See `SEGMENT-IDS.md` for the rationale. |
| `created_at` | `timestamptz` | `NOT NULL`, default `now()` | |

Rationale: 32-bit IDs cap the registry at ~4B segments — orders of magnitude beyond the 10K-100K target. Multi-tenant scoping (`(tenant_id, segment_id)` tuples) is explicitly deferred per Phase 0.3 and is not encoded in the v1 schema. Segment IDs are append-only and never reused; retirement is the only soft-delete mechanism.

### `campaign_targeting_segment`

Many-to-many join between campaign and segment. Drives the `segment → campaigns` inverted index.

| Column | Type | Constraints |
|---|---|---|
| `campaign_id` | `bigint` | `NOT NULL`, FK → `campaign(id)` ON DELETE CASCADE |
| `segment_id` | `integer` | `NOT NULL`, FK → `segment(id)` ON DELETE RESTRICT |

PK: `(campaign_id, segment_id)`. Secondary index: `(segment_id, campaign_id)` to support the inverted-index build query without a sort step.

Rationale: composite PK is the natural uniqueness constraint and gives campaign-side lookups for free. The reverse-order index is what the inverted-index `GROUP BY segment_id` build query actually scans.

### `campaign_targeting_geo`

Geo targeting per campaign. Stores ISO-3166 country codes and Nielsen DMA / metro codes side by side; the bidder treats them as opaque strings keyed into separate maps at index-build time.

| Column | Type | Constraints |
|---|---|---|
| `campaign_id` | `bigint` | `NOT NULL`, FK → `campaign(id)` ON DELETE CASCADE |
| `geo_code` | `text` | `NOT NULL` |
| `geo_kind` | `geo_kind` enum | `NOT NULL` (`'country'` or `'metro'`) |

PK: `(campaign_id, geo_kind, geo_code)`. Secondary index: `(geo_kind, geo_code, campaign_id)` for the inverted-index build.

Rationale: country and metro are two different lookup keys at request time (the OpenRTB request carries both); colocating them in one table with a discriminator column avoids a third targeting table while keeping the index lookup typed.

### `campaign_targeting_device`

Device-type targeting. Closed enum (`DESKTOP`, `MOBILE`, `TABLET`, `CTV`, `OTHER`).

| Column | Type | Constraints |
|---|---|---|
| `campaign_id` | `bigint` | `NOT NULL`, FK → `campaign(id)` ON DELETE CASCADE |
| `device_type` | `device_type` enum | `NOT NULL` |

PK: `(campaign_id, device_type)`. Secondary index: `(device_type, campaign_id)`.

Rationale: enum, not text — device-type cardinality is permanently small and an enum makes the inverted-index key a 4-byte tag.

### `campaign_targeting_format`

Ad-format targeting. Closed enum (`BANNER`, `VIDEO`, `NATIVE`, `AUDIO`).

| Column | Type | Constraints |
|---|---|---|
| `campaign_id` | `bigint` | `NOT NULL`, FK → `campaign(id)` ON DELETE CASCADE |
| `ad_format` | `ad_format` enum | `NOT NULL` |

PK: `(campaign_id, ad_format)`. Secondary index: `(ad_format, campaign_id)`.

### `campaign_daypart`

Hour-of-week activeness per campaign.

**Design choice: bitmap column (`bit(168)`), one row per campaign.**

| Column | Type | Constraints |
|---|---|---|
| `campaign_id` | `bigint` | PK, FK → `campaign(id)` ON DELETE CASCADE |
| `hours_active` | `bit(168)` | `NOT NULL` | One bit per hour-of-week starting Monday 00:00 UTC. |

Rationale for bitmap-over-rows:
1. **Storage and load cost.** 168 rows per campaign × 100K campaigns = 16.8M rows just for daypart. A `bit(168)` column is 21 bytes per campaign — three orders of magnitude smaller, faster to scan, faster to ship over the wire on refresh.
2. **The bidder needs the whole mask anyway.** The `daypart_active_now` inverted index is recomputed every minute by walking all campaigns and checking the current hour-of-week bit. With one row per campaign, that's a single sequential scan; with 168 rows per campaign, it's a `WHERE hour_of_week = $1` against an index — same complexity, more rows, no win.
3. **Set semantics in Postgres.** `bit(168)` supports `&`, `|`, `get_bit` natively, so admin tools can express "campaigns active at hour H" as `WHERE get_bit(hours_active, $1) = 1` without a join.

Trade-off: the bitmap is opaque to ad-hoc SQL readers ("which hours is this campaign on?" needs a `get_bit` loop or a helper view). Acceptable given this table is admin-tooling territory, not analyst territory.

### `creative`

Per-campaign creative variants. Not on the hot-path candidate-selection path (filtered after candidate retrieval), but loaded with the catalog so the response builder has them in memory.

| Column | Type | Constraints |
|---|---|---|
| `id` | `bigint` | PK, `GENERATED ALWAYS AS IDENTITY` |
| `campaign_id` | `bigint` | `NOT NULL`, FK → `campaign(id)` ON DELETE CASCADE |
| `ad_format` | `ad_format` enum | `NOT NULL` |
| `click_url` | `text` | `NOT NULL` |
| `image_url` | `text` | NULL allowed (video / audio creatives) |
| `width` | `integer` | NULL allowed (audio) |
| `height` | `integer` | NULL allowed (audio) |
| `created_at` | `timestamptz` | `NOT NULL`, default `now()` |

Index: `(campaign_id, ad_format)` for per-campaign creative lookup at response-build time.

Rationale: kept thin — Phase 6 (ML scoring) may add creative-level features (CTR priors etc.), but those are bolted on as separate tables, not crammed into `creative`.

## Index strategy

Every index exists to support a specific query. No speculative indexing.

| Index | Table | Query supported |
|---|---|---|
| `campaign_pkey` | `campaign(id)` | Primary lookup; PK auto. |
| `idx_campaign_active` | `campaign(id) WHERE status = 'active'` (partial) | Catalog load filter — `WHERE campaign.status = 'active'` covers a fraction of the table; partial index is significantly smaller. |
| `idx_campaign_updated_at` | `campaign(updated_at)` | Future incremental refresh (Phase 3 v2); harmless to index now. |
| `segment_pkey` | `segment(id)` | PK auto. |
| `segment_name_key` | `segment(name)` | UNIQUE constraint — used by the DMP-side UPSERT-on-first-sight path. |
| `cts_pkey` | `campaign_targeting_segment(campaign_id, segment_id)` | PK auto; per-campaign segment list. |
| `idx_cts_segment` | `campaign_targeting_segment(segment_id, campaign_id)` | Inverted-index build — `GROUP BY segment_id`. |
| `ctg_pkey` | `campaign_targeting_geo(campaign_id, geo_kind, geo_code)` | PK auto. |
| `idx_ctg_geo` | `campaign_targeting_geo(geo_kind, geo_code, campaign_id)` | Inverted-index build per geo dimension. |
| `ctd_pkey` | `campaign_targeting_device(campaign_id, device_type)` | PK auto. |
| `idx_ctd_device` | `campaign_targeting_device(device_type, campaign_id)` | Inverted-index build. |
| `ctf_pkey` | `campaign_targeting_format(campaign_id, ad_format)` | PK auto. |
| `idx_ctf_format` | `campaign_targeting_format(ad_format, campaign_id)` | Inverted-index build. |
| `campaign_daypart_pkey` | `campaign_daypart(campaign_id)` | PK auto. |
| `creative_pkey` | `creative(id)` | PK auto. |
| `idx_creative_campaign` | `creative(campaign_id, ad_format)` | Per-campaign creative lookup at response build. |

The composite reverse-order indexes (`idx_cts_segment`, `idx_ctg_geo`, `idx_ctd_device`, `idx_ctf_format`) are the ones that keep the catalog load under 5s — without them the planner sorts before grouping.

## Inverted-index build queries

These are the exact queries the `CampaignCatalog` background task runs at startup and on every 60s refresh. Each populates one inverted index field on the freshly-allocated `CampaignCatalog` (per the Phase 3 rebuild contract — built off-hot-path, swapped in via `ArcSwap`).

```sql
-- segment_to_campaigns
SELECT cts.segment_id,
       array_agg(cts.campaign_id ORDER BY cts.campaign_id) AS campaign_ids
  FROM campaign_targeting_segment cts
  JOIN campaign c ON c.id = cts.campaign_id
 WHERE c.status = 'active'
 GROUP BY cts.segment_id;
```

```sql
-- geo_to_campaigns (country and metro emitted in one pass; Rust splits by geo_kind)
SELECT ctg.geo_kind,
       ctg.geo_code,
       array_agg(ctg.campaign_id ORDER BY ctg.campaign_id) AS campaign_ids
  FROM campaign_targeting_geo ctg
  JOIN campaign c ON c.id = ctg.campaign_id
 WHERE c.status = 'active'
 GROUP BY ctg.geo_kind, ctg.geo_code;
```

```sql
-- device_to_campaigns
SELECT ctd.device_type,
       array_agg(ctd.campaign_id ORDER BY ctd.campaign_id) AS campaign_ids
  FROM campaign_targeting_device ctd
  JOIN campaign c ON c.id = ctd.campaign_id
 WHERE c.status = 'active'
 GROUP BY ctd.device_type;
```

```sql
-- format_to_campaigns
SELECT ctf.ad_format,
       array_agg(ctf.campaign_id ORDER BY ctf.campaign_id) AS campaign_ids
  FROM campaign_targeting_format ctf
  JOIN campaign c ON c.id = ctf.campaign_id
 WHERE c.status = 'active'
 GROUP BY ctf.ad_format;
```

```sql
-- daypart_active_now (recomputed by the per-minute task; $1 is current hour-of-week 0..167)
SELECT cd.campaign_id
  FROM campaign_daypart cd
  JOIN campaign c ON c.id = cd.campaign_id
 WHERE c.status = 'active'
   AND get_bit(cd.hours_active, $1) = 1;
```

```sql
-- campaign metadata + creatives (loaded into the catalog struct alongside the indices)
SELECT c.id, c.name, c.advertiser_id, c.status, c.bid_floor_cents,
       c.daily_budget_cents, c.hourly_budget_cents, c.version
  FROM campaign c
 WHERE c.status = 'active';

SELECT cr.id, cr.campaign_id, cr.ad_format, cr.click_url, cr.image_url, cr.width, cr.height
  FROM creative cr
  JOIN campaign c ON c.id = cr.campaign_id
 WHERE c.status = 'active';

SELECT id, name FROM segment WHERE status = 'active';
```

The eight queries (four inverted-index builds, plus daypart, campaigns, creatives, and segment registry) are issued concurrently across the pool. `array_agg` is ordered so the Rust side feeds RoaringBitmap construction with sorted IDs (RoaringBitmap accepts unsorted input but sorted is faster to build).

## Why this schema and not alternatives

**Why not a single `campaign.targeting_jsonb` column?**
`jsonb` + GIN works for ad-hoc filters but is ~5-10× slower for the full-catalog `GROUP BY segment_id` scan that builds the inverted index. The hot path doesn't read this table at all (it reads RoaringBitmaps in memory), so the only consumer is the 60s rebuild — and the rebuild's 5s budget is the binding constraint. Dedicated join tables with composite indexes win on that exact query pattern.

**Why not store inverted indices in Postgres?**
The rebuild contract requires atomic swap of the in-memory representation. Maintaining inverted indices in Postgres would require either materialized views (refresh latency, lock semantics) or a parallel write path the bidder doesn't own. Postgres holds the source of truth; the bidder owns the index representation.

**Why `bit(168)` for daypart instead of one row per active hour?**
Justified above under `campaign_daypart`. Short version: 100K campaigns × 168 hours = 16.8M rows is a serialization tax with no compensating query benefit, and the bidder consumes the full bitmap anyway.

**Why integer cents instead of `numeric`?**
Hot path does no arithmetic on these (Redis owns budget consumption). Integer cents is the standard ad-tech representation and avoids `numeric` decode cost on every refresh.

**Why no `down` migrations?**
Production rollback in this design is forward-fix or backup-restore, not `migrate revert`. Maintaining tested down paths is a tax we don't pay. Stated explicitly so a future contributor doesn't add empty `revert` files thinking they're load-bearing.

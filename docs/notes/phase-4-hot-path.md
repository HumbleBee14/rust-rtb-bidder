# Phase 4 — Hot path: targeting, scoring, freq-cap, budget pacing, ranking

## What this phase delivers

The bidder now produces real OpenRTB `BidResponse`s end-to-end. Six new stages slot in after `UserEnrichmentStage`. Phase 3 built the data plane (catalog, registry, segment cache); Phase 4 builds the bid plane that runs on top of it.

---

## Component map

```
                        ┌─────────────────────────────────────────────────────┐
                        │  bidder-core::pipeline::Pipeline                    │
                        │                                                     │
  POST                  │  RequestValidation (P2)                             │
  /rtb/openrtb/bid      │       │                                             │
        │               │       ▼                                             │
        ▼               │  UserEnrichment (P3)                                │
  ┌───────────────┐     │       │  populates ctx.segment_ids + ctx.catalog    │
  │ bid handler   │     │       ▼                                             │
  │               │     │  CandidateRetrieval                                 │
  │ Bytes →       │     │       │  catalog.candidates_for(...)                │
  │  Vec<u8> →    │ ─►  │       │  → RoaringBitmap of campaign IDs            │
  │  simd_json    │     │       │  → expand to Vec<AdCandidate> per imp       │
  │               │     │       ▼                                             │
  │ pipeline      │     │  CandidateLimit                                     │
  │   .execute()  │     │       │  BinaryHeap<Reverse<HeapEntry>> top-K       │
  │               │     │       │  K = pipeline.max_candidates_per_imp (20)   │
  │ on Bid:       │     │       ▼                                             │
  │   serialize   │     │  Scoring                                            │
  │   bid_resp    │     │       │  Scorer::score_all() — batched per imp      │
  │   try_record  │     │       │  FeatureWeightedScorer (Phase 4 default)    │
  │   per winner  │     │       ▼                                             │
  │               │     │  FrequencyCap                                       │
  │ on NoBid:     │     │       │  RedisFrequencyCapper.check(...)            │
  │   204         │     │       │  single MGET per request, slot-local        │
  └───────┬───────┘     │       │  on timeout → SkippedTimeout (uncapped)     │
          │             │       ▼                                             │
          │             │  BudgetPacing                                       │
          │             │       │  pacer.check_and_reserve() per candidate    │
          │             │       │  filters Exhausted; reserved cents -= bid   │
          │             │       ▼                                             │
          │             │  Ranking                                            │
          │             │       │  pick max-score per imp, tie-break by price │
          │             │       │  release reserved budget for non-winners    │
          │             │       │  set ctx.outcome = Bid | NoBid              │
          │             │       ▼                                             │
          │             │  ResponseBuild                                      │
          │             │       │  serialize ctx.winners → BidResponse        │
          │             │       │  store in ctx.bid_response                  │
          │             └─────────────────────────────────────────────────────┘
          │
          │  ImpressionEvent per winner (try_record, non-blocking)
          ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  ImpressionRecorder mpsc(65_536) — bounded, drop-on-full              │
  │       │                                                                │
  │       ▼                                                                │
  │  spawn_impression_workers (semaphore-bounded fan-out, N=2 default)     │
  │       │                                                                │
  │       ▼                                                                │
  │  Lua EVAL: INCR + EXPIRE-on-first                                      │
  │       v1:fc:{u:<id>}:c:<campaign_id>:h   TTL 3600                      │
  │       v1:fc:{u:<id>}:c:<campaign_id>:d   TTL 86400                     │
  └──────────────────────────────────────────────────────────────────────┘


  Catalog refresh + pacer reseed (every 60 s)
  ────────────────────────────────────────────────────────────────────────
   refresh_loop
        │
        ├── build()  same 8 queries from Phase 3
        │
        ├── pacer.reload(new_catalog.budget_seeds())   ← NEW in Phase 4
        │      seed (campaign_id, daily_budget_cents) into LocalBudgetPacer
        │      replaces stale budgets atomically
        │
        ├── shared_catalog.store(Arc::new(new_catalog))
        ├── shared_registry.store(Arc::new(new_registry))
        │
        └── on failure:
              consecutive_failures++
              if >= max: error!("circuit open"); old catalog stays live
```

---

## Data flow through `BidContext`

```
new(request)                                        empty
    │
    ▼  RequestValidation
context unchanged on success; ctx.outcome = NoBid(INVALID_REQUEST) on failure
    │
    ▼  UserEnrichment
ctx.segment_ids = Vec<SegmentId>     populated from moka / Redis
ctx.catalog     = Some(Arc<Catalog>) snapshot for this request
    │
    ▼  CandidateRetrieval
ctx.candidates = Vec<Vec<AdCandidate>>     one Vec per impression
    │
    ▼  CandidateLimit
ctx.candidates  trimmed to top_k per imp by bid_price_cents
    │
    ▼  Scoring
ctx.candidates[*][*].score = f32      assigned by Scorer::score_all
    │
    ▼  FrequencyCap
ctx.candidates  capped entries removed in-place
ctx.freq_cap_results = Vec<(imp_idx, campaign_id, capped)>
    │
    ▼  BudgetPacing
ctx.candidates  Exhausted entries removed in-place
                pacer state: reserved budget decremented per remaining candidate
    │
    ▼  Ranking
ctx.winners = Vec<ImpWinner>          one per imp (highest score)
ctx.outcome = Bid | NoBid(NO_ELIGIBLE_BIDS)
                pacer state: reserved budget RELEASED for non-winners
    │
    ▼  ResponseBuild
ctx.bid_response = Some(BidResponse)
    │
    ▼  handler
on Bid    → 200 + JSON BidResponse + try_record per winner
on NoBid  → 204
```

---

## New traits and types

### `Scorer` (`bidder-core/src/scoring/mod.rs`)

```rust
trait Scorer: Send + Sync + 'static {
    async fn score_all(&self, candidates: &mut Vec<AdCandidate>, segment_ids: &[u32]);
}
```

Batched API — encode request features once per call, mutate scores in place. Default impl `FeatureWeightedScorer` is a fast linear model (`0.4 * norm_price + 0.6 * overlap_proxy`); Phase 6 swaps in `MLScorer` and `CascadeScorer`.

### `FrequencyCapper` (`bidder-core/src/frequency/mod.rs`)

```rust
trait FrequencyCapper: Send + Sync + 'static {
    async fn check(&self, user_id: &str, candidates: &[AdCandidate], device_type_val: u8, hour_of_day: u8)
        -> FreqCapOutcome;
}

enum FreqCapOutcome { Checked(Vec<CapResult>), SkippedTimeout, SkippedNoUser }
```

Single MGET per user — all cap keys share `{u:<userId>}` so the read is one round-trip on cluster (REDIS-KEYS.md contract). On timeout, returns `SkippedTimeout` and bidding proceeds uncapped — SLA > bid quality.

### `BudgetPacer` (`bidder-core/src/pacing/mod.rs`)

```rust
trait BudgetPacer: Send + Sync + 'static {
    async fn check_and_reserve(&self, campaign_id: CampaignId, bid_cents: i32) -> PaceDecision;
    async fn release(&self, campaign_id: CampaignId, bid_cents: i32);
    async fn reload(&self, budgets: Vec<(CampaignId, i64)>);
}
```

`LocalBudgetPacer` impl: `RwLock<HashMap<CampaignId, Arc<AtomicI64>>>`. Hot path takes a read lock to fetch the `Arc<AtomicI64>` then does a lock-free `fetch_sub`. `reload` holds the write lock briefly during catalog refresh. `DistributedBudgetPacer` lands in Phase 5 against `v1:bud:{c:<id>}:d` with Redis `DECRBY`.

### `ImpressionRecorder` (`bidder-core/src/frequency/impression_recorder.rs`)

```rust
struct ImpressionRecorder { tx: mpsc::Sender<ImpressionEvent> }
impl ImpressionRecorder {
    fn try_record(&self, event: ImpressionEvent);  // non-blocking, drops on full
}
```

Bounded `mpsc(65_536)`. The bid handler calls `try_record` once per winner immediately after building the response. Workers consume off the bid path and write the freq-cap counters via Lua EVAL.

### `BidContext` additions

```rust
candidates:       Vec<Vec<AdCandidate>>,        // per impression, mutated through stages
winners:          Vec<ImpWinner>,                // per impression, post-ranking
freq_cap_results: Vec<(usize, u32, bool)>,       // (imp_idx, campaign_id, capped)
bid_response:     Option<BidResponse>,           // set by ResponseBuildStage
outcome:          PipelineOutcome,               // Pending | Bid | NoBid (Bid added Phase 4)
```

---

## Key design decisions

### `BinaryHeap<Reverse<HeapEntry>>` for top-K

Rust's `BinaryHeap` is a max-heap. Wrapping entries in `Reverse` flips the ordering so `pop()` returns the cheapest — the standard Rust min-heap idiom. Capacity is bounded at `top_k`; on overflow we `pop()` the cheapest, keeping the K most expensive candidates. Pre-filtering by price (not score, which is 0.0 at this stage) avoids spending scoring CPU on candidates that can't win on price alone.

### Pacer overshoot is bounded but not strictly one bid

`check_and_reserve` does `fetch_sub` first, then compensates with `fetch_add` if `prev <= 0`. This avoids a CAS loop and converges to correct net spend, but under N concurrent in-flight bids on the same near-zero-budget campaign, the counter can transiently dip below zero by up to N bids before the compensating adds catch up. For Phase 4 single-instance the magnitude is small and acceptable. Phase 5 distributed pacer uses Redis `DECRBY` which is atomic per slot.

### Pipeline short-circuit only on NoBid

The pipeline breaks the stage loop only when `ctx.outcome` is `NoBid(_)`. `Bid` outcome must continue through `ResponseBuildStage` so `ctx.bid_response` gets populated — otherwise the handler would 500 with `Bid + bid_response.is_none()`.

### Budget release for non-winners

`BudgetPacingStage` reserves budget for every candidate that survives freq-cap. Only one wins per impression. Without releasing, the K-1 losing candidates per imp leak their bid amounts from the pacer; at 30K RPS × 20 candidates × multi-imp this depletes daily budgets in seconds with zero actual spend. `RankingStage` now holds `Arc<dyn BudgetPacer>` and calls `release()` for every non-winner before pushing to `ctx.winners`.

### simd-json buffer model preserved

The bid handler copies `Bytes` into an owned `Vec<u8>` before calling `simd_json::from_slice` (in-place mutation requires `&mut [u8]`). `Bytes` is reserved for downstream zero-copy reference passing only — never the parse buffer.

### `#[allow(clippy::manual_async_fn)]` on stage impls

The `Stage` trait uses RPITIT (`impl Future`) rather than `#[async_trait]` to avoid per-call boxing on the hot path. Implementing this with `async fn` inside the `impl` triggers the clippy lint; the allow is intentional.

### Scorer overlap proxy is a placeholder

`FeatureWeightedScorer` uses `(campaign_id % 10) / 10.0` as an overlap signal. The real per-campaign segment set lives in the catalog's inverted index (segment → campaigns), not on `Campaign` or `AdCandidate`. Carrying the segment list per candidate would inflate `AdCandidate` size on the hot path. Phase 6 introduces `MLScorer` (ONNX) which needs proper features and will pre-compute campaign features at catalog load time.

---

## Config additions

```toml
[pipeline]
max_candidates_per_imp = 20

[freq_cap]
impression_workers = 2
```

`pipeline.max_candidates_per_imp` is the top-K cutoff. `freq_cap.impression_workers` is the concurrency cap for the impression-write fan-out; writes are off the bid path so a small N is fine.

---

## Wiring sequence in `main.rs`

```
1. Postgres pool          PgPoolOptions
2. Redis pool             fred round-robin (Phase 3)
3. SegmentCache           moka 500K (Phase 3)
4. ImpressionRecorder     bounded mpsc(65_536)  ← NEW
5. spawn_impression_workers(redis_pool, recorder_rx, N)  ← NEW
6. RedisSegmentRepo       (Phase 3)
7. RedisFrequencyCapper   timeout = latency_budget.frequency_cap_ms  ← NEW
8. LocalBudgetPacer       empty; reload happens inside catalog::start  ← NEW
9. Scorer                 FeatureWeightedScorer  ← NEW
10. catalog::start(pool, cfg, pacer)
       └── pacer.reload(catalog.budget_seeds())    ← NEW: seeded on init
       └── tokio::spawn(refresh_loop)              also reseeds on every refresh
11. Pipeline::new(...).add_stage(...) × 9 stages
12. AppState { health, pipeline, catalog, redis, segment_cache, impression_recorder, ... }
13. axum::serve(...)
14. warmup::run(...)  → health.set_ready()
```

---

## Bug fixes during PR review

Five wiring bugs found in review and fixed before merge:

| Bug | Symptom | Fix |
|---|---|---|
| Pacer never seeded | Every campaign returned `Exhausted` from empty `HashMap`; pipeline produced 0 bids | `catalog::start` takes `Arc<dyn BudgetPacer>` and calls `pacer.reload(catalog.budget_seeds())` after build; `refresh_loop` reseeds on each successful rebuild |
| `try_record` never called | Recorder + workers wired but no caller; freq-cap counters stayed at 0; cap check always passed | Bid handler enqueues per winner after `PipelineOutcome::Bid` |
| Pipeline short-circuited on `Bid` | `if outcome != Pending` broke loop before `ResponseBuildStage`; `bid_response = None` → 500 | Changed to `if matches!(outcome, NoBid(_))` |
| Non-winner budget leak | `BudgetPacingStage` reserved for all top-K but only 1 won per imp; K-1 leaks per imp | `RankingStage` holds `Arc<dyn BudgetPacer>`; releases each non-winner |
| Dead `CapDimension`/`CapConstraint` | Unused symbols in `frequency/mod.rs` | Removed |

Plus minor cleanups: removed `keys.clone()` allocation in `RedisFrequencyCapper::check`; extracted `current_hour_of_day` from inline duplicates into `bidder-core/src/clock.rs`; dropped `TODO(phase-6)` comment from `feature_weighted.rs`.

---

## Test coverage

26 tests (25 unit + 1 integration):

| Module | Count | Notes |
|---|---|---|
| `catalog::campaign_catalog` | 5 | Phase 3 |
| `catalog::types` | 1 | Phase 3 |
| `targeting` | 3 | Phase 3 |
| `scoring::feature_weighted` | 3 | Phase 4 |
| `pacing::local` | 6 | Phase 4 — overshoot semantics, release/reload |
| `pipeline::stages::ranking` | 4 | Phase 4 — score-max, tie-break, no-bid, multi-imp |
| `pipeline::stages::candidate_limit` | 3 | Phase 4 — top-K, noop under limit, empty |
| `pipeline_integration::golden_request_produces_bid` | 1 | Phase 4 — full pipeline against golden fixture; asserts `Bid`, non-empty winners, populated `bid_response` |

The integration test is gated behind the `test-helpers` feature (which exposes `CampaignCatalog::new_for_test`); CI runs it via `cargo test --release --all-features`.

---

## What was deferred and why

| Deferred | Why | Phase |
|---|---|---|
| Hedged Redis MGET with 4 guardrails | Requires circuit breaker + load-shed coupling | 5 |
| Per-campaign freq-cap limits from catalog | Hard-coded 10/day, 3/hour for Phase 4 | 5 |
| `DistributedBudgetPacer` (Redis DECRBY) | Phase 4 ships single-instance; multi-instance needs cross-pod state | 5 |
| `seed-redis.py` for `v1:seg:{u:<id>}` | Hot-path load tests need it; tracked from Phase 3 | 5 |
| Real segment overlap in `FeatureWeightedScorer` | Needs campaign segment list pre-computed at catalog load | 6 |
| `/rtb/win` notice endpoint | Phase 4 records on bid emit (proxy); real DSPs record on SSP win-notice | 5 |

---

## PLAN.md audit — Phase 4

| Deliverable | Status |
|---|---|
| `targeting::SegmentTargetingEngine` (bitmap intersection) | ✓ via `CampaignCatalog::candidates_for` from Phase 3, called from `CandidateRetrievalStage` |
| `pipeline::stages::CandidateLimitStage` with `BinaryHeap<Reverse<_>>` | ✓ |
| `scoring::FeatureWeightedScorer` with `score_all` batched API | ✓ |
| `frequency::RedisFrequencyCapper` paged MGET | ✓ single MGET per user (campaign-day + campaign-hour) |
| `frequency::ImpressionRecorder` bounded mpsc + workers | ✓ mpsc(65_536) + Lua EVAL workers |
| `pacing::LocalBudgetPacer` with `AtomicI64` | ✓ |
| `pacing::DistributedBudgetPacer` (Redis DECRBY) | ✗ deferred to Phase 5 |
| `pipeline::stages::RankingStage` top-1 per imp | ✓ |
| Hedged Redis MGET (4 guardrails) | ✗ deferred to Phase 5 |
| Partial-result fallback (skip freq cap on timeout) | ✓ `SkippedTimeout` |
| `SmallVec` inline storage | ✗ deferred (profile first) |
| `bytes::Bytes` for shared zero-copy buffers | ✓ used downstream from request decode |

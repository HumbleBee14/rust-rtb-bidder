# Phase 4 — Hot Path: Targeting, Scoring, Frequency Capping

## What was built

The full end-to-end bid pipeline now produces a real OpenRTB `BidResponse`. Six new stages were added after `UserEnrichmentStage`:

```
RequestValidation → UserEnrichment → CandidateRetrieval → CandidateLimit
  → Scoring → FreqCap → BudgetPacing → Ranking → ResponseBuild
```

### New stages

| Stage | File | What it does |
|---|---|---|
| `CandidateRetrievalStage` | `pipeline/stages/candidate_retrieval.rs` | Bitmap intersection via `catalog.candidates_for()`, expands to `Vec<AdCandidate>` per impression |
| `CandidateLimitStage` | `pipeline/stages/candidate_limit.rs` | Min-heap (`BinaryHeap<Reverse<_>>`) keeps top-K by price; K=20 by default |
| `ScoringStage` | `pipeline/stages/scoring.rs` | Calls `Scorer::score_all()` over each impression's candidate list |
| `FreqCapStage` | `pipeline/stages/freq_cap.rs` | Timeout-guarded MGET; on timeout returns `SkippedTimeout` and bidding continues uncapped |
| `BudgetPacingStage` | `pipeline/stages/budget_pacing.rs` | Calls `BudgetPacer::check_and_reserve()` per candidate; filters exhausted campaigns |
| `RankingStage` | `pipeline/stages/ranking.rs` | Picks highest-score candidate per impression, tie-breaks by price |
| `ResponseBuildStage` (updated) | `pipeline/stages/response_build.rs` | Serializes `ctx.winners` into `BidResponse` with `seatbid`; price = cents / 100 |

### New traits and impls

**`Scorer`** (`bidder-core/src/scoring/mod.rs`): `score_all(&mut Vec<AdCandidate>, segment_ids)`. Batched API — encode request features once per call, not per candidate.

**`FeatureWeightedScorer`** (default): linear model `0.4 * norm_price + 0.6 * segment_overlap_proxy`. Segment overlap is a proxy via `campaign_id % 10` in Phase 4; Phase 6 replaces it with a real pCTR model using actual per-campaign segment sets.

**`FrequencyCapper`** (`bidder-core/src/frequency/mod.rs`): `check(user_id, candidates, device_type, hour)` → `FreqCapOutcome`. The MGET builds keys `v1:fc:{u:<id>}:c:<campaign_id>:[h|d]`.

**`RedisFrequencyCapper`** (`bidder-server/src/freq_cap.rs`): Single MGET per request. Hard-coded caps: 10/day, 3/hour per campaign. Phase 5 loads per-campaign limits from the catalog.

**`ImpressionRecorder`** (`bidder-core/src/frequency/impression_recorder.rs`): Bounded `mpsc(65_536)`. Non-blocking `try_record()` — drops on full, increments `bidder.freq_cap.recorder.dropped`. Workers consume via `spawn_impression_workers` in `bidder-server/src/freq_cap.rs`.

**Impression write workers**: Lua EVAL script — atomic `INCR` + `EXPIRE` on first increment. Two keys per event: `v1:fc:{u:<id>}:c:<campaign_id>:h` (TTL 3600) and `...:d` (TTL 86400).

**`BudgetPacer`** (`bidder-core/src/pacing/mod.rs`): `check_and_reserve` / `release` / `reload`.

**`LocalBudgetPacer`** (`bidder-core/src/pacing/local.rs`): `RwLock<HashMap<CampaignId, Arc<AtomicI64>>>`. Hot path takes a read lock to get the `Arc<AtomicI64>`, then does a lock-free `fetch_sub`. Reload holds write lock only during catalog refresh (rare). Phase 5 replaces with Redis DECRBY against `v1:bud:{c:<id>}:d`.

### Pacer overshoot semantics

`check_and_reserve` checks `prev > 0`, not `prev >= bid_cents`. This means one bid per campaign can overshoot the remaining budget by up to one bid price. The counter goes negative; subsequent bids are blocked until `release` or `reload`. This is intentional — it avoids a compare-and-swap loop and the overshoot is bounded to a single bid.

### BidContext additions

```rust
pub candidates: Vec<Vec<AdCandidate>>,       // per impression
pub winners: Vec<ImpWinner>,                  // per impression, post-ranking
pub freq_cap_results: Vec<(usize, u32, bool)>,// (imp_idx, campaign_id, capped)
pub bid_response: Option<BidResponse>,        // set by ResponseBuildStage
```

`PipelineOutcome::Bid` variant added. `ResponseBuildStage` sets `bid_response` on both `Bid` and `NoBid` paths.

### Config additions

Two new sections in `config.toml` and `bidder-core/src/config/mod.rs`:

```toml
[pipeline]
max_candidates_per_imp = 20

[freq_cap]
impression_workers = 2
```

### Handler update

`bid` handler in `bidder-server/src/server/handlers.rs` now serializes `ctx.bid_response` as JSON on `PipelineOutcome::Bid`, returns 200 with `Content-Type: application/json`.

## Decisions and trade-offs

**No hedged Redis MGET in Phase 4.** PLAN.md calls for hedged MGET with 4 guardrails (idempotent-only, latency trigger, health gate, adaptive sample cap). Deferred to Phase 5 — Phase 4 uses a plain timeout-guarded MGET. The `SkippedTimeout` fallback provides the SLA protection.

**`#[allow(clippy::manual_async_fn)]` on stage impls.** The `Stage` trait uses RPITIT (`impl Future`) rather than `#[async_trait]` to avoid the boxing overhead on the hot path. Implementing this with `async fn` in the impl triggers the clippy lint; the allow is necessary and intentional.

**Scorer uses campaign_id % 10 as overlap proxy.** The inverted index in the catalog maps segments → campaign bitmaps, but the campaign struct doesn't carry its own segment list. Feeding the real per-campaign segment set into `score_all` requires either a catalog lookup per candidate (expensive) or a segment list stored on `AdCandidate`. Deferred to Phase 6 when the ONNX scorer will need richer features anyway.

**FreqCap keys in `write_impression_counters` do not use the `fc_key` helper.** The write path formats keys inline to avoid the `CapWindow` enum allocation. The key shape matches the read path exactly: `v1:fc:{u:<id>}:c:<campaign_id>:[h|d]`.

## Post-PR fixes (review round)

Several bugs found during PR review were fixed before merge:

**Budget pacer never seeded (zero-bid bug).** `LocalBudgetPacer::new()` creates an empty map; `check_and_reserve` for any campaign_id returns `Exhausted`. Fix: `catalog::start` now takes `Arc<dyn BudgetPacer>` and calls `pacer.reload(catalog.budget_seeds())` after the initial load. `refresh_loop` also takes the pacer and reseeds on every successful rebuild. `CampaignCatalog::budget_seeds()` extracts `(id, daily_budget_cents)` pairs for all campaigns.

**ImpressionRecorder never called (dead freq-cap counters).** `ImpressionRecorder` was created and workers spawned but `try_record` was never invoked. Fix: the `bid` handler in `handlers.rs` now calls `state.impression_recorder.try_record(ImpressionEvent { ... })` for each winner after `PipelineOutcome::Bid`.

**Pipeline short-circuit blocked ResponseBuildStage.** The pipeline's break condition fired on any non-Pending outcome, including `Bid`. `ResponseBuildStage` was therefore never reached when `RankingStage` set `Bid`, leaving `ctx.bid_response = None`. Fix: the break condition is now `matches!(ctx.outcome, PipelineOutcome::NoBid(_))` — only NoBid short-circuits.

**Non-winning candidate budget leak.** `BudgetPacingStage` reserved budget for all top-K candidates; only one winner per impression was needed. The remaining K-1 reserved amounts were never returned. Fix: `RankingStage` now holds `Arc<dyn BudgetPacer>` and calls `pacer.release(c.campaign_id, c.bid_price_cents)` for every non-winner after picking the winner.

**Dead exports removed.** `CapDimension` and `CapConstraint` were defined in `frequency/mod.rs` but never used anywhere. Removed.

**`keys.clone()` on hot path.** `RedisFrequencyCapper::check` cloned the key Vec before passing it to `mget`. Since `keys` is not used after the call, changed to move.

**`current_hour_of_day` deduplicated.** The function was inline in `pipeline/stages/freq_cap.rs` and referenced as `bidder_core::clock::current_hour_of_day` in `handlers.rs`. Extracted to `bidder-core/src/clock.rs` and imported from there in both callers.

**`TODO(phase-6)` comment removed** from `feature_weighted.rs`.

## Test coverage

26 tests (25 unit + 1 integration):
- `catalog::campaign_catalog` (5 tests, carried from Phase 3)
- `catalog::types` (1 test, carried from Phase 3)
- `targeting` (3 tests, carried from Phase 3)
- `scoring::feature_weighted` (3 tests)
- `pacing::local` (6 tests)
- `pipeline::stages::ranking` (4 tests)
- `pipeline::stages::candidate_limit` (3 tests)
- `pipeline_integration::golden_request_produces_bid` — full pipeline against golden fixture, asserts `PipelineOutcome::Bid`, non-empty winners, and populated `bid_response` (uses `test-helpers` feature to construct in-memory `CampaignCatalog`)

## Phase 5 prerequisites

- Redis DECRBY budget pacer (`v1:bud:{c:<id>}:d`) for multi-instance deployments
- Per-campaign freq-cap limits loaded from catalog (currently hard-coded 10/day, 3/hour)
- Hedged MGET with 4 guardrails
- `seed-redis.py` in `docker/` for dev/test bootstrapping
- Real segment overlap in `FeatureWeightedScorer` (requires campaign segment list on `AdCandidate`)

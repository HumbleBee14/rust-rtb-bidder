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

## Test coverage

25 unit tests across:
- `catalog::campaign_catalog` (5 tests, carried from Phase 3)
- `catalog::types` (1 test, carried from Phase 3)
- `targeting` (3 tests, carried from Phase 3)
- `scoring::feature_weighted` (3 tests)
- `pacing::local` (6 tests)
- `pipeline::stages::ranking` (4 tests)
- `pipeline::stages::candidate_limit` (3 tests)

## Phase 5 prerequisites

- Redis DECRBY budget pacer (`v1:bud:{c:<id>}:d`) for multi-instance deployments
- Per-campaign freq-cap limits loaded from catalog (currently hard-coded 10/day, 3/hour)
- Hedged MGET with 4 guardrails
- `seed-redis.py` in `docker/` for dev/test bootstrapping
- Real segment overlap in `FeatureWeightedScorer` (requires campaign segment list on `AdCandidate`)

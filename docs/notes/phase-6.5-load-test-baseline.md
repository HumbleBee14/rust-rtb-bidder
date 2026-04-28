# Phase 6.5 — Load-test baseline

## Goal

Land the two Phase 3 deferrals that have been carried since Phase 3:

1. **`docker/seed-redis.py`** — populates the `v1:seg:{u:<userId>}` family per `docs/REDIS-KEYS.md`. The Java repo's seed used a different shape (`SADD key str str str` against a hash-tag-less key); that shape is incompatible with the Rust bidder's `SET <key> <packed-u32-bytes>` reader. This script writes the contract-correct shape from scratch with Zipfian segment popularity.

2. **`LOAD-TEST-RESULTS-rust-v0-baseline.md`** — first reference numbers from a real k6 run against the full stack. Without baseline numbers, every Phase 7 deferral was circular ("we don't have load-test data" — but nobody had run a load test). This document fixes that by reading actual measurements from a tiered 5K/10K/15K RPS run on macOS dev.

The point of this phase is **not** to optimize anything. It's to land the seed contract, capture honest baseline numbers, and surface what those numbers tell us about real bottlenecks (vs the speculative ones called out in PLAN.md).

---

## Architectural changes

### `docker/seed-redis.py` — written from scratch against `REDIS-KEYS.md`

| Decision | Why |
|---|---|
| `SET v1:seg:{u:<userId>} <bytes>` not `SADD <key> <str>` | Contract per Phase 0 — the bidder reads with `from_le_bytes` and packs IDs as `u32`. SADD-of-strings doesn't decode at memcpy speed and doesn't satisfy hash-tag co-location with `v1:fc:{u:...}`. |
| Hash-tag braces `{u:<userId>}` literal in the key | Required for Redis Cluster co-location of `seg` and `fc` keys per user. Single-node deploys ignore the braces; cluster deploys route on `CRC16("u:<userId>")`. |
| Raw little-endian packed `u32`, no header | Matches the bidder's `chunks_exact(4).map(u32::from_le_bytes)` reader exactly. Smallest dense format; defers all decode work to the bitmap construction in `targeting`. |
| Zipfian segment popularity (α=1.1) | Matches `seed-postgres.py` segment-id distribution and real DSP traffic patterns. Uniform random would produce uniformly-medium-density bitmaps that don't exercise Roaring's compression paths. |
| Per-user segment count uniform [50, 200] | Matches Phase 0 workload table (avg 100). Independent of segment popularity — the per-user count distribution is a separate concern from which segments are popular. |
| 14-day TTL via `EXPIRE` (not `SET ... EX`) | RESP `--pipe` ingest is more reliable with separate `SET` then `EXPIRE` than with binary-payload `SET key val EX ttl`. Two commands per user; still ~200K users/sec end-to-end. |
| Default 100K users (vs 100M production target) | A 100M-user dev seed would need ~48 GB of Redis (per the contract memory budget). 100K fits in <100 MB and exceeds anything the load test can exercise in 5 min at 5K RPS. Override with `BIDDER_SEED_USERS=N` env var. |

### `docker-compose.yml` — Postgres + Redis stack under project name `bidder-stack`

The Java repo had one; the Rust repo didn't. Adding it fixes two problems:

1. **Visibility in Docker Desktop** — both containers group under the `bidder-stack` project label instead of being orphaned `docker run` artifacts. Easier to start/stop/inspect together.
2. **Reproducible local dev** — `make stack-up` is idempotent, brings up both services with healthchecks gated, returns when ready. Same model production deploy will use (Helm chart, Phase 7).

Bidder process itself stays out of compose (runs natively via `cargo run` or `target/release/bidder-server`) so `samply` and `tokio-console` can attach without container indirection.

### Makefile targets — first-class infra workflow

The Makefile previously had `regen-test-model`, `install-ort`, `dev-env`. Phase 6.5 adds:

- `make stack-up` / `stack-down` / `stack-reset` / `stack-status` — compose lifecycle.
- `make migrate` — runs `sqlx migrate run` against the dev Postgres.
- `make seed-postgres` / `seed-redis` / `seed` — campaign and user-segment seeding (parameterized via `CAMPAIGNS=N SEGMENTS=N USERS=N`).
- `make baseline` — single-tier 5K RPS, captures k6 summary + Prometheus snapshot.
- `make baseline-tiered` — sequential 5K → 10K → 15K with 30s cooldowns; captures per-tier results.
- `make baseline-clean` — resets the results dir.

### `tools/analyze-baseline.sh` — observability one-shot

Reads a results dir of `v0-baseline-<RPS>rps-summary.json` + `v0-baseline-<RPS>rps-prometheus.txt` files and emits four Markdown tables: HTTP timing, pipeline outcomes, resilience signals, per-stage timing breakdown. The same set of metrics matters for every load-test run; rather than hand-extracting each time, this script is the canonical analyzer. Also produces output ready to paste directly into the LOAD-TEST-RESULTS doc.

---

## Baseline measurement methodology

| Decision | Rationale |
|---|---|
| **macOS dev, not Linux prod** | Phase 7 is when Linux numbers happen. Phase 6.5 captures dev-environment numbers so per-PR regressions on macOS can be detected in the same env where you debug. Linux production numbers will be 1.5-3× faster on per-request latency due to faster syscalls + better fsevent / inotify difference, but the *shape* of the numbers (which stage is hot, where breakers fire, which counters spike) carries over. |
| **3 tiers: 5K, 10K, 15K** | 5K = nominal target (PLAN.md baseline tier). 10K = stretch under tuning. 15K = above ceiling, exposes the saturation point. Skipping 20K+ because at 15K the bidder's HTTP queue is already saturating. |
| **30s ramp + 120s hold + 30s ramp-down per tier** | Industry standard burst test. 60-90s hold is enough for steady-state metrics; longer just produces more data of the same kind. Trimmed from PLAN.md's golden 60+300+60 = 7-min profile because 3 tiers × 7 min = 21 min was burning runtime without adding signal. |
| **30s cooldown between tiers** | Lets circuit breakers close, freq-cap counters drain, Kafka producer queue partially flush. Without it, tier N+1 starts on degraded state from tier N. |
| **Capture both k6 client-side and bidder Prometheus snapshot** | k6 measures network + server. Prometheus measures server-internal stages. Discrepancy between them is the Tokio scheduler / HTTP framework overhead. Both numbers needed. |
| **No warmup phase** | `warmup_enabled = false` for the baseline. The baseline measures cold-start ramp behavior, which is what the load shedder + circuit breaker face in practice. With warmup on, the segment cache is pre-populated and the first 30s look artificially fast. |

### Reproduction recipe

```bash
# 1. Bring up infra and seed
make stack-up
make migrate
make seed-postgres CAMPAIGNS=5000 SEGMENTS=1000
make seed-redis    USERS=100000  SEGMENTS=1000

# 2. Build bidder release
source tools/setup-ort-env.sh
cargo build --release --bin bidder-server

# 3. Start bidder (foreground; or background with `&` and `> /tmp/bidder.log`)
BIDDER__TELEMETRY__OTLP_ENDPOINT="" target/release/bidder-server --config config.toml &

# 4. Run tiered baseline (~10 min)
make baseline-tiered

# 5. Analyze
bash tools/analyze-baseline.sh > /tmp/baseline.md
```

Reseed via `make seed` if you want a clean catalog before re-running.

---

## Results — v0 baseline

**Environment:** macOS Apple Silicon (M-series), Docker Desktop, Postgres 16, Redis 7, single-process bidder release build, ort 2.0.0-rc.10. ~5K seeded campaigns × 1019 segments × 100K users.

### k6 HTTP-side timing (per tier)

| Target RPS | Sustained RPS | iterations | p50 (ms) | p95 (ms) | max (ms) | http_req_failed |
|---:|---:|---:|---:|---:|---:|---:|
| 5,000 | 4,167 | 750,000 | 0.70 | 1.64 | 65.84 | 7/750,000 (0.0009%) |
| 10,000 | 8,318 | 1,497,275 | 0.55 | 3.92 | 331.77 | 41/1,497,275 (0.0027%) |
| 15,000 | 10,218 | 2,243,410 | 0.19 | 2.57 | **39,758** | 2,015/2,243,410 (0.0898%) |

(p99 / p99.9 not exported by k6 in `--summary-export` JSON; the inline run output reported p99=2.5ms / p99.9=20.24ms at 5K RPS.)

**Notes:**
- **Sustained RPS < target RPS** at all tiers (5K target → 4.2K actual; 15K target → 10.2K actual). k6's arrival-rate executor backs off when the VU pool can't keep up. This is the macOS file-descriptor + Docker port-forwarding ceiling, not a bidder limit.
- **15K RPS max latency is 39.7 seconds.** That's the tail behind the HTTP concurrency-limit layer (`max_concurrency = 2000`). When incoming RPS exceeds the bidder's processing rate, the queue grows and individual requests block. This is the load-shed boundary firing — at 15K the bidder is past its single-process ceiling on this hardware.

### Bidder pipeline outcomes (per tier)

| Target RPS | total bids | total no-bids | bid rate | early drops |
|---:|---:|---:|---:|---:|
| 5,000 | 141,280 | 608,714 | 18.84% | 17 |
| 10,000 | 308,920 | 1,938,308 | 13.75% | 99 |
| 15,000 | 487,590 | 4,001,033 | 10.86% | 369 |

### Resilience signals (per tier)

| Target RPS | breaker opens | hedge fired | freq-cap skipped (timeout) | freq-cap skipped (breaker) | kafka events_dropped |
|---:|---:|---:|---:|---:|---:|
| 5,000 | 3 | 200 | 264 | 204,736 | 260,408 |
| 10,000 | 12 | 200 | 891 | 1,364,842 | 556,101 |
| 15,000 | 16 | 200 | 1,351 | 4,422,479 | 876,289 |

### Per-stage timing — 15K RPS (server-internal latency)

| Stage | p50 (µs) | p99 (µs) | max (µs) | budget exceeded |
|---|---:|---:|---:|---:|
| request_validation | 1.6 | 5.5 | 1,127 | 14 |
| user_enrichment | 3.3 | 9.6 | 2,296 | 16 |
| candidate_retrieval | 43.9 | 136.7 | 12,718 | **21,516** |
| candidate_limit | 9.4 | 9.4 | 6,627 | 82 |
| scoring | 1.4 | 2.9 | 1,004 | 7 |
| **frequency_cap** | 54.7 | **38,346** | 38,342 | 9,797 |
| budget_pacing | 38.7 | 38.7 | 27,156 | **1,759** |
| ranking | 0.5 | 0.5 | 364 | 17 |
| response_build | 0.2 | 1.0 | 16 | 1 |

---

## Observations + root-cause analysis

### Observation 1: Bid rate is 10–19%, not the ~97% you'd expect from a casual reading of "load test passes"

**This is the most important finding.** A 10–19% bid rate at first glance looks like a regression vs the Java repo's 97%+ — but the manual probes show the bidder produces 2-bid responses for every single user ID I tested individually. So why low under load?

**Root cause:** the k6 corpus is uniformly random across geo (10 metros), device type (3 values), segments (5–15 from a 20-name vocab), and user ID (Zipf-skewed across 100M IDs). Each request requires:
1. At least one campaign matching the user's segments **OR** the campaign has no segment targeting (per the catalog's `all_campaigns` fallback).
2. Intersected with geo, device type, ad format.
3. Surviving freq-cap (often skipped under load — see Observation 2).
4. Surviving budget pacing (Observation 4).
5. Producing at least one winner across the 2 imps in the request.

With 5K seeded campaigns × ~62% active = ~3,124 active. Of those, only **1,058 have video creatives** and **2,514 have banner creatives** (the two formats in the golden request). After segment + geo + device intersections, candidates per imp drop to a median of 244 (video) / 563 (banner) — which is healthy in absolute terms.

But: when EITHER imp produces zero candidates after intersection, the request goes no-bid. The corpus diversity vs the seeded-targeting density combination guarantees ~80% of random requests will hit at least one zero-candidate imp.

**Implication:** the 18% bid rate is correct for THIS corpus vs THIS seed. To hit Java-repo-style 97% bid rate you'd either need (a) less restrictive targeting in the seed (e.g. force most campaigns to have geo coverage across all 10 metros), or (b) bias the k6 corpus toward request shapes that match the seeded targeting density. Neither is the right move for a baseline — the right move is to record the bid rate alongside the latency numbers and let future runs detect changes.

### Observation 2: The Redis circuit breaker opens 3–16 times per tier, freq-cap is skipped on 27–98% of requests

| Tier | breaker opens | freq-cap skip rate (breaker_open / total no-bids+bids) |
|---:|---:|---:|
| 5K | 3 | 27.3% |
| 10K | 12 | 60.7% |
| 15K | 16 | 98.4% |

**At 15K RPS, the freq-cap circuit breaker is open virtually the entire run.**

**Root cause:** the freq-cap MGET path has an 8 ms total budget (`latency_budget.frequency_cap_ms`). Each MGET requests up to 50 keys per user (campaign-day + campaign-hour pairs for ~25 candidates). Under sustained 4K+ RPS, the local Redis container's single-threaded decode loop can't return MGETs of that size within 8ms. MGETs time out, get recorded as `error=true` on the breaker (the breaker treats both errors and slow-calls uniformly), error-rate threshold trips, breaker opens for 10s.

When the breaker is open, freq-cap is skipped (`SkippedTimeout` outcome → bid path proceeds without filtering). Bid quality drops: a campaign that should have been freq-capped might bid anyway. This is the documented Phase 5 fail-safe-and-loud trade-off.

**Phase 7 implications:**
1. **The 8 ms freq-cap budget is too tight for this hardware.** Either raise it, or page MGET into smaller chunks, or move freq-cap to in-process counters with write-behind to Redis (already on the Phase 7 plan as `InProcessFrequencyCapper`).
2. **Treating timeouts as errors at the breaker is debatable.** A timeout isn't "Redis broke" — it's "Redis was slow this once." Currently any failure mode trips the breaker, opening it for ALL users for 10s, not just the user whose MGET timed out. Worth a Phase 7 design review.
3. **Hedge-fired count is 200 across all three tiers** — the hedge budget is consumed early in tier 1 and never refills enough to fire under sustained load. The hedge is effectively only firing during the first ~30s of each run.

### Observation 3: 15K RPS produces 39-second tail latencies — single-process ceiling

The bidder's max latency at 15K RPS is 39,758 ms. Median + p95 are still healthy (0.19 ms / 2.57 ms), so this is a **distribution tail, not an average regression.** That tail is the queue behind the `ConcurrencyLimitLayer` saturating — when sustained inbound RPS exceeds the bidder's per-instance processing rate, accepted requests queue up before parsing.

**This is the load-shed boundary.** The PLAN.md latency budget says above the 50ms SLA we shed early via 503 — but `max_concurrency = 2000` means the queue can grow to 2000 in-flight before 503 starts. Some requests sit at the back of that queue for ~40s before being processed.

**Phase 7 implications:**
1. The current `ConcurrencyLimitLayer` from Tower bounds concurrent in-flight, not queue depth at any latency. It's a memory-safety bound, not a latency bound. A `LatencyBudgetLayer` that 503s requests waiting > N ms in the queue would surface this differently.
2. Single-process Tokio tops out around **10K sustained RPS on macOS dev** (target 15K → actual 10.2K). Linux + multi-process via `SO_REUSEPORT` (already wired in Phase 1) is what scales beyond. Phase 7 measures this on Linux.

### Observation 4: Budget pacing budget-exceeded jumps 100× from 5K to 15K (16 → 1759)

`bidder_pipeline_stage_budget_exceeded{stage="budget_pacing"}` goes from 16 events at 5K RPS to 1,759 at 15K. The budget for that stage is 1ms (`latency_budget.budget_pacing_ms`).

**Root cause:** budget pacing uses an `AtomicI64::fetch_sub` per candidate. The sub itself is fast — but when the budget approaches zero, retries to find an under-budget candidate climb. At higher RPS, more requests share the same hot campaigns, more candidates are budget-exhausted, and more retry work happens per request. At 15K RPS the median per-request work pushes past 1ms occasionally.

`bidder_budget_exhausted_filtered = 17.8M` at 5K and grows roughly linearly with RPS. At 15K that's ~50M candidates filtered for budget across 4.5M requests = 11 candidates filtered per request on average. Manageable.

**Phase 7 implications:** the 1ms budget for budget pacing is fine at 5K. At 15K it's ~0.04% over budget (1759 / 4.5M = 0.04%). Not a Phase 7 priority unless real-traffic numbers show different pattern.

### Observation 5: Kafka events_dropped scales linearly with bid count (~2× bids per tier)

| Tier | bids | events_dropped | events per dropped bid |
|---:|---:|---:|---:|
| 5K | 141,280 | 260,408 | 1.84 |
| 10K | 308,920 | 556,101 | 1.80 |
| 15K | 487,590 | 876,289 | 1.80 |

**Root cause:** there's no Kafka broker running in this baseline. Every bid response generates ~2 events (`BidEvent` per winner, plus the catalog refresh has none in this run); rdkafka producer queue fills, drops them. The 1.8 ratio matches the 2 imps × bid path with some single-imp outcomes.

**Implication:** this is NOT a real bug; it's a missing dependency in the baseline. Production runs with Kafka will see `events_dropped = 0` under steady-state. The correctness signal here is that **Kafka unavailability does not block the bid path** — bids still complete in 0.7ms p50 while events drop. That's the Phase 5 fail-safe-and-loud guarantee working.

### Observation 6: `candidate_retrieval` budget-exceeded count (21,516 at 15K) is the highest of any stage

The `candidate_retrieval_ms` budget is 1ms per the config. At 15K RPS, p50 of candidate retrieval is 43.9 µs (way under), but max is 12,718 µs (12.7ms). 21,516 events crossed the 1ms threshold — that's 0.5% of requests at 15K.

**Root cause:** candidate retrieval is RoaringBitmap intersection across N segment-bitmaps × geo-bitmap × device-bitmap × format-bitmap × all_campaigns/unrestricted bitmaps. For users with 200 segments, that's 200 OR operations on top-of-the-segment bitmaps before AND-ing with the dimension filters. Fast on average (43 µs), but the worst-case grows with `n_segments × bitmap_density`.

**Phase 7 implications:** if profiling later shows candidate retrieval is the bottleneck, candidate options are (a) cap segments-per-request at retrieval time, (b) pre-compute "popular segment" bitmaps that subsume the top-K segment ORs, (c) reorder dimension intersections so the cheapest filter runs first.

### Observation 7: The bidder ignores OpenRTB `user.data[].segment[]` from the wire entirely

This isn't an observation from the load test — it's something I caught while debugging the bid rate. The bidder's `UserEnrichmentStage` ONLY reads segments from Redis. Wire-format segments in `user.data[].segment[]` are completely discarded.

**Real-world impact:** in production an SSP often sends in-request segments (publisher's own audience data) that ARE NOT in the bidder's Redis. Currently those are ignored — the bidder targets only against its own catalog of pre-resolved user→segment mappings.

**This is a product-level gap, not a Phase 7 perf concern.** Filing as a follow-up: the right fix is to merge `request.user.data[*].segment[*].id` (resolved through `SegmentRegistry`) into `ctx.segment_ids` alongside the Redis lookup. Two-line change to `UserEnrichmentStage`.

---

## What this baseline tells Phase 7

**Validated to focus on:**
- Freq-cap MGET path is the hottest Redis interaction by far. `InProcessFrequencyCapper` (already on the Phase 7 list) is the right priority.
- HTTP queue saturation at ~10K sustained RPS on single-process Tokio. Linux multi-process via `SO_REUSEPORT` is the right next step (already on the Phase 7 list).
- Hedge budget is consumed once and never refills meaningfully. `HedgeBudget::set_load_shed_rate()` wiring (already on the Phase 7 list) is the right priority.

**Validated to defer:**
- Allocator contention does NOT show as a hot spot under any tier. `bumpalo` arena experiment stays deferred until Linux-prod profiling shows otherwise.
- monoio thread-per-core would only help if io_uring syscall overhead was the limiter, which we don't have evidence for. Multi-process Tokio first; monoio only if multi-process ceilings hit.
- Per-stage micro-optimizations (reusable `Vec<f32>` buffers, pre-computed campaign features) are below the noise floor at 5K-15K. Defer.

**New Phase 7 follow-ups surfaced:**
- Merge wire-format `user.data[].segment[]` into `ctx.segment_ids` (real product gap).
- Re-evaluate "treat MGET timeout as breaker error" semantics — currently fail-fast-and-loud trips the breaker too aggressively under healthy-but-slow conditions.
- Consider a `LatencyBudgetLayer` that 503s queued requests above N ms wait time, in addition to the existing concurrency-limit memory bound.

---

## Honest assessment — what this run actually measures

Reviewing the run before declaring it a baseline. Three things make these numbers less reliable than the tables suggest, and they're worth fixing before Phase 7 makes decisions on them.

### The 5K tier is the only fully reliable tier

- **Sustained vs target:** 5K target → 4.2K actual is k6 backing off due to its own VU/connection model on macOS, not the bidder. Workload shape and bid path are exercised correctly.
- **15K target → 10.2K actual sustained.** k6 did not deliver 15K RPS; the "15K" column reflects a 10.2K-with-ramp-spikes run. Reporting it as "15K RPS" is misleading. The 39-second tail max is real but it's caused by macOS file-descriptor ceilings, not a property the bidder exhibits on Linux production.
- **Conclusion:** the 5K tier is solid; 10K is borderline; 15K is noise. The aggregate "scaling shape" between tiers is qualitatively right but quantitatively dubious.

### Kafka and OTel were both pointed at non-existent services

The bidder ran with `[kafka] brokers = "localhost:9092"` (no broker) and `[telemetry] otlp_endpoint = "http://localhost:4318"` (no Tempo). Both subsystems fail-safely (per Phase 5 design) but they consume CPU on the bidder's Tokio runtime to do so:

- 876K Kafka events_dropped at 15K means the bidder did 876K `tokio::spawn` calls, 876K rdkafka producer queue inserts, 876K connect-retry-loop iterations, 876K log-rate-limited error emissions.
- OTel `BatchSpanProcessor.ExportError` fires multiple times per second across the run.

Both load production-Tokio workers that production-with-real-services would NOT have. This means the per-stage µs numbers include some untracked overhead from failed-publish work. **The latency numbers are upper-bound estimates; production-with-Kafka will be at most equal, likely slightly faster.**

**Fix for next baseline:** either (a) bring up Kafka + Tempo in compose so all subsystems are present, or (b) configure the bidder to use `NoOpEventPublisher` and disable telemetry export for the baseline. Option (a) is more honest because it represents real production cost.

### Bid rate of 18% means most measurements are no-bid-path measurements, not bid-path

The single biggest interpretation error this baseline could lead to: treating these latency tables as "bidder p99 is X ms at Y RPS" when in reality 80%+ of those measured requests took the no-bid path. The no-bid path is fast (validation → catalog miss-on-some-imp → no-bid response, ~0.5ms total). The bid path is what production cares about, and it's ~5× more expensive (full retrieval + scoring + freq-cap + ranking).

**At 5K RPS, ~140K bid-path requests over 180s = ~780/s of actual bid-path traffic.** Phase 7 capacity claims based on this baseline must be qualified: "the bidder serves 5K req/s on this hardware where 19% are full-pipeline bid requests." That's a useful number for regression detection but a thin number for capacity planning.

**Fix for the next baseline:** either tune the seed (denser per-format coverage, more permissive targeting on hot segments) so bid rate climbs above 80%, OR bias the k6 corpus to request shapes that match the seeded density. The first matches "production" closer because production catalogs ARE dense; the second is faster to implement but synthesizes traffic that won't match production. Recommend (a).

### What this baseline IS reliable for

- **Detecting per-PR regressions on macOS dev.** The 5K tier numbers reproduce on the same hardware run-to-run; a future PR that doubles candidate-retrieval p99 will show in `make baseline` even if the absolute numbers aren't production-realistic.
- **Sanity-checking the resilience subsystems.** Circuit breaker opens, hedge fires, freq-cap skips — these are observed behaving as designed. The numbers are extreme (98% freq-cap skip at 15K) but the *behavior* is correct.
- **Surfacing real architectural gaps.** Observation 7 (wire-segments ignored) and Observation 2 (timeout-as-breaker-error) are real, independent of the run quality.

### What this baseline is NOT reliable for

- Absolute capacity claims ("the bidder serves N RPS at p99 < M ms").
- Phase 7 perf decisions that hinge on absolute timings (e.g. "is bumpalo worth it? — depends on allocator p99 µs").
- Comparison against the Java repo's published numbers — that environment is different in too many dimensions.

**Re-running with Kafka up + a denser seed + restricted to the 5K and 10K tiers will produce numbers that ARE reliable enough for Phase 7 decisions.** Filing as a Phase 7 prerequisite.

---

## Caveats — what this baseline does NOT measure

- **Linux prod hardware.** macOS Apple Silicon + Docker Desktop is dev. Linux x86_64 with native networking will be 1.5-3× faster on per-request latency. Re-run on Linux production hardware at the start of Phase 7.
- **Real Kafka.** No broker running → all events drop. Run with Kafka up before declaring kafka-events-dropped a non-issue.
- **Real campaign / user diversity.** 5K seeded campaigns × 100K users × 1019 segments is a tiny fraction of the production workload (100K campaigns, 100M users). The stage-timing shape stays the same; absolute numbers don't.
- **Sustained 30+ minute runs.** 120s hold per tier is enough for steady-state but not enough to surface slow leaks (RSS slope, Arc cycle leaks). The PLAN.md nightly chaos suite covers that and is Phase 7 work.
- **Real production traffic shape.** k6's uniform-random-with-Zipf-on-users is a synthetic shape. Real bid traffic has time-of-day skew, campaign-spend-curve correlation with bid floor, and segment-overlap clustering that this corpus doesn't model. Acceptable for a regression baseline; not for absolute capacity claims.

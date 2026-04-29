# Architecture

This document describes how `rust-rtb-bidder` is put together: the layers, what each layer owns, and how a request flows through them. It is meant as a single read-through for someone new to the codebase. For *why* a particular design choice was made, see [PLAN.md](PLAN.md) and the per-phase notes under [docs/notes/](notes/).

## High-level data flow

```
                         ┌────────────────────────────────────┐
                         │       Exchange (SSP / AdX)         │
                         └──────────────┬─────────────────────┘
                                        │ HTTP POST /rtb/openrtb/bid
                                        │ HTTP GET  /rtb/win?...
                                        ▼
┌──────────────────────────────────────────────────────────────────────┐
│  L1  Transport (axum + tower)                                        │
│      timeout • concurrency-limit • TraceLayer • metrics              │
│      LoadShedTracker  (feeds hedge feedback loop)                    │
└──────────────────────────────────────────────────────────────────────┘
                                        │ Bytes (request body)
                                        ▼
┌──────────────────────────────────────────────────────────────────────┐
│  L2  Exchange adapter  (trait ExchangeAdapter)                       │
│      decode_request : &mut Vec<u8>  → BidRequest                     │
│      encode_response: &BidResponse  → (Vec<u8>, content-type)        │
│      impls: OpenRtbGeneric (simd-json) • GoogleAdxAdapter (prost)    │
└──────────────────────────────────────────────────────────────────────┘
                                        │ BidContext { request, … }
                                        ▼
┌──────────────────────────────────────────────────────────────────────┐
│  L3  Pipeline                                                        │
│      ordered Vec<Box<dyn Stage>> with per-stage budget + deadline    │
│                                                                      │
│   ┌──────────────────────┐ validate shape, OpenRTB invariants        │
│   │ RequestValidation    │                                           │
│   ├──────────────────────┤ user.id → segments[]                      │
│   │ UserEnrichment       │  → SegmentCache (moka) → Redis hedged GET │
│   ├──────────────────────┤ CampaignCatalog inverted-index lookup     │
│   │ CandidateRetrieval   │  → Vec<Candidate> (RoaringBitmap fanout)  │
│   ├──────────────────────┤ truncate to max_candidates (config)       │
│   │ CandidateLimit       │                                           │
│   ├──────────────────────┤ Scorer (FeatureWeighted | Cascade | ML |  │
│   │ Scoring              │   AbTest)  → bidder_pctr, score           │
│   ├──────────────────────┤ in-process moka cap → write-behind Redis  │
│   │ FreqCap              │                                           │
│   ├──────────────────────┤ LocalPacer (token-bucket per campaign)    │
│   │ BudgetPacing         │                                           │
│   ├──────────────────────┤ sort + tie-break, 1st/2nd-price logic     │
│   │ Ranking              │                                           │
│   ├──────────────────────┤ build BidResponse, sign nurl with HMAC    │
│   │ ResponseBuild        │   → Bid.ext: pCTR + score                 │
│   └──────────────────────┘                                           │
└──────────────────────────────────────────────────────────────────────┘
                                        │ BidResponse
                                        ▼  (back through L2 encode → L1)
                                        │
                                        │ side-effects (fire-and-forget):
                                        │   • ImpressionRecorder → Redis
                                        │   • EventPublisher    → Kafka
                                        ▼
                              wire bytes → exchange
```

Win-notice path is symmetric but small: `GET /rtb/win` → `WinNoticeGateService` (HMAC verify + Redis `SET NX` dedup) → enqueue `AdEvent::Win` to Kafka. Same `LoadShedTracker`, separate `ConcurrencyLimitLayer` capped at 10% of bid concurrency.

## Layers

### L1 — Transport (`bidder-server/src/server/`)

`axum` over `tokio`, behind a `tower::ServiceBuilder` stack. One `Router` per route family because each family has its own concurrency cap and timeout:

| Route                     | Timeout       | Concurrency               |
| ------------------------- | ------------- | ------------------------- |
| `POST /rtb/openrtb/bid`   | 50 ms (config)| `max_concurrency`         |
| `GET  /rtb/win`           | win-timeout   | `max_concurrency / 10`    |
| `/health/{live,ready}`    | none          | none                      |
| `/metrics`                | none          | none                      |

Middleware applied:

- **`timeout_middleware`** (custom) — `tokio::time::timeout`. On timeout or downstream `503` it bumps `LoadShedTracker::record_shed`. On every entry it bumps `record_request`. Those two counters feed the hedge feedback loop.
- **`tower::limit::ConcurrencyLimitLayer`** — bounded inflight; rejects with `503` when full. The `503` is itself a load-shed signal.
- **`TraceLayer`** — `tower-http` request span; honours OTel context if present.
- **`MetricsLayer`** — request count + latency histogram, labelled by route + status.

Optional `SO_REUSEPORT` socket binding lives in [server/socket.rs](../bidder-server/src/server/socket.rs) for multi-process Linux deploys (kernel-level fanout, no userspace LB needed).

### L2 — Exchange adapter (`bidder-core/src/exchange/`)

Single trait, one impl per wire protocol:

```rust
trait ExchangeAdapter: Send + Sync {
    fn id(&self) -> ExchangeId;                                  // "openrtb" | "google-adx"
    fn decode_request(&self, body: &mut Vec<u8>) -> Result<BidRequest>;
    fn encode_response(&self, resp: &BidResponse) -> Result<(Vec<u8>, ResponseContentType)>;
}
```

Implementations:

- **`OpenRtbGeneric`** — simd-json parse over a mutable `Vec<u8>` (mandatory: simd-json mutates the buffer in place). `Bytes → Vec<u8>` is a memcpy in steady state because hyper pools its read buffers; documented in [handlers.rs](../bidder-server/src/server/handlers.rs).
- **`GoogleAdxAdapter`** — `prost` decode/encode against an in-repo slim `adx.proto` (see [bidder-protos/proto/adx.proto](../bidder-protos/proto/adx.proto)). Wire-compatible scaffolding, not production-ready against live AdX (no hyperlocal decryption, no signed nurl macro substitution).

Adapters are stateless. The handler decodes, runs the pipeline, encodes back, returns the bytes.

### L3 — Pipeline (`bidder-core/src/pipeline/`)

`Pipeline` is an ordered `Vec<Box<dyn Stage>>` driven by [pipeline/mod.rs](../bidder-core/src/pipeline/mod.rs). Two enforcement mechanisms:

1. **Per-stage budget** — declared in `[latency_budget]` config. Overruns emit `bidder.pipeline.stage.budget_exceeded{stage=…}` (metric, not abort).
2. **Pipeline deadline** — checked *before* each stage. If `ctx.elapsed_ms() ≥ pipeline_deadline_ms`, the run short-circuits to `NoBid(PIPELINE_DEADLINE)`. This is the hard cap.

Stage order (wired in [main.rs](../bidder-server/src/main.rs)):

| # | Stage                  | Reads                          | Writes                       | Outside calls            |
| - | ---------------------- | ------------------------------ | ---------------------------- | ------------------------ |
| 1 | RequestValidation      | `BidRequest`                   | early `NoBid` on invariant   | —                        |
| 2 | UserEnrichment         | `request.user.id`              | `ctx.segments`               | SegmentCache → Redis     |
| 3 | CandidateRetrieval    | catalog inverted index         | `ctx.candidates`             | — (in-memory)            |
| 4 | CandidateLimit         | `ctx.candidates`               | truncated `ctx.candidates`   | —                        |
| 5 | Scoring                | candidates + features          | `bidder_pctr`, `score`       | optional ONNX (in-proc)  |
| 6 | FreqCap                | `(user, campaign)` counters    | filter capped candidates     | InProcCache + Redis      |
| 7 | BudgetPacing           | per-campaign token bucket      | filter exhausted             | LocalPacer (in-memory)   |
| 8 | Ranking                | scored candidates              | winner(s) + clearing price   | —                        |
| 9 | ResponseBuild          | winners                        | `BidResponse` + signed nurl  | HMAC sign                |

Stages communicate strictly via `BidContext` (model/context.rs). A stage can set `ctx.outcome = NoBid(reason)` to stop downstream execution without erroring.

### L4 — Domain services (`bidder-core/src/`)

These are owned by `AppState` (see [server/state.rs](../bidder-server/src/server/state.rs)) and injected into stages via `Arc<dyn Trait>`:

- **`CampaignCatalog`** ([catalog/](../bidder-core/src/catalog/)) — read-only after build. Inverted indices on segment / geo / device / format → `RoaringBitmap` of campaign ids. `candidates_for()` always returns a fresh owned bitmap. Hot reload via background loader.
- **`SegmentCache`** ([cache/](../bidder-core/src/cache/)) — moka in-process LRU in front of Redis. Cache-aside, populates on miss.
- **`Scorer` trait** ([scoring/](../bidder-core/src/scoring/)) — pluggable. `FeatureWeighted` (linear), `Cascade` (rule fallthrough), `Ml` (ONNX), `AbTest` (split traffic across two scorers).
- **`FrequencyCapper` + `ImpressionRecorder`** ([frequency/](../bidder-core/src/frequency/)) — `InProcessFrequencyCapper` wraps a `moka::sync::Cache<UserId, Arc<UserCapMap>>`. Hot path is sub-µs, lock-free in steady state. Eviction listener flushes counters to Redis via the write-behind channel — no silent loss. The `ImpressionRecorder` drains the channel and writes to Redis.
- **`BudgetPacer`** ([pacing/](../bidder-core/src/pacing/)) — `LocalPacer`: token-bucket per campaign, in-memory.
- **`CircuitBreaker`** ([breaker/](../bidder-core/src/breaker/)) — latency-aware (not just error-rate) Closed → Open → HalfOpen state machine. Wraps Redis calls in `segment_repo.rs` and `freq_cap.rs`.
- **`HedgeBudget`** ([hedge.rs](../bidder-core/src/hedge.rs)) — bounded retry budget; only hedges Redis MGETs when load_shed_rate is low and Redis p95 is high.
- **`HedgeFeedbackLoop`** ([hedge_feedback.rs](../bidder-core/src/hedge_feedback.rs)) — periodic task that reads `LoadShedTracker` + `RedisLatencyTracker` and updates `HedgeBudget` thresholds. Self-instrumented for tick drift.
- **`EventPublisher`** ([events.rs](../bidder-core/src/events.rs)) — sync trait. Production impl is `KafkaEventPublisher` with `BaseProducer` + dedicated poll thread. `record_published()` fires from the delivery callback only (true ack), `record_dropped()` from `QueueFull` or callback error.
- **`KafkaIncidentState`** ([kafka_incident.rs](../bidder-core/src/kafka_incident.rs)) — auto-flips drop policy `Newest → RandomSample` when drop rate >1% over a 5-minute dwell window; auto-reverts when normalised.
- **`WinNoticeGateService`** ([win_notice.rs](../bidder-server/src/win_notice.rs)) — HMAC-SHA256 with `subtle::ConstantTimeEq`, per-SSP secrets keyed by `exchange_id` with a default fallback, Redis `SET NX` dedup.

## Stage deep-dives

The L3 table above gives the one-line summary of each pipeline stage. This section drills into the stages where the implementation choice has real performance implications — the ones worth understanding when reading flame graphs or chasing a regression.

### Frequency cap — the most expensive stage at high RPS

The freq-cap stage answers "should I drop this candidate because we've already shown the user too many of this campaign's ads today/this hour?" Two implementations exist behind the same `FrequencyCapper` trait, switched via `[freq_cap].in_process_enabled` in `config.toml`.

#### Why this stage matters

In Phase 6.5 baseline measurements, freq-cap was the only pipeline stage that routinely exceeded its budget under load. Stage-attribution data from a 10K-RPS run with the Redis-backed implementation:

```
Stage                 Avg µs    Share    Over budget
─────────────────────────────────────────────────────
frequency_cap            724    73.5%         4,171
candidate_retrieval      241    24.4%         1,827
budget_pacing              9     0.9%
ranking                    6     0.4%
[everything else]         <5     1.0%
```

One stage owned almost three-quarters of the request budget. Optimising it has the largest single-leverage impact in the bidder.

#### Implementation A: `RedisFrequencyCapper` (default until Phase 8)

```
                 ┌──────────────────┐
                 │ FreqCap stage    │ for ~10–50 candidates per request
                 └────────┬─────────┘
                          │ build N keys: v1:fc:{u:42}:c:7:d, v1:fc:{u:42}:c:7:h, …
                          ▼
                 ┌──────────────────┐
                 │ Redis MGET ×N    │ ~1ms RTT on Docker for Mac
                 │ (network bound)  │
                 └────────┬─────────┘
                          │ 50 small u64 values back over the wire
                          ▼
                 ┌──────────────────┐
                 │ Decode + compare │ for each candidate, check
                 │ to per-cap limit │ daily_count >= cap_d || hourly_count >= cap_h
                 └────────┬─────────┘
                          │
                          ▼
                       FreqCapOutcome::Checked(per-candidate verdicts)
```

Cost profile: dominated by the network round-trip. Decode + compare is sub-µs; the round-trip on Docker for Mac is 1–3ms. This is **inherent to crossing the Docker network**, not a Rust performance issue.

#### Implementation B: `InProcessFrequencyCapper` (Phase 8, opt-in)

```
                 ┌──────────────────┐
                 │ FreqCap stage    │
                 └────────┬─────────┘
                          │
                          ▼
                 ┌─────────────────────────────────────┐
                 │ moka::sync::Cache lookup            │
                 │   key   = user_id (String)          │
                 │   value = Arc<UserCapMap>           │  ~0.5 µs hash + atomic load
                 │           per-campaign atomic       │
                 │           counters                  │
                 └────────┬────────────────────────────┘
                          │
            ┌─────────────┴──────────────┐
            │ HIT (warm user)            │ MISS (cold user)
            ▼                            ▼
   compare counters in-RAM       fall through to RedisFrequencyCapper
   ~0.1 µs per candidate         (one-time cost; cache it for next call)
            │                            │
            └──────────────┬─────────────┘
                           ▼
                    FreqCapOutcome::Checked(verdicts)
                           │
                           │ ALSO (asynchronous, never blocks the bid):
                           ▼
                 ┌─────────────────────────────────────┐
                 │ tokio::sync::mpsc bounded channel   │
                 │ (write-behind queue, depth 65k)     │
                 └────────┬────────────────────────────┘
                          │ drained by N workers
                          ▼
                 ┌─────────────────────────────────────┐
                 │ ImpressionRecorder (worker pool)    │
                 │ INCR v1:fc:{u:…}:c:…:d              │ Redis writes batched,
                 │ EXPIRE … (TTL refresh)              │ never on the bid path
                 └─────────────────────────────────────┘
```

Two more invariants worth knowing:

```
                 ┌─────────────────────────────────────┐
                 │ moka eviction listener              │
                 │ (fires when LRU drops a UserCapMap) │
                 └────────┬────────────────────────────┘
                          │
                          ▼
                 ┌─────────────────────────────────────┐
                 │ Walk the evicted map, push every    │
                 │ counter snapshot onto the same      │
                 │ write-behind queue → workers        │
                 │ flush to Redis.                     │
                 │ Net: no in-process-only counters    │
                 │ silently disappear when an entry    │
                 │ ages out.                           │
                 └─────────────────────────────────────┘

                 ┌─────────────────────────────────────┐
                 │ Circuit breaker shared with         │
                 │ RedisFrequencyCapper                │
                 └────────┬────────────────────────────┘
                          │
                          ▼
                 If Redis is unhealthy and breaker is OPEN, return
                 SkippedTimeout instead of serving from a possibly-
                 stale moka cache. This preserves the Phase 5 fail-
                 safe-and-loud invariant: when the dependency is
                 broken, we don't quietly serve stale data.
```

Cost profile: hot path is **~0.5 µs for repeat users** (most of them, by Zipf), one-time Redis MGET on cold users (then cached). After ~30 seconds at 5K+ RPS the cache is warm for the working set and freq-cap drops from ~73% of pipeline time to a few percent.

#### When to enable

`in_process_enabled = false` (default) is the safe choice for multi-pod deployments without sticky routing — every bid hits Redis, which is the cluster-wide source of truth so two pods bidding on the same user agree.

`in_process_enabled = true` is correct when:
- single bidder instance per user-routing scope (sticky-by-user-id at the LB) **OR**
- a brief inconsistency window (≤ flush_interval) is tolerable for cap accuracy

Phase 8 ADR has the full multi-instance reasoning: [`docs/notes/phase-8-architectural-followups.md`](notes/phase-8-architectural-followups.md).

#### Why this is not just "another tweak"

Switching the trait implementation changes the latency profile of the whole bidder, not just one stage. Because freq-cap dominates pipeline time at high RPS, dropping it from ~700µs to ~5µs cuts total p99 by an order similar to the dominance ratio. This is the difference between p99 = 27 ms (freq-cap on Redis at 10 K RPS, breaches the budget) and p99 in the single-digit ms range (freq-cap in-process).

### Candidate retrieval — second-largest contributor

The `CandidateRetrievalStage` walks the inverted indices on the campaign catalog and intersects them to produce the candidate set:

```
                request: { user_segments, geo, device, format }
                                │
                                ▼
              ┌───────────────────────────────────────┐
              │ CampaignCatalog (built off hot path)  │
              │   segment_to_campaigns:               │
              │     SegmentId → RoaringBitmap         │
              │   geo_to_campaigns:                   │
              │     (kind, code) → RoaringBitmap      │
              │   device_to_campaigns:                │
              │     DeviceType → RoaringBitmap        │
              │   format_to_campaigns:                │
              │     AdFormat → RoaringBitmap          │
              │   daypart_active_now (1 bitmap,       │
              │     refreshed every minute)           │
              └────────────┬──────────────────────────┘
                           │
                           ▼
              ┌───────────────────────────────────────┐
              │ candidates_for(req)                   │
              │   start: Option<RoaringBitmap> = None │ (lazy: don't clone all_campaigns yet)
              │                                       │
              │   for each user segment:              │
              │     result |= segment_to_campaigns[s] │ union
              │   for the user's geo keys:            │
              │     result &= geo_union               │ intersect
              │   for the device type:                │
              │     result &= (device_eligible ∪      │ intersect
              │                device_unrestricted)   │
              │   for the ad format:                  │
              │     result &= (format_eligible ∪      │ intersect
              │                format_unrestricted)   │
              │   if any daypart targeting active:    │
              │     result &= daypart_active_now      │ intersect
              │                                       │
              │   if no filter ever fired:            │
              │     materialise all_campaigns clone   │ Phase 7 lazy-eval
              └────────────┬──────────────────────────┘
                           │
                           ▼
                 RoaringBitmap of candidate campaign ids
                           │
                           ▼
              ┌───────────────────────────────────────┐
              │ Hydrate to Vec<Candidate>             │
              │ (lookup creative + bid floor per id)  │
              └───────────────────────────────────────┘
```

Cost profile is mostly RoaringBitmap intersections, which are bandwidth-bound on the bitmap container size. At 5–10K campaigns this is sub-millisecond steady state. The Phase 7 review found one regression: anonymous traffic (no segments at all) used to trigger a full clone of `all_campaigns` upfront. The Phase 7 follow-up fix made the seed lazy — we now only materialise a concrete bitmap when an actual filter narrows the universe.

### User enrichment — the other Redis hop on the hot path

```
                                request.user.id
                                       │
                                       ▼
                          ┌─────────────────────┐
                          │ SegmentCache (moka) │ in-process LRU,
                          │ get(user_id)        │ ~0.5 µs hash lookup
                          └──────────┬──────────┘
                                     │
                       ┌─────────────┴─────────────┐
                       │ HIT                       │ MISS
                       ▼                           ▼
              return cached segments     ┌─────────────────────┐
                                         │ Redis hedged GET    │
                                         │ v1:seg:{u:<userId>} │
                                         └──────────┬──────────┘
                                                    │ raw LE-packed u32 segment ids
                                                    ▼
                                         ┌─────────────────────┐
                                         │ Decode chunks_exact │
                                         │ store in moka       │
                                         └──────────┬──────────┘
                                                    │
                                                    ▼
                                            return segments
```

The hedged GET is a Phase 5 detail: under specific conditions (load_shed_rate is low, Redis p95 is high, hedge budget allows) we issue a second Redis call after a small trigger delay; first response wins. This trades a bit of Redis traffic for tail-latency stability when one Redis node is briefly slow. The full guard-rail logic is in [`hedge.rs`](../bidder-core/src/hedge.rs); the feedback loop that updates the trigger thresholds based on observed system state is in [`hedge_feedback.rs`](../bidder-core/src/hedge_feedback.rs).

---

### L5 — Persistence

| Store     | Use                                                               | Module                                |
| --------- | ----------------------------------------------------------------- | ------------------------------------- |
| Postgres  | campaign + segment catalog source of truth                        | [repository/](../bidder-core/src/repository/), [catalog/loader.rs](../bidder-core/src/catalog/loader.rs) |
| Redis     | user→segments, freq-cap counters, win-notice dedup keys           | [cache/](../bidder-core/src/cache/), [server/freq_cap.rs](../bidder-server/src/freq_cap.rs), [server/segment_repo.rs](../bidder-server/src/segment_repo.rs) |
| Kafka     | AdEvents (Win, Loss, Impression) — fire-and-forget                | [server/kafka.rs](../bidder-server/src/kafka.rs) |

Hard contracts for keys/columns/segment ids live in [REDIS-KEYS.md](REDIS-KEYS.md), [POSTGRES-SCHEMA.md](POSTGRES-SCHEMA.md), [SEGMENT-IDS.md](SEGMENT-IDS.md). When a layer touches one of these, it implements the contract verbatim.

### L6 — Observability

- **Metrics** — `metrics` crate, exported via `metrics-exporter-prometheus` at `/metrics`. All counter/histogram names are namespaced `bidder.*`.
- **Tracing** — `tracing` + `tracing-opentelemetry`. Head sampling by default (`Sampler::ParentBased(TraceIdRatioBased)`); opt-in tail sampling via OTel collector (`tail_sampling_via_collector = true`, see [docker/otel-collector/config.yaml](../docker/otel-collector/config.yaml)).
- **Health** — `HealthState` exposes `/health/live` (process up) and `/health/ready` (catalog loaded + Redis ping + Kafka producer alive).
- **Grafana** — auto-provisioned dashboard via [docker-compose.yml](../docker-compose.yml).

## Concurrency model

- Single tokio multi-threaded runtime. One worker per CPU by default.
- One `tokio::task` per HTTP request (axum). The pipeline runs entirely on that task; stages are `async` but don't spawn.
- Background tasks (one each):
  - Catalog reload loop (Postgres pull every N seconds)
  - SegmentCache TTL maintenance (moka internal)
  - `ImpressionRecorder` drain → Redis
  - InProcessFrequencyCapper write-behind drain
  - `HedgeFeedbackLoop` tick (default 1s)
  - `KafkaIncidentState` monitor (30s ticks)
- Dedicated `std::thread` for the Kafka poll loop (rdkafka delivery callbacks).
- All inter-task channels are bounded with documented overflow policy. Drops emit dedicated counters (`bidder.freq_cap.in_process.redis_desync_total`, etc).

## Backpressure surfaces (in order of activation)

1. **Bid concurrency limit** (`tower::limit`) → 503.
2. **HTTP timeout** (50 ms) → 503.
3. **Pipeline deadline** (config) → `NoBid(PIPELINE_DEADLINE)` 204.
4. **Circuit breaker open** on Redis dependency → fallback path (no segments / no freq-cap), pipeline continues.
5. **Recorder channel full** → `try_record` returns false; `redis_desync_total` increments; in-process counter is now ahead of Redis until next flush.
6. **Kafka rdkafka queue full** → `record_dropped`; `KafkaIncidentState` may flip to `RandomSample` if sustained.

Each surface emits a metric; none silently absorb load.

## Configuration

Single `config.toml` parsed by `bidder-core::config`. Sections: `[server]`, `[redis]`, `[postgres]`, `[kafka]`, `[telemetry]`, `[latency_budget]`, `[scoring]`, `[freq_cap]`, `[win_notice]`, `[hedge]`. Environment overrides via `BIDDER__<SECTION>__<FIELD>` (double-underscore separator).

## Process model

The production binary is `bidder-server` (Tokio). An experimental `bidder-server-monoio` exists as a separate binary because `Send + Sync` requirements diverge — it is not a feature flag on the main binary. The default Linux deployment topology is multi-process behind `SO_REUSEPORT` (kernel-level fanout, no userspace LB). See [DEPLOYMENT.md](DEPLOYMENT.md).

## Crate layout

```
bidder-core/      pure library — no axum, no fred, no rdkafka
  pipeline/         stages + orchestration
  catalog/          read-only campaign + index
  cache/            segment cache
  scoring/          scorer trait + impls
  frequency/        freq-cap trait + in-proc + recorder
  pacing/           budget pacer
  breaker/          circuit breaker
  hedge.rs          hedge budget
  hedge_feedback.rs feedback loop
  exchange/         adapter trait + OpenRTB + AdX
  events.rs         event publisher trait
  kafka_incident.rs incident-mode state machine
  metrics/          counter + histogram registration
  telemetry/        OTel init
  config/ model/ repository/

bidder-server/    binary — wires everything, owns I/O
  main.rs            startup + shutdown
  server/            axum router, handlers, layers, state
  freq_cap.rs        Redis-backed FrequencyCapper impl
  segment_repo.rs    Redis-backed SegmentRepo impl
  kafka.rs           BaseProducer + delivery thread
  win_notice.rs      WinNoticeGateService

bidder-protos/    prost-generated wire types (events, adx)
bidder-bench/     criterion benches
```

The split is enforced: `bidder-core` has no I/O dependencies. Anything network-, disk-, or process-bound lives in `bidder-server`.

## Data shapes

This section is the concrete view: what bytes show up at each boundary and how they mutate as they flow. Field names match the code so you can grep.

### Input — `POST /rtb/openrtb/bid`

Wire format: JSON, OpenRTB 2.6 (header `x-openrtb-version: 2.6`). Full canonical sample at [tests/fixtures/golden-bid-request.json](../tests/fixtures/golden-bid-request.json). Trimmed shape:

```jsonc
{
  "id": "req-9f3a…",                    // request id, echoed back
  "imp": [{
    "id": "1",
    "banner": { "w": 300, "h": 250 },   // or "video": {...}, "native": {...}
    "bidfloor": 0.15,                    // USD CPM floor
    "bidfloorcur": "USD",
    "tagid": "slot-42"
  }],
  "site": { "id": "pub-7", "domain": "publisher.example.com",
            "page": "https://…", "cat": ["IAB1"] },
  "device": { "ua": "Mozilla/5.0…", "ip": "203.0.113.4",
              "geo": { "country": "USA", "region": "CA", "city": "SF" },
              "devicetype": 2, "os": "iOS" },
  "user": { "id": "user-abc-123",       // ← drives segment lookup
            "buyeruid": "…" },
  "tmax": 100,                           // SSP-side timeout in ms
  "cur": ["USD"],
  "at": 2                                // 1=first-price, 2=second-price
}
```

For Google AdX the wire is protobuf (`application/octet-stream`) defined in [bidder-protos/proto/adx.proto](../bidder-protos/proto/adx.proto): `BidRequest { id: bytes, adslot: AdSlot, site, device, user, geo, … }`. The adapter maps it into the same internal `BidRequest` struct.

### Internal — `BidContext` (lives only in-process)

Defined in [bidder-core/src/model/context.rs](../bidder-core/src/model/context.rs). One per inflight request, mutated stage-to-stage:

```rust
struct BidContext {
    request:    BidRequest,           // decoded once at L2
    started_at: Instant,              // for elapsed_ms() / deadline check
    segments:   Vec<SegmentId>,       // filled by UserEnrichment
    candidates: Vec<Candidate>,       // filled by CandidateRetrieval, mutated by every later stage
    outcome:    PipelineOutcome,      // Bid(BidResponse) | NoBid(reason)
}

struct Candidate {
    campaign_id:  CampaignId,
    creative_id:  CreativeId,
    base_cpm:     f64,                 // catalog floor for this creative
    bidder_pctr:  Option<f32>,         // set by Scoring
    score:        Option<f32>,         // bidder_pctr × base_cpm × tweaks
    // …matched dimensions for tracing/diagnostics
}
```

The pipeline's "shape change" is essentially: `request → +segments → +candidates → +scores → filtered → ranked → BidResponse`.

### Sidecar reads (during pipeline)

- **Redis user → segments** — `GET user:{user_id}:segments` returns a packed `Vec<u32>` of segment ids (see [SEGMENT-IDS.md](SEGMENT-IDS.md)). Cached in moka with a TTL.
- **Redis freq-cap counters** — `INCR fc:{user_id}:{campaign_id}:d:{yyyymmdd}` and `…:h:{yyyymmddHH}`. Read in-process first via `InProcessFrequencyCapper`; the value lands in Redis through write-behind.
- **Postgres campaigns** — read-only at startup + periodic reload, never on the bid path.

### Sidecar writes (after the bid response is encoded)

- **Win notice URL** — embedded in `Bid.nurl`; format:
  ```
  https://bidder.example.com/rtb/win?bid_id={bid_id}&campaign_id={cid}&creative_id={crid}
                                    &auction_price=${AUCTION_PRICE}
                                    &exchange_id=openrtb
                                    &token={hex_hmac_sha256}
  ```
  HMAC scope is `bid_id|campaign_id|creative_id|exchange_id` (excluding `${AUCTION_PRICE}` — the SSP fills that *after* signing).
- **AdEvent → Kafka** — protobuf from [bidder-protos/proto/events.proto](../bidder-protos/proto/events.proto):
  ```
  AdEvent { kind: WIN | LOSS | IMPRESSION, request_id, bid_id,
            campaign_id, creative_id, price_cpm, ts_ms,
            user_id_hash, exchange_id, bidder_pctr, score }
  ```
  Enqueued onto rdkafka's bounded queue from the bid handler; delivery thread does the actual broker write. Drops are accounted by `KafkaIncidentState`.

### Output — bid response

OpenRTB JSON, content-type `application/json`. Empty on no-bid (HTTP 204). Trimmed bid shape:

```jsonc
{
  "id": "req-9f3a…",                    // mirrors request.id
  "seatbid": [{
    "bid": [{
      "id":     "bid-01H…",
      "impid":  "1",                    // matches request.imp[].id
      "price":  1.42,                   // CPM in `cur`
      "crid":   "creative-7",
      "cid":    "campaign-3",
      "adid":   "creative-7",
      "nurl":   "https://bidder.example.com/rtb/win?…&token=…",
      "adm":    "<html>…",              // creative markup (if banner)
      "adomain":["advertiser.com"],
      "w": 300, "h": 250,
      "ext": { "bidder_pctr": 0.0273, "score": 0.0388 }   // analytics passthrough
    }]
  }],
  "cur": "USD"
}
```

For AdX the encoder produces protobuf `BidResponse { id: bytes, ad: [Ad{ max_cpm_micros, buyer_creative_id, adslot_id, nurl, ext_json }, …] }`. `max_cpm_micros = price × 1_000_000`.

### Win-notice path (separate request)

Input — `GET /rtb/win?...` with the URL the bidder itself signed. Handler steps:
1. Parse `WinParams { bid_id, campaign_id, creative_id, auction_price, exchange_id, token }`.
2. `WinNoticeGateService::check(message, token, exchange_id)` — constant-time HMAC verify.
3. Redis `SET NX` `win:{bid_id}` with TTL — drops replays.
4. On first acceptance, enqueue `AdEvent::WIN` to Kafka with `price_cpm = auction_price`.
5. Return `204 No Content` either way (no information leak to attackers).

### Sizes (rules of thumb)

| Object                         | Typical size       |
| ------------------------------ | ------------------ |
| OpenRTB BidRequest JSON        | 1–4 KB             |
| AdX BidRequest protobuf        | 0.5–2 KB           |
| Internal `BidRequest` (heap)   | ~3–8 KB            |
| `Candidate` struct             | ~64–128 B each     |
| Candidates per request (post-`CandidateLimit`) | 50–500 |
| BidResponse JSON               | 0.5–2 KB           |
| AdEvent protobuf               | ~120 B             |

These are the numbers that drive the bottleneck hierarchy: at 50K RPS × 4 KB request, ingress is ~1.6 Gbps; that's why the hot path is JSON parse + Redis hop, not Rust compute.

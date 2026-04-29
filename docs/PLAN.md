# RTB Bidder — Rust

A standalone Rust DSP bidder. Designed from day one for production Linux server deployment (multi-core, NUMA-aware, kernel-tuned), not a laptop benchmark target. Reuses the existing Docker infrastructure for local dev (Redis, Postgres, Kafka, ClickHouse, Prometheus, Grafana, Tempo) but the architecture is shaped for the production environment, not the dev environment.

The goal is to compete with real production DSPs (Criteo, The Trade Desk, Moloco) on per-instance throughput and tail latency at realistic workload — 1M user audience, **50K-100K active campaigns** (mid-size DSP scale; The Trade Desk runs 100K-500K line items; Criteo shards into ~100K per pod), 50 ms p99 SLA, full observability stack. Realistic single-instance SLA-bound throughput on a production Linux server: 25K-50K RPS, with 50K+ as a stretch when kernel tuning and thread-per-core are applied.

The Java repo benched against 1000 campaigns — small enough that linear-scan-with-bitmap-AND was fast. The Rust version targets the actual production catalog size, which forces a fundamentally different candidate-retrieval architecture (inverted indices) — and that's the single largest architectural improvement over the Java baseline.

## Workload assumptions — production scale, not toy scale

Architecture is shaped by workload size. The Java repo benched against numbers small enough that naive designs worked; this plan targets numbers that force the right architecture from day one. Each row below is a deliberate sizing decision, not a default.

| Dimension | Java repo (toy bench) | Real DSP typical | This plan targets | Why this number drives architecture |
|---|---|---|---|---|
| Active campaigns | 1,000 | 50K-500K (TTD: 100K-500K line items; Criteo: ~100K per shard) | **50K-100K** | Forces inverted-index candidate retrieval. Linear scan over 100K campaigns dies; inverted indices keep candidate retrieval <1ms. |
| Segments total (catalog) | 50 | 10K-100K | **10K-100K** | Forces `RoaringBitmap` (compact for sparse sets) instead of `u64` or fixed-size `BitSet`. |
| Segments per user (avg) | ~10 | 50-200 | **50-200** | Affects per-request bitmap-union cost; still <1ms with Roaring. |
| User audience | 1M | 100M-1B | **100M+** | Redis no longer fits in one node's memory; forces user-key sharding, hash-tag-aware key design, and lower in-process cache hit ratio assumptions. |
| Frequency cap counters per user | 3-5 | 10-50 | **10-50** | Drives MGET page size. Single-page MGET stays under 8ms p99 with this count + RTT budget. |
| Impressions per request | 1 (Java OpenRTB subset) | 1-10 | **1-10** | Already supported via `Vec<Imp>` from Phase 2. |
| SLA deadline (p99) | 50 ms | 80-120 ms (most exchanges); 50 ms (premium) | **50 ms** | Aggressive on purpose — forces tail-latency discipline. Real exchanges often grant more, so we have margin in production. |
| RPS per instance (target) | 15K (Java max passing) | 20K-100K depending on hardware | **25K-50K (50K stretch)** | Realistic Linux production server numbers; aligns with public Moloco / Cloudflare ad-tech engineering reports. |

### What this means for the architecture

These numbers aren't aspirational — they directly drive concrete design decisions made elsewhere in this plan:

1. **Inverted indices in Phase 3** (`segment→campaigns`, `geo→campaigns`, etc., stored as `RoaringBitmap`) — required by 100K campaigns, not optional.
2. **RoaringBitmap mandatory** in the Stack decisions table — required by 10K-100K segments.
3. **User key sharding strategy + hash-tag aware Redis keys** — required by 100M+ user audience; called out in the "Redis throughput discipline" section in Phase 3.
4. **Local moka cache sized realistically** (500K hot users, not 1M) — required by 100M+ audience; cache hit ratio is now a tracked SLO metric, not a default-true assumption.
5. **MGET page size tuned to 10-50 freq counters** — drives Phase 4's freq-cap component.
6. **Redis as #1 bottleneck** in the bottleneck hierarchy — confirmed by these numbers; Rust compute stays cheap *because* of inverted indices.

### What we explicitly do NOT scale to (in this project)

- **1B+ user audiences with global active-active replication** — single-region only (already in Out of Scope).
- **1M+ campaigns per pod** — would force a different storage layer (ClickHouse/Pinot for campaign metadata); not in scope. 100K is the upper end here.
- **Sub-10ms p99 SLA** — premium exchanges only; we target 50ms which is the dominant industry standard.
- **Geo-distributed campaign catalog** — single source-of-truth Postgres; multi-region is a deployment-topology concern, not a code concern.

The point is: every number above is a deliberate "this is real DSP scale" decision. If a future deployment hits a workload outside these bounds, the architecture extends (sharding, federated catalogs, etc.) rather than rewriting.

## Why Rust

| Constraint observed in the Java implementation | How Rust changes it |
|---|---|
| ZGC pauses became a tail-latency factor at sustained allocation rate; required heap tuning + allocation reduction passes to stay under the 50 ms p99. | No GC. Allocation pressure manifests as throughput drag, not tail-latency spikes. |
| Lettuce single-decoder-thread saturation required a hand-rolled round-robin connection array. | `fred`'s native multiplexing gets us most of the way there. Round-robin across N connections still useful when Redis-side decode is the bottleneck — verified via profiling, not assumed. |
| Set-of-string segment intersection had to be hand-replaced with a 64-bit bitmap. | Same algorithm, but `serde` zero-copy + lifetimes + bitmap intrinsics (`u64::count_ones`) are idiomatic, not a workaround. |
| Concurrent code (worker pool, freq-cap writes) required runtime review for visibility/happens-before; mistakes compile. | `Send` + `Sync` bounds checked at compile time. Data races become build failures. |
| 100 MB fat JAR, multi-second JVM startup, Maven dependency tree complexity. | ~15-20 MB single static binary, sub-second startup, Cargo handles the build graph. Container cold-start matters at horizontal scale. |
| Thread-per-request was avoided via Vert.x + executeBlocking offload; Vert.x worker pool became the bottleneck at sustained RPS. | Async runtime (Tokio) at the request level + thread-per-core option (`monoio`/`glommio`) on Linux for io_uring-grade perf. |
| Distributed tracing was never added. | `tracing` + `tracing-opentelemetry` make it nearly free; built in from Phase 1. |

## Performance targets

These are the design targets, not promises. Every claim must be JFR-equivalent profile-validated (samply / tokio-console / criterion).

| Tier | Single-instance SLA-bound RPS (50 ms p99) | Notes |
|---|---|---|
| Baseline (Tokio multi-threaded, default tuning) | **20K-25K** | Comparable to or modestly above Java on identical hardware. |
| Tuned (kernel knobs, simd-json, guarded hedged Redis, jemalloc/mimalloc, profiled allocation hotspots closed, batched Redis pipelines) | **30K-40K** | Realistic production ceiling on 12-16 core Linux. |
| Stretch (thread-per-core via monoio + CPU pinning + NUMA-aware deployment + binary internal serialization) | **50K+** | What real DSPs publicly report (Moloco's 7 ms prediction latency at 1M+ QPS implies ~50K-100K per pod with full optimization). |

Tail latency (p99.9) is a first-class design constraint, not an afterthought. Per-stage budgets are declared up front (see "Latency budget" below), enforced via instrumentation, and alerted on.

### Success criteria (operational, not aspirational)

The bidder is "working" when **all** of the following hold under sustained load:

1. **p99 < 50 ms** at the target RPS for the tier (baseline / tuned / stretch).
2. **No cascading failure when a single dependency degrades.** Redis adding +30 ms latency must not push bid p99 over SLA — partial fallback engages instead.
3. **Graceful degradation at 2× expected load.** At 2× RPS, the bidder sheds load and serves a healthy fraction at SLA, rather than collapsing all requests into timeout.
4. **Recovery without restart.** When a degraded dependency recovers, circuit breakers close and full functionality resumes within one minute.

These are go/no-go criteria for each phase that ships a hot path (Phase 4 onward), tested via `k6` chaos scenarios (Redis kill, Redis-add-latency, Kafka pause).

## Expected bottleneck hierarchy

Honest ordering of where latency and throughput limits will appear at 30K+ RPS, based on Java profiling and DSP literature. Profile work prioritized accordingly.

```
1. Redis (network round-trips + decode CPU) — caps the system before Rust does.
   Mitigations: pipeline depth tuning (commands/round-trip), local moka cache,
   in-process hot freq counters with write-behind, request coalescing.
2. JSON parsing (OpenRTB request) — top-3 cost on every request.
   Mitigation: simd-json from day one.
3. Network syscall overhead (recv/send, epoll wakeups) — visible at 25K+ RPS.
   Mitigations: SO_REUSEPORT multi-process, TCP_NODELAY, busy polling, monoio io_uring.
4. Allocation pressure — only at very high RPS, and only after the above are flat.
   Mitigations: jemalloc/mimalloc, SmallVec, bytes::Bytes, optional bumpalo arena (experimental).
5. Rust compute (candidate retrieval, segment match, scoring) — least likely to be the limiter
   *with proper indexing*. Inverted-index intersection over RoaringBitmaps + batched
   scoring keeps this at <1 ms even with 100K campaigns. Without inverted indices,
   compute would dominate by Phase 4 — see Phase 3 for the indexing strategy.
```

If profiling shows compute (#5) dominating before Redis (#1), treat it as a design bug — not a success. The most likely cause is missing/incomplete inverted indices (linear-scanning the catalog).

## Failure philosophy — operational rules

When the system is under stress, behavior is governed by concrete rules, not principles. The single invariant: **never miss the SLA silently**. Drop, degrade, or shed — but always fast and observable.

| Trigger | Action | Metric / Alert |
|---|---|---|
| Pipeline elapsed > 40 ms (10 ms before SLA) | Cancel remaining stages, return 204 No Bid | `bidder.pipeline.early_drop` counter |
| Redis call exceeds **max(p95 latency, 8 ms floor)** (single attempt, idempotent reads) | Issue hedge request (4 guardrails — including hedge budget that contracts when load-shed is active, see Phase 4) | `bidder.redis.hedge_fired` counter, `bidder.redis.hedge_budget_remaining` gauge |
| Freq-cap MGET > 10 ms total (incl. hedge) | Skip freq cap; bid proceeds without freq enforcement | `bidder.freqcap.skipped` counter, alerts at >1% rate |
| HTTP queue depth > N | ConcurrencyLimit layer rejects with 503 before parse | `bidder.load_shed` counter |
| Internal stage queue (scoring, freq-cap workers) full | Drop or fail-fast per stage policy (see Internal backpressure) | per-queue `*.dropped` counter |
| Kafka producer queue full | Drop event per Kafka drop policy (see Phase 5) | `bidder.kafka.events_dropped` counter, alert on rate |
| Circuit breaker open for a dependency | Skip dependency, use fallback/cached path | `bidder.circuit.open` gauge |
| 2× target RPS sustained | Load shed at HTTP layer; healthy fraction served at SLA | `bidder.load_shed_rate` |

Skipping a feature is preferred over blocking the pipeline. Returning 204 fast is preferred over a 50 ms timeout. Bid quality may dip during degradation; SLA is sacred.

## Internal backpressure

Every internal queue, channel, and pool is bounded with a documented overflow policy. Implicit reliance on Tokio scheduling is not enough at 30K+ RPS — overflow is a real failure mode.

| Queue / Channel | Bound | Overflow policy | Why |
|---|---|---|---|
| HTTP accept queue (`ConcurrencyLimitLayer`) | 2× target concurrent in-flight | Reject with 503 | Early shed before parse cost |
| ImpressionRecorder mpsc | 65,536 | `try_send` → drop, increment counter | Bid path must not block on freq-cap writes |
| Kafka producer internal queue | 100,000 events | Drop **newest** (preserve event-stream continuity) | Older events more likely already partially consumed; document choice explicitly |
| Scoring batch buffer | per-request, arena-bounded | N/A (request-scoped) | No cross-request queuing |
| Redis worker pool (if explicit) | N = num_cpus | Round-robin; no separate queue | fred handles its own queueing |
| Circuit breaker half-open probes | 1 in flight at a time | Reject extras | Avoid thundering herd on recovery |

All bounded queues emit `*.depth` gauge and `*.dropped` counter. New queues added in later phases must declare bound + policy in this table.

## Inspiration from production DSP systems

| Source | What we adopt |
|---|---|
| Koah Labs (LGTM stack — Loki, Grafana, Tempo, Mimir) | OpenTelemetry tracing exported to Tempo from Phase 1; structured logs via `tracing` (Loki-compatible JSON output); Prometheus exposition for Mimir. |
| Koah Labs (multi-impression bid request data models) | Full OpenRTB 2.6 `imp[]` array support from Phase 2. Per-impression candidate set, per-impression winner, single response with `seatbid.bid[]` matched 1:1. |
| Koah Labs (flexible engagement event schemas across ad formats) | `enum AdEvent` sum type — Bid, Win, Impression, Click, VideoQuartile, Conversion, ViewabilityMeasured, etc. Compile-time exhaustiveness on every consumer. Extensible without breaking existing code. |
| Koah Labs (low-latency frequency capping) | Paged MGET + bounded write queue (port from Java) plus hedged Redis reads + partial-result fallback. |
| Koah Labs (multi-exchange bidding schemas) | `ExchangeAdapter` trait. Implementations: `OpenRtbGeneric`, `GoogleAdx` (binary protobuf over their endpoint), `Magnite`, `Index Exchange`. Each handles its own request/response variant. |
| Cloudflare edge ad-tech | `SO_REUSEPORT` for multi-process bidding on one node, each process owns its CPU set. |
| Moloco DSP (1M+ QPS fleet) | Aggressive cascade scoring: cheap pre-filter (feature-weighted) → expensive ML only on top-K. Same pattern as Java v5.2; pushes further by adding feature snapshot caching. |
| The Trade Desk / Criteo | Distinct hot-path and cold-path stores. Hot freq counters can live in process with periodic Redis flush; sacrifices brief consistency for latency. Optional Phase 7 deliverable behind a config flag. |
| HFT-adjacent Linux network tuning | `TCP_NODELAY`, `SO_REUSEPORT`, busy polling, IRQ affinity, CPU isolation via `cpuset`/`taskset`, NUMA-aware memory allocation on dual-socket servers. |

## Latency budget — explicit, enforced, alerted

Every stage gets a hard wall-clock budget. Sum stays under the 50 ms p99 SLA with margin for the tail. Each stage emits `histogram!("pipeline.stage.duration", "stage" => name)` and gets a Prometheus alert when its p99 exceeds budget.

```
Total SLA budget: 50 ms (p99)
─────────────────────────────────────────────────────────────────────
HTTP receive + JSON parse (simd-json)            ≤ 2 ms
Request validation                                ≤ 0.5 ms
User enrichment (Redis SMEMBERS, mostly cached)   ≤ 5 ms (cache hit) / 8 ms (cache miss, shared Redis budget)
Candidate retrieval (in-memory bitmap match)      ≤ 1 ms
Candidate limit (top-K via heap)                  ≤ 0.5 ms
Scoring (feature-weighted batched)                ≤ 1 ms
Frequency cap (paged MGET, hedged)                ≤ 5 ms (within shared Redis latency envelope)
Ranking                                           ≤ 0.5 ms
Budget pacing (atomic local)                      ≤ 0.1 ms
Response build + JSON serialize                   ≤ 1 ms
Network send                                      ≤ 1 ms
─────────────────────────────────────────────────────────────────────
Sum:                                              ~17 ms
Slack for tail / GC-equivalent (allocator) / OS:  ~33 ms
```

If any stage's p99 in production exceeds its budget, an alert fires before the SLA does. If the cumulative sum approaches 50 ms, request is dropped early via partial fallback or load shed — not allowed to time out late.

Redis-backed stages share a common latency envelope; overlapping or sequential calls to the same Redis cluster are budgeted against this shared envelope rather than treated as strictly additive per-stage costs.

Per-stage budgets are declared in `config.toml` and instrumented automatically via a `tower::Layer` wrapping each stage. Adding a new stage requires declaring its budget; this is a compile/config check, not a documentation convention.

## Stack decisions

| Concern | Choice | Why |
|---|---|---|
| Async runtime | `tokio` (multi-threaded scheduler) for v1; `monoio` thread-per-core experiment in Phase 7 | Tokio's ecosystem (fred, rdkafka, sqlx, axum) is the practical default. Thread-per-core via monoio + io_uring is a known win on Linux at scale; experiment in Phase 7 once baseline is solid. |
| HTTP server | `axum` 0.8+ on `hyper` 1.x | Type-safe extractors, async, integrates with `tower` middleware (load shed, rate limit, auth, metrics, tracing) declaratively. Industry default. |
| External wire format | OpenRTB 2.6 JSON via `simd-json` (parsing) + `serde_json` (writing). **Important constraint:** `simd-json` requires `&mut [u8]` (it mutates the input in place to resolve escapes / strip whitespace). Therefore the request body buffer must be a per-request **owned mutable `Vec<u8>`** allocated freshly for the parse — never `bytes::Bytes` (immutable, would force a copy and defeat the SIMD win) and never a custom buffer pool (object pools fight Tokio's scheduler and hide allocator-cache benefits). Let jemalloc/mimalloc thread-local caches absorb the allocations; revisit only if flame graphs show allocator contention. | The protocol is dictated by exchanges; SIMD parsing closes ~half the parse cost — but only if the buffer model is right. |
| Internal wire format (Kafka events, inter-service if any) | **Protocol Buffers via `prost`** | Schema-based, codegen, smaller than JSON, faster than Avro. Standard for service-to-service in modern infrastructure. Schema Registry-compatible if Confluent. |
| In-process snapshot serialization (campaign state, etc.) | `rkyv` if zero-copy needed; `bincode` if not | rkyv produces aligned bytes you can `mmap` and use directly — fastest deserialization possible (microseconds). |
| Redis client | `fred` 9.x | Native multiplexing, RESP3, cluster + sentinel + pool modes, async-first. The `redis` crate is older. |
| Redis connection topology | **Round-robin pool of N connections from day one** (typically N = num_cpus or num_cpus / 2). Single-multiplexed-connection is **not** the default — at 30K+ RPS its single decode thread pegs to 100% CPU and silently spikes p99 while Redis itself looks healthy (mysterious internal bottleneck the Java repo hit and the reason it had to retrofit a pool). Start with the pool, benchmark down to fewer connections only if profiling shows pool overhead exceeds decode-parallelism win. | Same lesson as Java, applied earlier this time: client-side RESP3 decode is single-threaded per connection. Parallelize the decode loop or it caps your throughput. |
| Postgres client | `sqlx` 0.8+ | Compile-time-checked SQL via `query!`. Used at startup only — ergonomics > raw speed. |
| Aerospike client | Trait abstraction in place; impl deferred (Rust client is sync-only) | Wrap behind `FrequencyCapper`. If we want it, run sync calls via `spawn_blocking`. Default Redis. |
| Kafka client | `rdkafka` 0.36+ | librdkafka FFI, battle-tested. Pure-Rust alternatives less proven for hot paths. |
| Kafka serialization | `prost` (protobuf) for event payloads | Compact, fast, schema-evolvable. Confluent Schema Registry compatible. |
| ML inference | `ort` 2.x (ONNX Runtime bindings) | Reuses existing `ml/pctr_model.onnx`. Same model, faster runtime. |
| In-process cache | `moka` (W-TinyLFU, async API) | Caffeine-equivalent for Rust. |
| Bitmaps | `roaring::RoaringBitmap` everywhere (mandatory at production scale); `u64` only for the ≤64-segment toy bench | At 50K-100K campaigns × 10K-100K segments, `u64` is unusable. Roaring is the industry standard (Druid, Elasticsearch, Pilosa) — compact for sparse sets, fast intersection, paginatable. |
| Candidate retrieval | **Inverted indices** from each targeting dimension to campaign IDs — segment→campaigns, geo→campaigns, device→campaigns, format→campaigns. Per request, intersect the relevant inverted lists. | Linear scan over `Vec<Campaign>` is O(N) per request and dies above 10K campaigns. Inverted-index intersection is O(K × avg_list_len) where K = number of targeting dimensions. This is how every real DSP does it (Criteo "Cuttle", Moloco "Hopla", AppNexus "Bonsai"). |
| Tracing | `tracing` + `tracing-subscriber` + `tracing-opentelemetry` → OTLP → Tempo. **Head-based sampling from day one.** Default rule: 100% of errors + 100% of SLA violations + 1% of successful fast bids/no-bids (configurable). Tail-sampling at the OTel collector picks up additional anomalies. Without sampling, 50K RPS × ~5 child spans per request = 250K span exports/sec, which silently eats 20-30% CPU and applies backpressure to the hot path. | Async-aware, structured, replaces both logging and tracing in one library — but tracing is **not free** at scale. Sampling is part of the tracing config, not an afterthought. |
| Metrics | `metrics` (facade) + `metrics-exporter-prometheus` | Vendor-neutral. Mimir-compatible exposition. |
| Configuration | `figment` | Multi-source merge: TOML + env vars + CLI args, single resolution order. |
| Memory allocation | Default: jemalloc (or mimalloc) global allocator. Arena (`bumpalo`) is **experimental, off by default** — Phase 7 only, behind a feature flag, after profiling proves allocator contention. | jemalloc/mimalloc are excellent and almost always sufficient. Arena lifetimes across `.await` boundaries are fragile in async Rust; the benefit only appears once allocator is genuinely hot in flame graphs. |
| Profiling | `samply` (sampling), `tokio-console` (async runtime introspection), `cargo flamegraph` | Replaces JFR/JMC. `tokio-console` is the killer feature — no JVM equivalent. |
| Benchmarking | `criterion` | JMH-equivalent. Statistically rigorous. |
| Load testing | k6 (unchanged) | Same scripts, same Makefile targets. |
| Build | Cargo workspace from day 1: `bidder-server`, `bidder-core`, `bidder-bench`, `bidder-protos` (proto-generated types) | Workspace lets us swap `bidder-server` impl (axum vs custom) and share `bidder-core` between server, bench, and any future tooling. |

## Architectural patterns

### Layered architecture with `tower::Layer` for cross-cutting concerns

```
External request
      │
      ▼
┌────────────────────────────────────┐
│ axum routes                        │
│   ↓                                │
│ ConcurrencyLimitLayer (load shed)  │── early reject before parse if queue depth > threshold
│   ↓                                │
│ TimeoutLayer (50 ms hard deadline) │── tokio::time::timeout at the layer level
│   ↓                                │
│ TracingLayer (OTel span)           │── automatic span on every request
│   ↓                                │
│ MetricsLayer                       │── automatic histogram on duration, counter on outcome
│   ↓                                │
│ BidRequestHandler::handle          │── parse, build context
│   ↓                                │
│ Pipeline::execute                  │── 8-stage processing with per-stage budget enforcement
└────────────────────────────────────┘
```

Cross-cutting concerns live in `tower::Layer`s, not the handler or pipeline. Adding rate-limiting, auth, request-id propagation, etc. is a layer composition change, not a code change in the bid path.

### Trait-driven plugin architecture for swap points

| Trait | Selectable at | Implementations |
|---|---|---|
| `FrequencyCapper` | Startup (config) | `RedisFrequencyCapper`, `AerospikeFrequencyCapper` (deferred), `InProcessFrequencyCapper` (Phase 7, write-behind to Redis) |
| `Scorer` | Startup (config) | `FeatureWeightedScorer`, `MLScorer`, `CascadeScorer`, `ABTestScorer` |
| `BudgetPacer` | Startup (config) | `LocalBudgetPacer`, `DistributedBudgetPacer`, `HourlyPacedBudgetPacer`, `QualityThrottledBudgetPacer` |
| `TargetingEngine` | Startup (config) | `SegmentTargetingEngine`, `EmbeddingTargetingEngine`, `HybridTargetingEngine` |
| `ExchangeAdapter` | Per-route (axum routing per `/rtb/{exchange}/bid`) | `OpenRtbGeneric`, `GoogleAdx`, `Magnite`, `IndexExchange` |
| `EventPublisher` | Startup (config) | `KafkaEventPublisher`, `NoOpEventPublisher`, `KinesisEventPublisher` (future) |
| `UserSegmentRepository` | Startup (config) | `RedisUserSegmentRepository`, `AerospikeUserSegmentRepository` (future) |
| `CampaignRepository` | Startup (config) | `PostgresCampaignRepository`, `JsonFileCampaignRepository` (test) |

Trait dispatch is `Arc<dyn Trait + Send + Sync>` — runtime cost is one virtual call per invocation, negligible vs the work each does. Hot-path traits (Scorer, FrequencyCapper) can be promoted to generic monomorphization (`impl<F: FrequencyCapper>`) if profiling shows the dyn dispatch is measurable; default to dyn.

### Production-realistic concurrency model

**Phases 1-6 development default:** Tokio multi-threaded scheduler, work-stealing across N workers (N = `num_cpus::get()`). Single-process. Easy local dev, easy debugging. This is what we run on macOS.

**Phase 7 production default (Tokio path):** **Multi-process Tokio + `SO_REUSEPORT`** — N single-threaded Tokio runtimes, one process per CPU core, kernel routes `accept(2)` round-robin across processes. Eliminates work-stealing cross-thread synchronization for ~90% of the thread-per-core perf benefit with none of the ecosystem pain. This is the production deployment topology for the Tokio path. Same `bidder-server` binary; a process supervisor (systemd, K8s with HPA on pods, or a thin parent in `main.rs`) launches N copies bound to the same port.

**Phase 7 experimental binary (monoio path):** Completely separate `bidder-server-monoio` workspace member with its own `main.rs`. Does **not** share `Send + Sync`-bound traits with `bidder-core` — instead, monoio gets its own slim adapters for the parts of `bidder-core` that need `!Send` semantics (mostly the I/O layer; pure compute is portable). Treating monoio as a runtime toggle is a known trap: `axum`/`tower`/Tokio assume `Send + Sync` because tasks can be stolen; monoio is strictly thread-local `!Send`. Trying to compile `bidder-core` clean against both forces brutal `#[cfg]` walls and compromises both paths. We don't do that.

**Decision rule:** if Phase 7 multi-process Tokio + SO_REUSEPORT hits the throughput target, monoio is unnecessary and stays as a learning experiment. If profiling shows the io_uring/syscall path is still the limiter at that point, the monoio binary becomes a real shipping option — but as a separate binary, not a magic flag.

#### Process-per-core vs thread-per-core — explicit tradeoff

These are two distinct ways to scale to all cores on a node. They're not the same thing.

| Approach | How | Pros | Cons | When to choose |
|---|---|---|---|---|
| **Multi-process Tokio + SO_REUSEPORT** (one single-threaded Tokio runtime per process, one process per core) | N processes bind same port; kernel routes accept(2) round-robin; no work-stealing cross-thread sync | Strong isolation (one crash = one process); simpler debugging; works without io_uring; allocator state per-process so no cross-core contention; **gets ~90% of thread-per-core benefit without ecosystem pain** | Higher memory overhead — campaign catalog (~50-150 MB), ONNX model (~10-50 MB), and allocator state are duplicated per process. On a 16-core node that's ~1-3 GB total just for the catalog/model footprint, before request memory. Must be sized into K8s pod memory requests/limits explicitly. Cross-process state requires Redis. | **Production default for Tokio path.** What we ship. |
| **Thread-per-core runtime** (separate `bidder-server-monoio` binary, N pinned monoio runtimes, `!Send` thread-local) | One process, N runtimes, each owns one CPU + one io_uring | Lower memory (single shared cache); io_uring eliminates per-syscall overhead; better tail latency at very high RPS | Whole separate binary (not a flag — `Send + Sync` vs `!Send` makes shared `bidder-core` traits across both impossible without `#[cfg]` walls); harder to debug; Linux 5.10+ only; smaller ecosystem | Phase 7 experiment. Considered for production only if profiling on multi-process Tokio shows io_uring/syscall path is the limiter. |

Both are supported. The Tokio path is the default; monoio is a separate binary. The choice is a deployment decision (which binary you launch + how you supervise it), not a runtime config flag — `bidder-server` and `bidder-server-monoio` are different entry points sharing only the portable parts of `bidder-core`.

## Phase 0 — Lock-in deliverables (before Phase 1 code)

A short pre-implementation phase. These five artifacts shape every downstream decision; getting them wrong means rework in Phase 3-4. Do not skip.

**0.1 — Redis key schema (`docs/REDIS-KEYS.md`):**
- Exact key shape for every Redis-backed concept: user segments, frequency counters, distributed budget pacing, hot-user warm-set.
- Hash-tag strategy for cluster-slot affinity — e.g., `freqcap:{user:42}:c123` so all of user 42's freq counters land on the same slot, enabling MGET in one round-trip.
- Encoding choice per value (raw int, packed binary via `rkyv`, or string).
- TTL per key family.
- Decision committed to the doc; changing it later forces data migration.

**0.2 — Postgres campaign schema (`docs/POSTGRES-SCHEMA.md` + a migration file):**
- Tables: `campaign`, `campaign_targeting_segment`, `campaign_targeting_geo`, `campaign_targeting_device`, `campaign_targeting_format`, `campaign_daypart`.
- Each targeting table is a join table with a foreign key to `campaign(id)` and the targeting value — drives the inverted-index build query directly (`SELECT segment_id, array_agg(campaign_id) FROM campaign_targeting_segment GROUP BY segment_id`).
- Indices on the targeting columns so the load query stays under 5s for 100K campaigns.
- Schema version tracked; future schema changes go through a migration tool (`sqlx migrate` or `refinery`).

**0.3 — Segment ID mapping strategy (`docs/SEGMENT-IDS.md`):**
- **Decision: globally-assigned 32-bit segment IDs** maintained in a `segment` table in Postgres (`id SERIAL`, `name TEXT UNIQUE`, `category TEXT`).
- Bidder loads `HashMap<String, SegmentId>` at startup alongside the campaign catalog.
- Per-tenant scoping is **out of scope for v1** — this is a single-tenant DSP. If multi-tenant is needed later, IDs become `(tenant_id, segment_id)` tuples and bitmaps shard per tenant. Documented as a deferred decision so the v1 schema doesn't accidentally lock it out.
- New segments seen on the wire (in user data from a DMP) auto-register on first sight via UPSERT; bidder learns about them at the next 60s catalog refresh. Until then the segment is treated as no-match.

**0.4 — The golden bid request (`tests/fixtures/golden-bid-request.json`):**
- One canonical, realistic OpenRTB 2.6 bid request. Multi-impression (3 imps), populated user segments (10), geo, device, all required fields per the spec.
- Used **everywhere**: every unit test, every integration test, every criterion bench, every k6 fixture (k6 generates variations from this seed).
- A second variant `golden-bid-request-no-user.json` for the "no user data, segment-only matching" path.
- These two files are the single source of truth for "what does a bid request look like." If a test or bench needs a different shape, it derives from these — never reinvents.

**0.5 — The golden load test (`k6/golden.js`):**
- One canonical k6 script. Fixed RPS profile: 60s ramp 0→target, 300s hold, 60s ramp down.
- Pulls request bodies from a generated corpus seeded from the golden bid request (1000 variations covering segment combinations, multi-impression, geo skew per the Zipfian seed strategy).
- **This script never changes** without explicit "we are recalibrating the baseline" decision. Every load-test result in the project — phase comparisons, regression hunts, monoio vs Tokio benchmarks — uses this script. Otherwise we're comparing apples to different apples.
- Run targets: 5K, 10K, 15K, 20K, 25K, 30K, 40K, 50K. Same k6 script, parameterized only by target RPS.

These five artifacts are committed to the repo before Phase 1 implementation begins. Phase 1 references them as established contracts.

---

## Phases

Seven phases (after Phase 0). Each delivers a runnable bidder, an architectural layer, and a written learnings doc.

---

### Phase 1 — Foundation

Cargo workspace skeleton; HTTP server; observability (tracing → Tempo, metrics → Mimir, logs → Loki); config; graceful shutdown; production Dockerfile.

**Deliverable:** `cargo run --release` starts a bidder on `:8080` returning hardcoded responses. The k6 baseline test passes against it. Distributed traces appear in Tempo. Metrics scrape works.

**Components:**
- Cargo workspace: `bidder-server`, `bidder-core`, `bidder-bench`, `bidder-protos`
- jemalloc (or mimalloc) wired in as `#[global_allocator]` from day one; arena left out
- `axum` HTTP server on tokio multi-threaded runtime
- `tower` layer stack: ConcurrencyLimit (load shed), Timeout (50 ms), TracingLayer (OTel), MetricsLayer
- `figment` config (TOML + env-var overlay), CLI args for overrides
- `tracing` structured logs (JSON output for Loki)
- `tracing-opentelemetry` exporting to Tempo via OTLP/HTTP, with **head-based sampling configured from day one**. Sampler keeps: 100% of error-tagged spans, 100% of SLA-violation spans (>40 ms), and 1% of remaining spans (configurable via `[telemetry] success_sample_rate`). Metrics (`metrics-exporter-prometheus`) stay at 100% sampling — they're cheap aggregations. Tracing is for anomaly debugging, not aggregate visibility.
- `metrics` + `metrics-exporter-prometheus` on `/metrics`
- Graceful SIGTERM shutdown (in-flight requests finish; new requests get 503 during drain)
- Multi-stage Dockerfile: `rust:1-bookworm` builder → `gcr.io/distroless/cc` runtime, ~15 MB final
- `SO_REUSEPORT` listener configuration ready for production (multi-process per node)

**Production-deployment-ready from day 1:**
- Liveness vs readiness probe split. Liveness = process alive; readiness = warmup complete (see below).
- Resource requests/limits-aware (no unbounded memory growth)
- Outputs structured logs that Loki ingests
- Listens on configurable port (production: behind L7 LB or Envoy)

**Warmup phase (separates "process started" from "ready for production traffic"):**
At 30K+ RPS, a cold pod blowing the SLA for 30 seconds = ~900K degraded requests = real revenue loss + alert noise. The pod stays out of the LB rotation until warmup completes. Warmup is a Phase 1 concern even though Phase 1 has no hot path yet — the *contract* and *control flow* are wired in early so later phases populate it.

Warmup steps (ordered, all must complete before readiness probe returns 200):
1. **Catalog load** — load Postgres campaigns + build inverted indices (Phase 3+). Until then, no candidates can be returned.
2. **Connection priming** — open and authenticate every Redis pool connection, run a `PING` round-trip on each. Open the Postgres pool. Open the Kafka producer connection (handshake + metadata fetch). Eliminates first-request TLS/auth latency.
3. **Hot-cache pre-population** — pre-load the top-K most-active user IDs (from a tracked "warm set" written to Redis by a previous instance, or computed as the most-frequently-targeted user segments cross-referenced against a recent activity sample) into `moka`. Configurable size, default 50K hot users. Eliminates the 30-second cache-miss-storm after a restart.
4. **Memory pre-touch** — for the campaign catalog and inverted indices, walk the structures once to fault in pages and warm the allocator's thread-local caches.
5. **Self-test** — run 100 synthetic bid requests through the pipeline against the warmed-up state; assert all complete under SLA. If any fail, abort startup with clear logs.

Warmup target: < 30s on a fresh pod with a populated Redis. SIGTERM during warmup is graceful (no LB traffic to drain). Metric `bidder.warmup.duration_seconds` tracked per restart.

**Platform support from day 1:**
- `cargo build --release` succeeds on macOS Apple Silicon (dev), Linux x86_64 (production), Linux aarch64 (production). No effort spent on other targets.
- `Dockerfile` (production, distroless) and `Dockerfile.dev` (Debian-based, for running the Linux build locally on a Mac) both build from the same workspace.
- `docker compose up bidder` brings up the bidder + Redis + Postgres + Kafka + Grafana + Tempo. Same compose file the Java repo uses.
- CI matrix from Phase 1: `{macos-latest (Apple Silicon), ubuntu-latest (x86_64), ubuntu-latest (aarch64)} × {default features, --all-features}`. Linux-only features (`linux-tuning`, `monoio-runtime`) gated behind `cfg(target_os = "linux")` so macOS CI never tries to build them.

**Profiling baseline:** Run k6 baseline on macOS host (and inside Linux Docker container for parity check). Capture samply flame graph on both. Capture tokio-console snapshot. Note RSS, startup time, idle CPU. Document any macOS-vs-Linux delta — production numbers are Linux numbers.

---

### Phase 2 — Pipeline architecture, OpenRTB 2.6 models, per-stage latency budgeting

Full type system for OpenRTB 2.6 (multi-impression from day 1); pipeline trait + orchestrator; per-stage budget enforcement.

**Deliverable:** Bidder parses real OpenRTB requests (multi-impression supported), runs through a 2-stage skeleton (validation + response build), returns valid OpenRTB responses. Per-stage timing visible in Tempo + Prometheus.

**Components:**
- `bidder-protos`: full OpenRTB 2.6 model — `BidRequest`, `BidResponse`, `Imp`, `Banner`, `Video`, `Native`, `User`, `Device`, `App`, `Site`, `Source`, `Regs`, `Geo`, etc.
- `bidder-core::model::BidContext` — per-request mutable state, owned, no pooling; standard heap allocation under jemalloc/mimalloc (arena deferred to Phase 7 experiment)
- `bidder-core::pipeline::Stage` trait + `Pipeline` orchestrator
- Per-stage budget enforcement: `tower::Layer` wraps each stage, records duration, alerts via metrics if p99 exceeds declared budget
- `simd-json` for incoming parse (default); fallback to `serde_json` for environments without SIMD. **Buffer model:** the axum handler reads the request body into a freshly-allocated owned `Vec<u8>` (never `Bytes` — `simd-json` requires `&mut [u8]`). The Vec is dropped when the handler returns; per-request lifetime is enforced by ownership, no pool needed. `bytes::Bytes` is reserved for downstream zero-copy *reference* passing (e.g. cached Redis key buffers), never for the parse buffer.
- `serde_json` for response write (hot path is parse, not serialize, so SIMD is less impactful here; revisit in Phase 6)
- Sum-typed `enum NoBidReason` exhaustively matched at boundary
- First stage: `RequestValidationStage`; last stage: `ResponseBuildStage` (hardcoded campaign output)
- `tracing::instrument` on every stage with structured fields (campaign_count, outcome, etc.)

**Production patterns:**
- Multi-impression: `Vec<Imp>` from day one, per-imp candidate selection, response `seatbid.bid[]` matched 1:1
- `enum AdEvent` sum type defined now even though only Phase 5 publishes events — locks in the schema
- Latency budget table (above) committed to `config.toml`; overrides via env var

---

### Phase 3 — Data layer + campaign indexing

Postgres campaign loading at production scale (50K-100K), Redis user segments, in-process caching, **inverted-index candidate retrieval** (the single largest architectural improvement over the Java baseline).

**Deliverable:** Bidder loads 50K-100K campaigns from Postgres at startup, builds inverted indices, fetches user segments from Redis on the hot path. Candidate retrieval stays sub-millisecond at production catalog size.

**Components:**
- `sqlx` Postgres → `Vec<Campaign>` (50K-100K) → `Arc<ArcSwap<CampaignCatalog>>` for atomic refresh (background job every 60s)
- **Catalog rebuild contract (explicit, non-blocking):**
  1. A dedicated `tokio::spawn`'d background task wakes every 60s.
  2. It loads the new catalog from Postgres into a **freshly-allocated** `CampaignCatalog` (new Vec, new HashMaps, new RoaringBitmaps) — no mutation of any data the hot path can see.
  3. Building the inverted indices over 100K campaigns takes ~200-500ms; this happens entirely on the background task, hot path is unaffected.
  4. When the build completes, a single `ArcSwap::store(Arc::new(new_catalog))` atomically swaps the pointer. In-flight requests holding the old `Arc` finish reading from it and drop it; new requests see the new catalog. Zero coordination needed.
  5. If the rebuild fails (Postgres timeout, schema mismatch), the old catalog stays live and an alert fires — never partial visibility. Failure of N consecutive rebuilds opens the catalog-refresh circuit breaker (signal that the system is running on an old-but-valid catalog; alert SRE).
  6. Incremental rebuild is **out of scope** for v1 — full rebuild every 60s is fine at 100K campaigns. Revisit only if rebuild crosses 5s.
- `CampaignCatalog` is **not** just a Vec. It's the campaign list + a set of inverted indices built at load time:
  - `segment_to_campaigns: HashMap<SegmentId, RoaringBitmap>` (campaign IDs as bits)
  - `geo_to_campaigns: HashMap<GeoCode, RoaringBitmap>`
  - `device_to_campaigns: HashMap<DeviceType, RoaringBitmap>`
  - `format_to_campaigns: HashMap<AdFormat, RoaringBitmap>`
  - `daypart_active_now: RoaringBitmap` (recomputed per minute by background task)
- Per-request candidate retrieval is roaring-bitmap **intersection** of the relevant indices, not Vec scan. With 100K campaigns × ~10 segments per request, this is O(segments × avg_list_len) — typically <1ms.
- **Bitmap-mutation safety contract (critical):** the inverted indices live inside the shared `Arc<ArcSwap<CampaignCatalog>>` and are **read-only** for the lifetime of a snapshot. Per-request union/intersection code MUST clone the first bitmap into a working set (`let mut working = catalog.segment_to_campaigns[s0].clone()`) before calling any in-place op (`or_inplace`, `and_inplace`, `sub_inplace`). Mutating an indexed bitmap by reference corrupts the catalog for every subsequent request — silent, hard-to-debug data poisoning. Enforced by API design: the `CampaignCatalog::candidates_for(...)` method returns a *new* `RoaringBitmap`; raw access to the indices is `pub(crate)` only and goes through a helper that always clones first.
- `fred` Redis as a **round-robin pool from day one** — `Vec<fred::Client>` of size N (start with `num_cpus` or `num_cpus / 2`), per-request modulo selection. Same client-side decode-parallelism reason the Java repo retrofitted Lettuce; we apply it upfront here.
- Benchmark calibration during Phase 3: sweep N ∈ {1, 2, 4, 8, num_cpus} at 5K and 10K RPS. Pick the lowest N that doesn't show decode-thread saturation in `samply` flame graphs. Document the chosen N and the saturation evidence.
- `moka` async cache for user segments (500K capacity, 60s TTL)
- `repository::CampaignRepository` and `repository::UserSegmentRepository` traits
- `targeting::SegmentRegistry`: `HashMap<String, SegmentId>` for name → integer ID. Roaring bitmaps store `SegmentId` (u32), not strings.
- Seeding: extend `docker/seed-redis.py` and add `docker/seed-postgres.py` to populate 50K-100K campaigns with **realistic distribution — explicitly Zipfian, not uniform random**:
  - **Segment popularity follows Zipf with α ≈ 1.0-1.2** (a handful of "auto-intender", "in-market: travel", "millennial" type segments cover ~80% of campaigns; long tail of niche segments covers the rest). Real ad-tech segment data is heavily clustered.
  - **Geo distribution skewed** to top-10 metro areas (~60% of campaigns), long tail of the rest.
  - **Daypart variation** — most campaigns active during business hours / prime time, fewer overnight.
  - **Frequency cap counters per user** also Zipfian (active users have many cap counters; the long tail of users have few).
  - **Why this matters for benches, not just realism:** `RoaringBitmap` perf characteristics depend heavily on data sparsity and run-length-encoding opportunities. Uniform random data produces uniformly-medium-density bitmaps that compress poorly and intersect at one speed; real Zipfian data produces a mix of dense (popular segments) and very sparse (niche segments) bitmaps that exercise Roaring's actual hot paths. Benchmarks against uniform data give misleading flame graphs. Seed script outputs both uniform and Zipfian fixtures so we can A/B the hot path under both distributions and confirm we're not optimizing for the wrong shape.

**Production patterns:**
- `arc-swap` for hot-read shared state without RwLock contention
- `bytes::Bytes` for cheap-clone reference-counted Redis key buffers (avoid String concatenation per request)
- Connection health check + auto-reconnect (fred handles this; verify config)

**Redis throughput discipline (key for 30K+ RPS):**
- Maximize commands per round-trip: pipeline / `MGET` / `EVAL` rather than serial single-key calls. Per-request, batch all freq-cap reads into one MGET; batch all segment reads where the user has them across multiple keys.
- Tune `fred` pipeline depth: explicit `max_command_attempts`, pipeline buffer size, and submit cadence. Document the chosen values and the profiling that justified them.
- Local-cache-first reads via `moka` for user segments — Redis is hit only on cache miss. Cache-hit ratio is a tracked metric (`bidder.cache.user_segments.hit_ratio`).
- At 30K+ RPS, Redis network + decode is the system limiter, not Rust compute. The mitigations above plus Phase 7's `InProcessFrequencyCapper` (write-behind) are the levers — not micro-optimizing the Rust side.
- **Key design is a first-class performance lever.** Avoid high-cardinality scatter (per-key round trips) and prefer batchable layouts (MGET/EVAL-friendly schemas). Poor key shape will dominate latency before any client-side optimization matters. Schema decisions (key prefix, hash-tag for cluster slot affinity, value packing) get reviewed alongside code.

**Comparison checkpoint:** k6 stress at 5K and 10K. Profile with samply + tokio-console. Should be at or above Java numbers.

---

### Phase 4 — Hot path: targeting, scoring, frequency capping

The full bid pipeline. Bitmap segment match, top-K candidate selection, batched scoring, paged MGET freq cap, bounded write queue, atomic budget pacing.

**Deliverable:** End-to-end bid pipeline producing real bid responses. Bid quality matches Java v5.2 on identical k6 traffic. Per-stage budget enforcement live; alerts fire on budget overruns.

**Components:**
- `targeting::SegmentTargetingEngine` using inverted-index intersection (built in Phase 3): for each user segment, look up the `RoaringBitmap` of matching campaigns; `RoaringBitmap::or_inplace` to union, then intersect with geo/device/format/daypart bitmaps. Output is a `RoaringBitmap` of candidate campaign IDs — typically a few hundred to a few thousand from a 100K catalog.
- `pipeline::stages::CandidateLimitStage` with `BinaryHeap<Reverse<AdCandidate>>` for top-K (`Reverse` flips max-heap to min-heap behavior — Rust idiom)
- `scoring::FeatureWeightedScorer` with `score_all` batched API (encode user bitmap once, reuse across candidates)
- `frequency::RedisFrequencyCapper` with paged MGET via `fred`'s pipeline API
- `frequency::ImpressionRecorder`: bounded `tokio::sync::mpsc::channel(65536)` + N spawned worker tasks consuming and writing freq counters via Redis EVAL (atomic INCR + EXPIRE Lua script)
- `pacing::LocalBudgetPacer` with `AtomicI64::fetch_sub` per campaign; `pacing::DistributedBudgetPacer` with Redis DECRBY for multi-instance
- `pipeline::stages::RankingStage`: sort survivors by score, pick top-1 per impression

**Tail-latency-aware additions (NEW vs Java):**
- **Hedged Redis MGET** — guarded, not unconditional. Hedging without guardrails amplifies load during the exact failure modes you want to survive. All four conditions must hold to fire a hedge:
  1. **Idempotent reads only.** MGET / SMEMBERS / GET — yes. Any state-changing command (INCR, EVAL, DECRBY) — never. Enforced by trait split: `IdempotentRedisRead` is a separate method from `RedisWrite`.
  2. **Latency trigger.** First request hasn't returned by **max(p95 latency, 8 ms floor)** (adaptive but never too aggressive — a healthy Redis with rising p95 doesn't get hammered, and a degraded Redis with falling p95 doesn't get hedge-amplified).
  3. **Health gate.** Circuit breaker for Redis is closed (system isn't already degrading). Disable hedging entirely while breaker is open or half-open.
  4. **Sample cap, adaptively coupled to system health.** Token bucket grants ≤10% of requests hedge eligibility per second under normal load. **The bucket size dynamically contracts when the HTTP layer is shedding load.** Specifically: if `bidder.load_shed_rate > 0.01` (>1% of requests rejected at HTTP), hedge budget drops to 2%. If `> 0.05`, hedge budget drops to 0% (hedging fully disabled). Reasoning: hedging during a traffic spike injects *more* load into a degrading system — the exact opposite of what you want. The hedge budget is a feedback-controlled lever, not a static knob.
- **Partial-result fallback:** if freq-cap MGET exceeds 10 ms (just under its 5 ms p99 budget + slack), skip freq cap entirely with a metric-tagged warning. Bid quality drops marginally; SLA is preserved.

**Allocation discipline:**
- jemalloc/mimalloc as global allocator (chosen at Phase 1)
- `SmallVec` for inline storage of small candidate lists (bypass heap for typical sizes)
- `bytes::Bytes` for shared zero-copy buffers
- `bumpalo` arena considered only if Phase 4 profiling shows allocator contention dominating; off by default (see Phase 7)

---

### Phase 5 — Resilience + events + full observability

Circuit breakers, hedged calls, partial-fallback already exist from Phase 4 — formalize them. Kafka event publishing with protobuf. Distributed tracing across the entire request including Redis/Postgres calls.

**Deliverable:** Bidder publishes events to Kafka in protobuf format. Circuit breakers protect every external call. Tempo shows full request span tree including Redis/Postgres operations.

**Components:**
- `rdkafka::FutureProducer` with bounded internal queue, `prost`-generated protobuf event types
- `bidder-protos::events`: `BidEvent`, `WinEvent`, `ImpressionEvent`, `ClickEvent`, `VideoQuartileEvent`, `ConversionEvent`, `ViewabilityEvent` — all in a `oneof AdEventBody { ... }`
- `EventPublisher` trait with `KafkaEventPublisher` and `NoOpEventPublisher` impls
- Circuit breaker: hand-rolled per-dependency state machine (Closed → Open → Half-Open). **Latency-aware open conditions**, not just error-aware: breaker opens on either (a) error rate > threshold, or (b) **slow-call ratio** > threshold (slow = call duration above the dependency's p99 budget × 2, sliding window). Slow-call ratio is the critical addition — without it, a Redis instance that's slow but not erroring would let hedging keep firing despite obvious degradation. With it, breaker opens on slowness alone, which automatically disables hedging via guardrail #3 in Phase 4. State transitions tagged in tracing for debugging.
- Dependency-specific timeouts: Redis 8 ms, Postgres N/A (startup only), Kafka producer 50 ms (background, doesn't block bid path)
- Resilience patterns formalized via `tower` layers reusable by future deps
- Tempo span hierarchy: `bid_request` → `pipeline.execute` → per-stage spans → per-Redis-call spans → per-Kafka-publish spans

**Kafka drop policy (explicit, adaptive):**
- Bounded internal queue: 100,000 events.
- **Adaptive drop strategy via config flag `[kafka] drop_policy = "newest" | "oldest" | "random_sample" | "incident_mode"`** — default `"newest"`, overridable at runtime via SIGHUP / config reload without restart.
  - `"newest"` (default, normal ops): preserves historical stream continuity for billing/analytics. Drops freshest telemetry under overload — acceptable in steady-state.
  - `"oldest"` / `"random_sample"` (incident ops): preserves the most recent N events so on-call has fresh visibility during a live incident. Activated by SRE during outages so we don't go blind right when fresh telemetry matters most.
  - `"incident_mode"` auto-activates when `bidder.kafka.events_dropped` rate sustains >1% for >5 min — automatic flip to `random_sample` so you don't need a human in the loop to preserve incident visibility. Config-overridable.
- Bid path **never** awaits the Kafka send. `try_send` only; failure increments `bidder.kafka.events_dropped` counter and logs at warn level (sampled).
- Kafka slowness must never propagate to bid latency. If broker is unreachable for >30s, circuit breaker opens; events drop at the publisher layer with metrics. Bidding continues unaffected.
- Alert: `rate(bidder.kafka.events_dropped[5m]) > 0` is page-worthy in production. Alert payload includes current `drop_policy` so on-call knows what telemetry shape they're getting.

**Production observability:**
- OTel semantic conventions: `http.method`, `http.status_code`, `messaging.system="kafka"`, custom `bidder.*` namespace
- Prometheus dashboard panels for: per-stage p50/p99/p999, dependency error rates, circuit breaker states, queue depths, JVM-equivalent (allocator) pressure indicators
- Structured logs include `trace_id` so Loki + Tempo cross-link

---

### Phase 6 — ML scoring + cascade

ONNX inference via `ort`, cascade scorer with proper threshold tuning, feature extraction performance work.

**Deliverable:** Bidder runs `cascade` scoring (FeatureWeighted pre-filter → ML on top-K survivors) by config flag. ML inference does not blow the SLA at sustained 10K-15K RPS.

**Components:**
- `scoring::MLScorer` using `ort` with the existing `ml/pctr_model.onnx`
- `scoring::CascadeScorer`: stage1 = FeatureWeighted, stage2 = ML, threshold from config
- Feature extraction: pre-compute static campaign features at load time (avoid recomputing per request); per-request features extracted into a `Vec<f32>` buffer reused across the call (arena-allocated)
- ONNX session pool: one session per worker thread (sessions are not Send by default in ort — verify and document)
- `MLScorer::score_all` batched API — pass all candidates' features as a single `[N, F]` tensor, run inference once, get back `[N]` scores. Avoids per-candidate session overhead.
- `simd-json` adoption for response write path if profiling shows it's hot
- Cascade threshold sweep doc: how to find the threshold that keeps ML cost in budget while preserving bid quality (lessons from Java cascade testing)

**Production patterns:**
- A/B test support via `ABTestScorer` decorator: hash user ID, route % to ML, % to FeatureWeighted, emit metric tagged with the variant
- ML pCTR scores tagged on bid responses for analytics correlation
- ONNX model hot-reload via watching the model file (no bidder restart needed for model updates)

---

### Phase 7 — Production hardening + multi-exchange + thread-per-core experiment

Kernel tuning, multi-exchange adapters, in-process freq counters with write-behind, thread-per-core monoio experiment, full benchmark write-up.

**Deliverable:** Production deployment guide for Linux servers. Multi-exchange adapter trait with at least one alternate format (Google AdX). Optional thread-per-core build profile. Full per-RPS comparison across all configurations.

**Components:**
- `ExchangeAdapter` trait + impls: `OpenRtbGeneric` (default), `GoogleAdx` (binary protobuf endpoint, different request shape), `Magnite`, `IndexExchange`
- Multi-exchange routing: axum routes `/rtb/openrtb/bid`, `/rtb/adx/bid`, `/rtb/magnite/bid` → different parsers, common pipeline
- Load shedding moved to phase-1's tower layer (already there) — verify the threshold is tuned for production load, not laptop load
- `frequency::InProcessFrequencyCapper` (optional, behind config flag): hot freq counters in `Arc<DashMap<UserId, Vec<u32>>>` with periodic write-behind to Redis. Trades brief inconsistency for sub-microsecond reads. Single-instance only; document the constraint clearly.
- `monoio` thread-per-core experiment: separate `bin/bidder-monoio` entry point sharing `bidder-core`. Same pipeline, different runtime. Compare throughput on Linux server.
- ~~`bumpalo` arena experiment~~ — retired in Phase 8. No flame graph through Phase 7 surfaced allocator contention as a top hot spot, so the arena's complexity (especially around `.await` boundaries) wasn't earning its keep. Re-add only if a future profile changes the picture.

**Kernel + OS production tuning** (documented as deployment checklist):
- `SO_REUSEPORT` for multi-process bidding on one node
- `TCP_NODELAY` (default in axum, verify)
- `SO_BUSY_POLL` for low-latency reads (Linux only, root or `CAP_NET_ADMIN`)
- IRQ affinity: pin NIC interrupts to specific cores
- CPU isolation: `cpuset` cgroups or `isolcpus=` boot param
- NUMA-aware deployment: `numactl --cpunodebind=0 --membind=0` if dual-socket
- Tcp tunings: `net.core.somaxconn`, `net.ipv4.tcp_tw_reuse`, `net.ipv4.tcp_fin_timeout`
- Container limits: huge pages for the heap, `ulimit -n` for fd budget

**Comparison docs:**
- `LOAD-TEST-RESULTS-rust-v1.md` mirroring the Java repo's results docs structure
- Per-RPS table: 5K, 10K, 15K, 20K, 25K, 30K, 40K, 50K
- Per-config: tokio default, tokio + simd-json, tokio + hedged Redis, tokio tuned, monoio thread-per-core
- Honest negative results: what we tried that didn't help

**Multi-instance production deployment:**
- Helm chart for K8s
- HPA on `bidder.bid.duration_seconds.p99` and `bidder.qps`
- PodDisruptionBudget for graceful drain
- Topology spread constraints for AZ resilience
- Multi-process per pod via SO_REUSEPORT (vs multi-pod) trade-off documented

**Phase 7 deferred items — all closed in Phase 8:**

- ~~OTel sampling reality (head-based limitations)~~ — Phase 8 added `tail_sampling_via_collector` config + reference collector config at `docker/otel-collector/config.yaml`.
- ~~Kafka publish path is `tokio::spawn(publish().await)`~~ — Phase 8 swapped to `BaseProducer` + dedicated poll thread; trait is now synchronous.
- ~~`InProcessFrequencyCapper` cap is hard-ceiling, not LRU~~ — Phase 8 swapped to `moka::sync::Cache` with TinyLFU eviction + eviction-listener that flushes counters to Redis on evict.

See `docs/notes/phase-8-architectural-followups.md`.

---

### Phase 8 — Architectural follow-throughs (shipped)

Phase 8 closed every architectural compromise Phase 7 documented as deferred. No new features. After Phase 8 the project is feature-complete; remaining work is profiling and stress-test, not architecture.

**What shipped:**

1. **`InProcessFrequencyCapper` → `moka::sync::Cache`** with TinyLFU eviction + `time_to_live = 1h`. Eviction listener flushes counters to Redis through the existing write-behind channel so no in-process increment is silently lost. Hard-cap branch and `capacity_rejected_total` counter retired.

2. **Kafka publisher → `BaseProducer` + dedicated poll thread.** `EventPublisher::publish` is now synchronous. Bid-path call sites in `handlers.rs` no longer wrap publish in `tokio::spawn`. Backpressure surfaces through rdkafka's bounded queue with `queue.full.behavior=error`, exactly as the architecture diagram has always claimed. Net win: zero tokio task allocations on the event path (was ~100K/s at 50K RPS × 2 winners).

3. **OTel tail sampling — opt-in via collector.** New `tail_sampling_via_collector` config field. When true, the bidder ships 100% of spans and the collector's `tail_sampling` processor (reference config at `docker/otel-collector/config.yaml`) keeps 100% of error spans + 100% of >50ms spans + 1% probabilistic on the rest. Default false (head sampling, same as Phase 7) so the baseline harness stays unchanged.

4. **Bytes → Vec<u8> zero-copy verified at handler entry.** `body.into::<Vec<u8>>()` is O(1) when the buffer is uniquely owned (always true at axum handler entry). Comment expanded to make the invariant explicit so future edits don't accidentally break it.

5. **bumpalo retired.** Was Phase 7 feature-gated aspiration; never had any code. Removed from the plan rather than leaving it as standing intent.

6. **Multi-instance freq-cap correctness — sticky routing is the answer.** Best-effort multi-instance breaks the cap contract by definition; consistent-hash on `user_id` at the L7 LB (Envoy `ring_hash`, NGINX `hash $user_id consistent`) keeps the in-process cache correct. The existing in_process startup warning is now the canonical statement of this constraint.

7. **Lazy `candidates_for` bitmap evaluation** — landed in the Phase 7 review-feedback commit, listed here for cross-reference.

Full write-up + before/after rationale: `docs/notes/phase-8-architectural-followups.md`.

---

## Workflow rules

- One phase = one PR. Each phase ends with a runnable bidder, a perf comparison vs the previous phase, and a learnings doc capturing what was tried and what didn't help.
- Profile before optimizing. Every "fix" is samply / tokio-console / criterion-validated. Negative results documented with the same rigor as positive ones. Same discipline as the Java repo.
- `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --check` are commit gates.
- Latency budget per stage is part of the contract, not a guideline. New stages declare their budget in config; budget overruns alert in production.
- Trait abstractions are committed before their implementations. Each pluggable concern has its trait reviewed for forward-compatibility before any concrete impl lands.
- Production deployment is the design target. Every architectural choice is reviewed against "would this work on a real Linux fleet under real traffic" — not "does this run on a Mac in dev mode."

## Platform strategy

**Where we focus: macOS Apple Silicon for dev, Linux (x86_64 + aarch64) for production. Everywhere else: should just work, but we don't actively test.**

Local development happens on macOS Apple Silicon. Production runs on Linux. CI validates these two paths. The architecture itself is portable Rust — no platform-specific code in the hot path, no platform-specific dependencies in the core stack. If someone clones this repo on Linux x86_64, Linux aarch64, Intel Mac, Windows, or anywhere else Rust runs, `cargo build` will most likely succeed and the bidder will run — we just don't make CI promises about it. The entire architecture (every trait, every stage, every model, every layer) is identical across all platforms.

**Worst-case porting cost** for someone on a non-supported OS: swap the global allocator (one line — jemalloc → mimalloc on Windows, for example), or skip the Linux-only `monoio-runtime` / `linux-tuning` features (already feature-gated, default off). Architecture, business logic, and traits never change.

### What's portable (everything in Phases 1-6)

All core dependencies — axum, tokio, hyper, fred, sqlx, rdkafka, ort, moka, simd-json, prost, rkyv, jemalloc/mimalloc, tracing, metrics, samply, criterion — work natively on macOS Apple Silicon and Linux. The Docker stack (Redis, Postgres, Kafka, Grafana, Tempo, Loki, Prometheus, ClickHouse) is reused from the Java repo's `docker-compose.yml` and runs identically on both.

simd-json autodetects the SIMD ISA at build time: NEON on Apple Silicon (dev) and aarch64 Linux, AVX2 on x86_64 Linux. ort uses CoreML on Apple Silicon for fast local inference and CPU on Linux production (CUDA optional, not in scope).

### What's Linux-only (Phase 7 production tuning, scoped behind feature flags)

| Feature | Why Linux-only | How handled |
|---|---|---|
| `monoio` thread-per-core | Requires `io_uring` (Linux 5.10+) | Separate workspace member `bidder-server-monoio` behind `--features monoio-runtime`. `bidder-server` (Tokio) is the default cross-platform entry point. |
| `SO_BUSY_POLL` busy polling | Linux-specific socket option | `cfg(target_os = "linux")` block; macOS path uses default poll behavior. No API difference at the application level. |
| `cpuset` / `isolcpus` / `taskset` CPU pinning | Linux scheduler primitives | Deployment-time concern, not code. macOS dev runs unpinned. |
| `numactl` NUMA binding | Linux-only | Deployment-time. Single-socket Macs don't need it. |
| IRQ affinity (`/proc/irq/*/smp_affinity`) | Linux sysfs | Deployment-time, Helm chart concern. |
| Sysctl tunings (`net.core.somaxconn`, etc.) | Linux sysctl | Deployment-time. |
| `SO_REUSEPORT` accept-load-balancing semantics | Linux kernel routes accept(2) round-robin; macOS is last-binder-wins on some versions | Code uses `SO_REUSEPORT` unconditionally (both OSes accept the option). On macOS we only run single-process locally, so the semantic difference doesn't matter. CI validates Linux multi-process behavior. |

### Cargo feature flags

```toml
[features]
default = []                  # cross-platform; macOS + Linux + Docker all build
linux-tuning = []             # SO_BUSY_POLL, sysctl applies, Linux-only socket opts
monoio-runtime = ["monoio"]   # Phase 7 thread-per-core experiment, Linux-only
# allocator-arena (bumpalo) was planned for Phase 7 but retired in Phase 8 —
# no flame graph through that point showed allocator contention worth the
# arena's `.await`-boundary constraints.
```

`cargo build` and `cargo test` with no flags must succeed on macOS Apple Silicon, Linux x86_64, and Linux aarch64. CI runs all three. Linux-specific features (`linux-tuning`, `monoio-runtime`) are validated only in Linux CI jobs.

### Docker as the universal fallback

A second Dockerfile (`Dockerfile.dev`) builds a development image based on `rust:1-bookworm` (Debian Linux). On macOS, you can run the bidder inside this container against the same docker-compose stack — useful for validating Linux-only behavior (monoio, cpuset, etc.) without leaving your Mac. `docker compose up bidder` brings up the bidder + full backing stack.

### Profiling parity

| Tool | macOS | Linux |
|---|---|---|
| `samply` (sampling profiler) | ✅ Apple Silicon native | ✅ |
| `tokio-console` | ✅ | ✅ |
| `cargo flamegraph` | ✅ (uses `dtrace` on macOS) | ✅ (uses `perf`) |
| `criterion` benchmarks | ✅ | ✅ |
| `perf` hardware counters | ❌ (use Instruments.app instead) | ✅ |
| eBPF-based observability | ❌ | ✅ |

Most profiling decisions can be made on either OS. Hardware-counter-level investigations (cache misses, branch prediction) on macOS use Instruments; on Linux use `perf stat`. Both produce comparable insights.

## Latency-budget enforcement in CI

A latency budget that's only checked in production is a budget that gets violated by PRs and discovered during a 3am page. CI catches budget regressions before merge.

**Per-PR (every push):**
- `cargo bench --bench pipeline` runs critical-path criterion benchmarks (segment match, scoring, bitmap intersection, JSON parse/serialize). Regression detection uses **rolling median over the last 20 main-branch runs + 2 standard deviations**, with a hard floor of 7%. Fixed-percentage thresholds (the naive 5%) generate noisy fails on shared CI runners due to CPU/cache variance; rolling median smooths the noise floor while still catching real drift. A regression must persist for 3 consecutive runs on main before the threshold tightens automatically. PR fails immediately on a single run that exceeds median + 2σ AND the absolute threshold floor.
- `cargo bench --bench redis_freqcap` runs against a Dockerized Redis with seeded data. Fails if p99 of the freq-cap MGET path exceeds its declared budget.
- `cargo build --release && cargo test --release` on macOS Apple Silicon, Linux x86_64, Linux aarch64 (matrix).
- `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --check` are commit gates.

**Per-PR (smoke load test):**
- Quick k6 run at 5K RPS for 60s against the PR build, full Docker stack. Pass criteria: p99 < 50 ms, error rate < 0.1%, no per-stage budget alerts fired. Smoke test runtime ~3 min — fast enough for every PR.

**Nightly (heavier):**
- Full k6 ramp 5K → 10K → 15K → 20K → 25K against the main-branch build. Captures p50/p99/p99.9 per stage, posts a delta vs prior night to a tracked dashboard (and a Slack/issue thread if any tier degraded).
- Chaos scenarios: Redis-add-latency, Kafka-pause, single-pod-kill. Validates the "no cascading failure" success criterion. Fails the nightly if any scenario takes the bidder over SLA.
- Memory leak check: 30-min steady-state at 10K RPS. RSS slope must be < 1 MB/min. Catches slow leaks (un-bounded queue growth, leaked `Arc` cycles, etc.) before they hit production.

**Budget regression dashboard:**
- Each PR writes its bench numbers to a tracked artifact. The `LOAD-TEST-RESULTS-rust-vN.md` file is regenerated and committed at phase boundaries, giving a per-phase historical record.

This is "latency budget as a contract enforced by tooling" — PRs that silently slow the hot path don't merge.

## Non-goals (early phases)

The following optimizations are explicitly deferred until profiling proves necessity. This list exists to prevent premature optimization and keep engineering effort aligned with actual bottlenecks.

- **No custom allocators** beyond jemalloc/mimalloc until allocator shows as a top hotspot in flame graphs.
- **No thread-per-core (monoio)** until ≥25K RPS is sustained on Tokio with profiling evidence that the I/O path is the limiter.
- **No arena allocation (`bumpalo`)** unless allocation pressure dominates flame graphs — and even then, only with `.await`-safe scoping.
- **No internal binary protocol on the external hot path.** OpenRTB stays JSON; Kafka/internal-service paths are protobuf.
- **No micro-optimizing compute** (scoring, bitmap intrinsics, branch hints) before Redis and network costs are flat.
- **No premature genericization.** Hot-path traits stay `Arc<dyn Trait>` until profiling shows dyn dispatch is measurable.

Each of these can be revisited and lifted with profiling evidence — they are deferrals, not bans.

## Out of scope

- FPGA acceleration. The architecture supports any Scorer impl, but writing one in HDL is not part of this project.
- Custom kernel modules / DPDK. Stays in user-space; kernel-bypass would require dedicated hardware.
- Bidder-side audience modeling pipelines. Audience segments come pre-computed from a DMP / data pipeline; bidder reads them. Same as Java repo.
- Multi-region active-active coordination. Single-region deployment with horizontal pod scaling. Multi-region is a deployment-topology decision, not a bidder code decision.

## Repo layout

```
rust-rtb-bidder/
├── Cargo.toml                       # workspace
├── Cargo.lock
├── Dockerfile                        # production: multi-stage, distroless runtime
├── Dockerfile.dev                    # dev: Debian-based, for Linux validation on macOS hosts
├── README.md                         # project intro
├── .env.dev                          # committed dev config (no secrets)
├── .env.example
├── config.toml                       # latency budgets, defaults
│
├── bidder-protos/                    # workspace member: protobuf-generated types
│   ├── proto/
│   │   ├── openrtb.proto             # if we ever bridge to a binary OpenRTB variant
│   │   └── events.proto              # AdEvent oneof for Kafka payloads
│   └── build.rs                      # prost codegen
│
├── bidder-core/                      # workspace member: pipeline + stages + repos
│   └── src/
│       ├── lib.rs
│       ├── pipeline/
│       ├── targeting/
│       ├── scoring/
│       ├── frequency/
│       ├── pacing/
│       ├── repository/
│       ├── exchange/                  # ExchangeAdapter trait + impls
│       ├── event/
│       ├── resilience/
│       ├── config/
│       └── model/                     # BidContext, AdEvent enum, OpenRTB models
│
├── bidder-server/                    # workspace member: axum entry + tower layers
│   └── src/
│       ├── main.rs                    # composition root, axum bind, tokio runtime
│       └── server/
│           ├── routes.rs
│           ├── handlers.rs
│           └── layers/                # tower::Layer impls (load_shed, timeout, tracing, metrics)
│
├── bidder-server-monoio/             # OPTIONAL workspace member: thread-per-core entry (Phase 7)
│   └── src/
│       └── main.rs
│
├── bidder-bench/                     # workspace member: criterion benchmarks
│   └── benches/
│       ├── segment_match.rs
│       ├── scoring.rs
│       └── pipeline.rs
│
├── tests/                            # integration tests (axum-test)
│
├── docs/
│   ├── PLAN.md                       # this file
│   ├── ARCHITECTURE.md               # written during/after Phase 4
│   ├── LATENCY-BUDGET.md             # per-stage budget table + enforcement
│   ├── DEPLOYMENT.md                 # K8s + Helm + kernel tuning checklist (Phase 7)
│   ├── LOAD-TEST-RESULTS-*.md        # per-phase comparisons
│   └── notes/                        # per-phase learning logs
│
└── ml/                               # symlink to ../java-rtb-bidder/ml/
```

Docker infrastructure reused from the Java repo's `../java-rtb-bidder/docker-compose.yml`. No duplication; the Rust bidder talks to the same containers.

## Changelog — scale-driven architecture changes vs the Java baseline

This project is **not** a Java port. It is a ground-up rebuild for real DSP production workload. The table below captures the concrete architectural changes we made because the workload assumptions changed (see "Workload assumptions" section at top), so future readers understand what's new and why.

| Area | Java repo (toy scale: 1K campaigns, 1M users, 50 segments) | Rust plan (production scale: 50K-100K campaigns, 100M+ users, 10K-100K segments) | Why it changed |
|---|---|---|---|
| Campaign catalog representation | `List<Campaign>` linear scan per request | `Arc<ArcSwap<CampaignCatalog>>` with **inverted indices**: `segment→RoaringBitmap`, `geo→RoaringBitmap`, `device→RoaringBitmap`, `format→RoaringBitmap`, `daypart_active_now` | Linear scan over 100K is O(N) per request; dies at 5K+ RPS. Inverted-index intersection is O(K × avg_list_len) and stays sub-millisecond. |
| Segment encoding | 64-bit `long` bitmap (because total segments = 50, fits in 64 bits) | `RoaringBitmap` mandatory (sparse sets of 10K-100K segment IDs) | `u64` is unusable above 64 distinct segments. Roaring is the industry standard (Druid, Elasticsearch, Pilosa). |
| User audience cache | `moka(500K, 60s TTL)` assumed near-100% hit ratio (1M users fit comfortably) | Same cache size, but cache hit ratio is now a **tracked SLO** because 500K hot fraction of 100M+ audience is realistic, not guaranteed | At 100M users, cache misses hit Redis directly — Redis throughput becomes a first-class concern, not an afterthought. |
| Redis key design | Implicit (single-node Redis, key shape rarely matters) | **First-class performance lever** in Phase 3: hash-tag-aware keys for cluster-slot affinity, batchable layouts (MGET/EVAL-friendly), avoid high-cardinality scatter | At 100M+ users on Redis Cluster, bad key shape kills latency before any client optimization helps. |
| Frequency cap MGET page size | Tuned for 3-5 counters per user | Tuned for **10-50** counters per user | Real DSPs track multi-dimensional caps (campaign × creative × device × daypart). Affects MGET page size and the 8ms hedge floor. |
| Targeting dimensions indexed | Segment-only | Segment, geo, device, format, daypart — all inverted-indexed independently | Pre-filtering on multiple dimensions before scoring is required at 100K-campaign scale; otherwise scoring runs on too many candidates. |
| Postgres seed | 1K campaigns via SQL fixture | New `docker/seed-postgres.py` to generate **50K-100K campaigns** with realistic distribution (long-tail segment popularity, geographic skew, daypart variation) | Toy fixtures don't exercise the inverted-index code paths; need realistic distribution to find perf issues. |
| Bottleneck assumption | Compute (Vert.x worker pool, decode) | **Redis** (network + decode) — Rust compute stays cheap *because* of inverted indices | Compute dominating at production scale would mean the indexing failed; flagged as a design bug in the bottleneck hierarchy. |
| Performance target framing | "X RPS at 1K campaigns" | "X RPS at 50K-100K campaigns" — apples-to-real-DSP comparison, not apples-to-toy | A 30K RPS claim at 1K campaigns is meaningless to anyone evaluating production readiness. |

**One-line summary:** *the Java repo proved we could write a fast bidder; this Rust project proves we can write a fast bidder at production catalog and audience scale, where the architectural levers are completely different.*

## Changelog — v2: scaling-Rust pitfall corrections

A second round of architectural review surfaced five Rust-async-at-scale pitfalls that the v1 plan would have hit at 50K+ RPS. All are now corrected; this section preserves the *why* so future readers don't reintroduce them.

| Pitfall | What v1 said | What v2 says (current) | Why |
|---|---|---|---|
| Tokio↔monoio runtime toggle | `[runtime] mode = "tokio" \| "monoio"` config flag, single `bidder-core` shared across both | Two separate binaries: `bidder-server` (Tokio, default production via multi-process + SO_REUSEPORT) and `bidder-server-monoio` (separate experimental binary). No runtime flag. | `axum`/`tower`/Tokio are `Send + Sync`; monoio is `!Send` thread-local. Sharing `bidder-core` across both forces `#[cfg]` walls and compromises both. Multi-process Tokio + SO_REUSEPORT gets ~90% of monoio's benefit without the pain. |
| simd-json + Bytes incompatibility | `simd-json` and `bytes::Bytes` both listed in stack as if interchangeable for the parse buffer | simd-json mandates per-request owned `Vec<u8>` (it mutates in place). `bytes::Bytes` is reserved for downstream zero-copy reference passing only, never the parse buffer. | `simd-json` requires `&mut [u8]`. `Bytes` is immutable; using it forces a copy and defeats the SIMD win. |
| Single multiplexed Redis connection by default | "One connection by default; pool if profiling shows decode hot" | **Round-robin pool of N connections from day one**; benchmark down to fewer only if pool overhead exceeds parallelism win. | At 30K+ RPS, single-connection RESP3 decode pegs one core at 100% and silently spikes p99 (mysterious internal bottleneck — Java repo had to retrofit this). Apply the lesson upfront. |
| Tracing without sampling | "OTel tracing on every request from Phase 1" | Head-based sampling from day one: 100% errors + 100% SLA violations + 1% successful (configurable). Metrics stay 100%. | 50K RPS × ~5 spans/req = 250K span exports/sec. Eats 20-30% CPU silently and applies backpressure. Tracing is for anomaly debugging; metrics for aggregate visibility. |
| Static Kafka drop policy + static hedge budget | Drop-newest always; 10% hedge budget always | Adaptive Kafka drop policy (`newest` / `oldest` / `random_sample` / `incident_mode`); hedge budget contracts when HTTP layer is shedding load (10% → 2% → 0%). | Drop-newest blinds you during incidents (when fresh telemetry matters most). Static hedge budget injects more load into a degrading system. Both must be feedback-controlled. |

Plus a new section: **Latency-budget enforcement in CI** — per-PR criterion benchmarks, smoke k6 at 5K, nightly chaos + ramp + memory-leak runs, regression-fails-the-build. So the budgets aren't just declared, they're enforced by tooling at PR time.

## Changelog — v3: implementation hazards (caught before coding)

Two implementation-time pitfalls flagged in the final design review. Both are silent failure modes — they don't cause obvious crashes; they cause subtle data corruption or misleading benchmark numbers. Documenting them here so the implementation honors the constraints from the first commit.

| Hazard | Where it bites | What we do about it |
|---|---|---|
| **RoaringBitmap shared-ref mutation** | Per-request candidate retrieval. The inverted indices live in `Arc<ArcSwap<CampaignCatalog>>` and are read by every concurrent request. Calling `or_inplace` / `and_inplace` / `sub_inplace` on a borrowed reference into the catalog mutates the canonical index → silently corrupts results for all subsequent requests. | Encapsulated in `CampaignCatalog::candidates_for(...)` which always clones the first bitmap into a per-request working set before applying in-place ops. Raw index access is `pub(crate)` and routes through a clone-first helper. Documented as a contract; clippy lint or a wrapper newtype enforces it if abuse becomes possible. |
| **Uniform-random seed data hides Roaring perf characteristics** | Phase 3 seed script + Phase 4 benchmarks. Real ad-tech data is Zipfian (a few segments cover most campaigns; long tail is sparse). Uniform random data produces uniform-density bitmaps that compress poorly and intersect at one speed; real production data has a mix of dense and very sparse bitmaps. Bench numbers from uniform data don't predict production behavior. | `seed-postgres.py` generates Zipfian distributions (α ≈ 1.0-1.2) for segment popularity, geo distribution, daypart, and freq-cap counter density. Seed script also outputs a uniform-random variant for A/B comparison so we can confirm the hot path behaves correctly under both shapes — and so flame graphs reflect real workload characteristics, not a synthetic best case. |

## Changelog — v4: operational realism (caught in final review)

A final operational pass from a smaller-model review surfaced four real issues. Two were correct, two were partial — documented honestly here:

| Concern raised | Verdict | What changed |
|---|---|---|
| Hedge trigger `max(p95, floor)` becomes useless when p95 climbs | **Partial agree.** The reviewer's proposed fix (`min(p95, ceiling)`) was wrong — it would hedge *more* as Redis degrades, amplifying load. The correct fix is making the circuit breaker latency-aware: slow-call ratio (calls > p99-budget × 2) triggers breaker open in addition to error rate. When Redis degrades, breaker opens → guardrail #3 (no hedging when breaker open) kicks in automatically. Trigger formula stays `max(p95, 8 ms floor)`. | Phase 5 circuit breaker description updated to specify slow-call-ratio as an open condition, not just error rate. |
| Catalog rebuild blocking / partial visibility | **Fully agree.** Rebuilding inverted indices over 100K campaigns takes 200-500ms; needed an explicit non-blocking contract. | Phase 3 now spells out the rebuild contract: dedicated background task, fully off-thread build, single `ArcSwap::store` swap, in-flight requests finish on old `Arc`, failure handling with circuit breaker, incremental rebuild explicitly out of scope for v1. |
| Multi-process memory duplication unstated | **Fully agree, just a doc gap.** Catalog (~50-150 MB) + ML model (~10-50 MB) duplicated across 16 cores = 1-3 GB total. Surprises a K8s memory-limit setter. | Concurrency-comparison table now states the memory duplication explicitly with size estimates and the K8s-sizing implication. |
| Missing warmup phase | **Fully agree, big omission.** Cold pod at 30K RPS = 900K degraded requests for ~30s; readiness probe should gate this. | Phase 1 now defines a 5-step warmup contract: catalog load → connection priming → hot-cache pre-population (50K hot users) → memory pre-touch → 100-request self-test. Pod stays out of LB rotation until complete; tracked via `bidder.warmup.duration_seconds`. |
| 5% criterion regression threshold too tight | **Agree, but tune the numbers.** Reviewer suggested 7-10%; rolling median is even better. | CI section now uses rolling median over last 20 main-branch runs + 2σ, with a 7% absolute floor. Single-run fail requires both conditions; main-branch tightens automatically after 3 consecutive low-noise runs. |
| Pre-Phase-1 lock-ins (Redis schema, DB schema, segment IDs, golden request, golden k6) | **Fully agree, was missing.** These are foundational — getting them wrong means rework in Phase 3-4. | New **Phase 0 — Lock-in deliverables** section before Phase 1. Five named artifacts, all committed to the repo before any Phase 1 code. The golden request and golden k6 script become the immutable truth baselines for every bench claim. |

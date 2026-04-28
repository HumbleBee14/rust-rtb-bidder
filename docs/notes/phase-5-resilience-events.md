# Phase 5 — Resilience + Events + Full Observability

## Goal

Formalize resilience (circuit breakers, hedged reads) and add Kafka event publishing with
protobuf encoding. Every external dependency now has an explicit failure mode, a timeout, and
a circuit breaker. The Kafka bid/win event stream is the foundation for downstream analytics,
budget reconciliation, and fraud detection.

---

## Architecture overview

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                            HTTP Layer (axum)                                 │
│  POST /rtb/openrtb/bid              GET /rtb/win                             │
└───────────────┬─────────────────────────────┬────────────────────────────────┘
                │                             │
                ▼                             ▼
┌──────────────────────────┐   ┌──────────────────────────────────────────────┐
│     bid() handler        │   │  win() handler                               │
│                          │   │  - records ImpressionEvent (freq-cap)        │
│  1. parse body (simd-json│   │  - publishes WinEvent → Kafka                │
│  2. pipeline.execute()   │   │  - returns 200 OK                            │
│  3. serialize response   │   └──────────────────────────────────────────────┘
│  4. for each winner:     │
│     - tokio::spawn →     │
│       EventPublisher     │
│       .publish(BidEvent) │
└──────────────────────────┘

                Pipeline stages (unchanged from Phase 4)
                ┌─────────────────────────────────────┐
                │ RequestValidation                   │
                │ UserEnrichment  ◄── RedisSegmentRepo│ ←┐
                │ CandidateRetrieval                  │  ├── CircuitBreaker (redis) — shared
                │ CandidateLimit                      │  │
                │ Scoring                             │  │
                │ FreqCap         ◄── RedisFreqCapper │ ←┘
                │ BudgetPacing                        │
                │ Ranking                             │
                │ ResponseBuild                       │
                └─────────────────────────────────────┘

EventPublisher trait
        │
        ├── KafkaEventPublisher (rdkafka FutureProducer, cmake-build)
        │       - bounded internal queue (100K messages)
        │       - fire-and-forget: bid path spawns a detached task
        │       - overflow → bidder.kafka.events_dropped counter + warn log
        │
        └── NoOpEventPublisher
                - fallback when Kafka broker is unreachable at startup

Event schema (bidder-protos/proto/events.proto)
        AdEvent { oneof body {
            BidEvent        ← published by bid handler per winner
            WinEvent        ← published by /rtb/win handler
            ImpressionEvent ← future (Phase 7+)
            ClickEvent      ← future (Phase 7+)
            VideoQuartileEvent ← future (Phase 7+)
        }}

CircuitBreaker state machine (bidder-core::breaker)
        Closed ──(error_rate ≥ 0.5 OR slow_rate ≥ 0.5, min 20 calls)──► Open
          ▲                                                                │
          │ (probe success)                                               │ (open_duration = 10s)
          │                                                               ▼
        HalfOpen ◄──────────────────────────────────────────────────────

        Open:     all requests short-circuit immediately
        HalfOpen: exactly one probe in flight at a time (no thundering herd)
        Closed:   normal operation; sliding window of 100 calls

Hedged Redis reads (bidder-core::hedge)
        First call issued
            │
            ├── returns within trigger (max(p95, 8ms)) ──► use result
            │
            └── trigger elapsed — evaluate 4 guardrails:
                  1. Idempotent read? (enforced by usage convention)
                  2. Latency trigger exceeded?       (always true here)
                  3. breaker.is_closed_sync()?       (health gate)
                  4. budget.try_consume()?           (adaptive token bucket)
                       ├── all 4 pass → spawn hedge, race both, return first
                       └── any fails → wait for original, increment hedge_blocked

        HedgeBudget capacity contracts on load-shed:
          load_shed_rate > 0.01  →  20% of nominal capacity
          load_shed_rate > 0.05  →  0%  (hedging fully disabled)

OTel span tree (Tempo)
        bid_request                          ← #[instrument] on bid()
          └── pipeline.execute
                ├── request_validation
                ├── user_enrichment
                │     └── redis.get          ← #[instrument] on segments_for()
                │           db.system=redis, db.operation=GET
                ├── candidate_retrieval
                ├── candidate_limit
                ├── scoring
                ├── frequency_cap
                │     └── redis.mget         ← #[instrument] on check()
                │           db.system=redis, db.operation=MGET
                ├── budget_pacing
                ├── ranking
                └── response_build
          └── kafka.produce (detached)       ← #[instrument] on publish()
                messaging.system=kafka
```

---

## New modules and files

| Path | What it is |
|---|---|
| `bidder-protos/` | New workspace member. `prost 0.13` + `protoc-bin-vendored 3.0`. Vendored protoc binary — no system protoc required. |
| `bidder-protos/proto/events.proto` | `AdEvent` oneof schema for all Kafka events. |
| `bidder-core/src/events.rs` | `EventPublisher` trait + `NoOpEventPublisher`. |
| `bidder-core/src/breaker/mod.rs` | `CircuitBreaker`, `BreakerConfig`, `BreakerState`. |
| `bidder-core/src/hedge.rs` | `HedgeBudget`, `RedisHedgeState`, `hedged_call()`. |
| `bidder-server/src/kafka.rs` | `KafkaEventPublisher` using `rdkafka::FutureProducer`. |
| `bidder-server/src/server/handlers.rs` | `win()` handler added; `bid()` now publishes `BidEvent`. |
| `bidder-server/src/server/routes.rs` | `GET /rtb/win` route wired. |
| `bidder-server/src/server/state.rs` | `event_publisher: Arc<dyn EventPublisher>` added to `AppState`. |

---

## Config additions (`config.toml`)

```toml
[kafka]
brokers = "localhost:9092"
events_topic = "bidder.events.v1"
queue_capacity = 100000       # rdkafka internal queue depth; overflow drops newest
send_timeout_ms = 50          # background; never blocks bid path
drop_policy = "newest"        # newest | oldest | random_sample | incident_mode
```

---

## Kafka drop policy (explicit)

The `drop_policy` field is deserialized as `KafkaDropPolicy` enum in `bidder-core::config`.
`incident_mode` auto-activation (flip to `random_sample` when `events_dropped > 1%` for 5 min)
is implemented in config but the monitoring loop is not yet wired — deferred to Phase 7.

---

## What is NOT in Phase 5

- `HedgeBudget::set_load_shed_rate()` is implemented but not wired. Requires a load-shed rate
  gauge from the axum concurrency-limit layer. Deferred to Phase 7 (kernel tuning phase).
- `RedisHedgeState::update_p95()` is implemented but not wired. Requires a histogram reader
  from the Prometheus metrics registry. Deferred to Phase 7.
- Per-campaign freq-cap limits from catalog (still hard-coded day=10, hour=3). Phase 6.
- `incident_mode` auto-activation monitoring loop. Phase 7.

---

## Rust-specific decisions and surprises

### `rdkafka` requires cmake on macOS
`cmake-build` feature compiles librdkafka from source via cmake. Not installed by default on macOS.
Fixed with `brew install cmake`. Linux CI already has it. Noted for new contributor setup docs.

### `protoc-bin-vendored` eliminates the system protoc dependency
`build.rs` sets `PROTOC` via `protoc_bin_vendored::protoc_bin_path()` before calling `prost_build`.
This means `cargo build` works out of the box on any machine with a C toolchain, no protoc install.

### Circuit breaker `Window` counters are not perfectly atomic across snapshot+reset
Between `snapshot()` and `reset()`, racing threads can increment counters. The window resets with
slightly stale data. This is intentional: a mutex here would add contention on every hot-path call.
The circuit breaker is a heuristic — ±a few counts in a 100-call window doesn't change correctness.

### `HedgeBudget::restore()` is not perfectly atomic
`fetch_min(cap)` then `fetch_add(1)` are two separate atomic operations. At very high concurrency
tokens can briefly exceed capacity by the number of concurrent restores. This is a soft cap, not
a hard guarantee — the excess is bounded and harmless.

### `is_closed_sync()` uses `try_read()` for lock-free fast path
The hedge guardrail check is on the hot path (every Redis call). Acquiring a full async read lock
would add latency. `try_read()` returns `Err` if the write lock is held (state transition in
progress). In that case, `is_closed_sync()` returns `false`, which conservatively blocks the hedge.
This is correct: state transitions are rare; refusing one hedge during a transition is harmless.

### Kafka publish is fire-and-forget via `tokio::spawn`
Each winner spawns a detached task. The bid handler never awaits the publish. If the Kafka producer
queue is full, `FutureRecord::send` with a zero timeout returns immediately with an error. The task
increments the dropped counter and exits. This guarantees Kafka slowness can never inflate bid p99.

### `BidEvent.bid_price_micros = bid_price_cents * 10_000`
OpenRTB uses USD cents internally in this codebase. The proto schema uses microdollars (1 USD =
1,000,000 microdollars). Conversion: cents × 10,000 = microdollars. Cast to `i64` before multiply
to avoid `i32` overflow on large bids.

---

## Post-PR review fixes (phase-5 branch)

These bugs were caught in code review and fixed before merge.

### `hedged_call` issued 3 calls instead of 2
Original: `timeout(trigger, first_call)` drops the first future when the timer fires, then always
issues a second call — so every hedged request issues 2 calls minimum, and on the slow path 3.
Fix: pin the original future with `tokio::pin!` and race it against `sleep(trigger)` via
`tokio::select!`. The original stays in-flight across the trigger boundary. Only one extra call is
ever issued per request.

### `HedgeBudget::restore()` could exceed capacity
Original: `fetch_min(cap)` before `fetch_add(1)` clamped first, then added — tokens could reach
`cap + N` under concurrent restores. Fix: `fetch_add(1)` then `fetch_min(cap)`.

### Half-open TOCTOU: two probes could be granted simultaneously
`allow_request()` read `half_open_probe_in_flight` under a read lock, then returned. Two concurrent
callers could both observe `false` before either set it `true`. Fix: any non-Closed state falls
through to `try_enter_half_open()` which holds a write lock for the check-and-set.

### Double-counted impressions
`bid()` handler called `try_record()` and `win()` handler also called `try_record()`. Each win was
recorded twice. Fix: `bid()` no longer calls `try_record()`; `win()` is the sole impression
recorder (it fires only on confirmed SSP wins).

### `BidEvent` published before serialization could fail
Original: serialization happened after events were spawned — a serialize failure would return 500
but phantom BidEvents were already in flight. Fix: serialize first; only spawn event tasks after
`serde_json::to_vec` succeeds.

### `events_topic` hardcoded in win handler
Win handler had `"bidder.events.v1"` hardcoded. Fix: threaded `events_topic: Arc<str>` through
`AppState`, driven by `config.kafka.events_topic`.

### `/rtb/win` shared the bid path concurrency limit
Win-notice endpoint was on the same `ConcurrencyLimitLayer` as the bid path. High win-notice
traffic could starve bid slots. Fix: split into two separate routers; win router has no concurrency
limit and its own 500 ms timeout (`latency_budget.win_timeout_ms`).

### Per-call-site circuit breakers were independent
`FrequencyCapper` and `SegmentRepo` each constructed their own `CircuitBreaker`. A Redis failure
observed by freq-cap would not open the segment-repo breaker. Fix: one shared `Arc<CircuitBreaker>`
constructed in `main()` and passed to both. Both call sites now observe the same Redis health.

### `hedged_call` not wired at Redis call sites
`HedgeBudget` and `hedged_call` existed but both Redis call sites (`RedisFrequencyCapper`,
`RedisSegmentRepo`) were not using them. Fix: both call sites now wrap their Redis ops with
`hedged_call()` using the 8 ms floor trigger.

### Known gap: `/rtb/win` has no authentication or SSID validation
Any caller can fire win notices, inflating freq-cap counters. Auth/anti-replay is deferred to
Phase 7 (shared secret header or signed token per SSP). Documented here so it is not forgotten.

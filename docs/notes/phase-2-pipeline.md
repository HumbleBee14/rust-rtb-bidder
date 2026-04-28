# Phase 2 — Pipeline architecture: implementation notes

## What was built

`bidder-core::model`: Full OpenRTB 2.6 type system — `BidRequest`, `BidResponse`, `Imp`,
`Banner`, `Video`, `Audio`, `Native`, `Pmp`, `Deal`, `Site`, `App`, `Dooh`, `Device`, `Geo`,
`User`, `Data`, `Segment`, `Eid`, `Source`, `Regs`, `Publisher`, `Content`, `Producer`,
`Format`. `NoBidReason` as a newtype-wrapped `u32` with named constants (not an enum — exchange
extensions add arbitrary codes; a closed enum forces `#[non_exhaustive]` or parse failures).
`AdFormat` enum derived from `Imp`. `AdEvent` sum type with all Phase 5 event variants locked in
now so Phase 5 has no model changes to make.

`bidder-core::model::BidContext`: per-request mutable state threaded through the pipeline. Owned,
heap-allocated, no pooling. Tracks `started_at: Instant` for deadline enforcement and
`outcome: PipelineOutcome` (Pending → NoBid; Bid variant added Phase 4).

`bidder-core::pipeline`: `Stage` trait (RPITIT with explicit `'a` lifetime bound to satisfy
the compiler) + `Pipeline` orchestrator with per-stage budget enforcement. `ErasedStage` wrapper
for object-safe `Vec<Arc<dyn ErasedStage>>` storage — RPITIT is not object-safe so the erased
wrapper boxes the future at the boundary. Type erasure is internal; the public `Stage` trait
stays clean.

`Pipeline::execute`: checks pipeline deadline before each stage, records `bidder.pipeline.stage.duration_seconds`
histogram per stage, fires `bidder.pipeline.stage.budget_exceeded` counter when a stage exceeds
its declared budget, short-circuits on any non-Pending outcome.

`bidder-core::pipeline::stages::RequestValidationStage`: validates `id` non-empty, `imp[]`
non-empty, GDPR consent when `regs.gdpr=1`. Emits per-reason rejected counters.

`bidder-core::pipeline::stages::ResponseBuildStage`: resolves any remaining Pending outcome to
NoBid(NO_ELIGIBLE_BIDS). Phase 4 replaces with real bid construction.

`bidder-server::server::state::AppState`: replaces bare `HealthState` as the axum shared state
carrier. Holds `HealthState` + `Arc<Pipeline>`. Health routes use a nested router with
`HealthState`; bid route uses `AppState`.

`bidder-server` handler: `simd-json::from_slice` on a freshly-allocated `Vec<u8>` copied from
`axum::body::Bytes`. Bytes → Vec copy is intentional — simd-json requires `&mut [u8]`; Bytes is
immutable. jemalloc absorbs the per-request allocation. serde_json used for response serialization
(hot path is parse not serialize; SIMD on write path deferred to Phase 6 profiling).

New crates added: `simd-json 0.17` (SIMD OpenRTB parse), `bytes 1.11` (zero-copy Redis key
buffers, Phase 3+), `uuid 1.23` (bid ID generation, Phase 4+).

## Key decisions made during implementation

**`NoBidReason` as newtype `u32` not enum:** Exchange-defined extensions (e.g. AppNexus uses
`300+` codes) make a closed enum unsafe — unknown values would fail deserialization. Newtype
with associated constants gives named access without exhaustiveness pressure, stays
serde-transparent, and accepts arbitrary exchange codes without `#[non_exhaustive]` gymnastics.

**`AdEvent` sum type defined in Phase 2:** PLAN calls for locking this schema early even though
Phase 5 publishes events. Doing it now means Phase 5 has zero model changes — it only adds the
Kafka publisher. The enum variants match the PLAN exactly.

**RPITIT + explicit `'a` lifetime on `Stage::execute`:** Using `impl Future + 'a` in the trait
signature requires naming the lifetime. The original `async fn execute(&self, ctx: &mut BidContext)`
compiles but the returned future doesn't satisfy the `'a` bound needed by the erased wrapper.
Fixed with explicit `fn execute<'a>(&'a self, ctx: &'a mut BidContext) -> impl Future + Send + 'a`.

**Type erasure via `ErasedStageWrapper`:** RPITIT (return-position impl Trait in traits) is not
object-safe — you can't put `dyn Stage` in a `Vec`. The wrapper boxes the concrete future at the
boundary. The public `Stage` trait stays clean; only the pipeline internals pay the boxing cost,
which is negligible vs the I/O work each stage does.

**`AppState` + nested routers:** Health routes need `HealthState`; bid routes need `AppState`.
Axum requires a single state type per router. Solution: nested routers (`Router::merge`) each
with their own `.with_state(...)`, merged into the top-level router before the middleware stack.
This avoids putting `HealthState` inside `AppState` as a field (which would work but is
redundant — `AppState` already owns `HealthState`). Actually `AppState` does carry
`HealthState` for the `readiness` handler, which uses `State<HealthState>` extracted from the
`AppState`-typed router. This is clean: `AppState` is the server state carrier; `HealthState`
is embedded in it.

**`bidder.pipeline.early_drop` double-emit:** The handler emits this counter on
`NoBidReason::PIPELINE_DEADLINE` and the pipeline itself emits it when the deadline check fires.
Removed the handler-side emit to avoid double counting — the pipeline owns the deadline counter.

## Surprises

- RPITIT lifetime: `async fn` in a trait desugars to a future that captures the receiver
  lifetime. Without an explicit `'a` the compiler can't prove the future outlives the borrow.
  This is a known Rust limitation (stabilized with RPITIT in 1.75 but the lifetime constraint
  isn't inferred for object-safe use). Named lifetime fixes it.
- `tracing::instrument` on `async fn execute<'a>` works fine — the macro handles explicit
  lifetimes correctly.

## Tradeoffs

- **`Vec<u8>` copy for simd-json parse buffer.** `Bytes::into()` copies once. The alternative
  is passing `Bytes::to_mut()` which also copies if the buffer is shared (which axum's body
  always is). No way around the copy — simd-json's contract requires exclusive mutable ownership.
  jemalloc thread-local caches make this cheap; profiling in Phase 3 will confirm.
- **No `bidder-protos` crate yet.** OpenRTB types live in `bidder-core::model::openrtb` as
  serde structs, not protobuf-generated code. `bidder-protos` (Phase 2 PLAN item) is deferred
  to Phase 3 — it's needed for Kafka event serialization (Phase 5), not for OpenRTB JSON
  parsing. Adding it now would mean an empty crate or premature `.proto` authoring.
- **`uuid` added but not used yet.** Bid ID generation is Phase 4. Crate added now so the
  dep is in the workspace graph for Phase 4 without a Cargo.toml change mid-phase.

## What was deferred

- `bidder-protos` crate (Phase 3/5) — OpenRTB parse doesn't need protobuf; Kafka events do.
- Real bid construction (`PipelineOutcome::Bid` variant, `SeatBid`/`Bid` population) — Phase 4.
- Candidate retrieval, scoring, freq-cap, ranking stages — Phase 3/4.
- `simd-json` for response serialization — Phase 6 profiling decision.
- `bytes::Bytes` for Redis key buffers — Phase 3 when Redis client is wired.
- `uuid` in active use — Phase 4 bid ID generation.

## PLAN.md audit

All Phase 2 deliverables present:
- OpenRTB 2.6 full type system: ✓
- `BidContext` per-request state: ✓
- `Stage` trait + `Pipeline` orchestrator: ✓
- Per-stage budget enforcement via metrics: ✓
- `simd-json` for incoming parse: ✓
- `serde_json` for response write: ✓
- `enum NoBidReason` exhaustive at boundary: ✓ (newtype, not enum — documented above)
- `RequestValidationStage`: ✓
- `ResponseBuildStage` (hardcoded no-bid): ✓
- `tracing::instrument` on every stage: ✓
- Multi-impression `Vec<Imp>` from day one: ✓
- `enum AdEvent` sum type: ✓
- Latency budget table in `config.toml`: ✓ (Phase 1 deliverable, unchanged)

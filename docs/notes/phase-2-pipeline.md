# Phase 2 — Pipeline architecture

## What this phase delivers

The request pipeline: OpenRTB 2.6 type system, a `Stage` trait, a `Pipeline` orchestrator with per-stage deadline enforcement, and two concrete stages (validation + response build). Any future stage is one `impl Stage` away. The bid handler now parses real OpenRTB JSON and routes it through the pipeline.

---

## Component map

```
  POST /rtb/openrtb/bid
        │
        │  Bytes (immutable, shared ref)
        ▼
  ┌─────────────────────────────────────────────────────┐
  │ bid handler (bidder-server)                         │
  │                                                     │
  │  Bytes → Vec<u8>   simd-json requires &mut [u8];    │
  │                    copy once, jemalloc absorbs it   │
  │                                                     │
  │  simd_json::from_slice(&mut buf) → BidRequest       │
  │  BidContext::new(request)                           │
  │  pipeline.execute(&mut ctx).await                   │
  └──────────────────┬──────────────────────────────────┘
                     │
                     ▼
  ┌─────────────────────────────────────────────────────┐
  │ Pipeline::execute (bidder-core)                     │
  │                                                     │
  │  for each stage:                                    │
  │    ┌─────────────────────────────────────────────┐  │
  │    │ deadline check: elapsed > pipeline_deadline?│  │
  │    │  yes → NoBid(PIPELINE_DEADLINE), return     │  │
  │    └─────────────────────────────────────────────┘  │
  │    ┌─────────────────────────────────────────────┐  │
  │    │ stage.execute(&mut ctx).instrument(span)    │  │
  │    │  ↳ records stage.duration_seconds histogram │  │
  │    │  ↳ fires budget_exceeded counter if slow    │  │
  │    └─────────────────────────────────────────────┘  │
  │    if ctx.outcome != Pending: break                 │
  │                                                     │
  └──────────┬──────────────────────┬───────────────────┘
             │                      │
             ▼                      ▼
  ┌──────────────────┐   ┌───────────────────────────┐
  │ RequestValidation│   │ ResponseBuildStage        │
  │ Stage            │   │                           │
  │  • id non-empty  │   │  Pending → NoBid(         │
  │  • imp[] present │   │    NO_ELIGIBLE_BIDS)      │
  │  • GDPR consent  │   │  (Phase 4: real bid here) │
  └──────────────────┘   └───────────────────────────┘
```

---

## The Stage trait and type erasure

Rust's RPITIT (return-position `impl Trait` in traits) is not object-safe, so `Vec<Box<dyn Stage>>` doesn't compile directly. The solution splits into two layers:

```
Public API (what stage authors implement):

  trait Stage {
      fn name(&self) -> &'static str;
      fn execute<'a>(&'a self, ctx: &'a mut BidContext)
          -> impl Future<Output = anyhow::Result<()>> + Send + 'a;
  }

  The explicit 'a lifetime is required: async fn desugars to a future
  that captures the receiver. Without naming 'a, the compiler can't prove
  the future outlives the borrow — a known Rust limitation with RPITIT.


Pipeline internals (type-erased storage):

  trait ErasedStage: Send + Sync {
      fn name(&self) -> &'static str;
      fn execute_erased<'a>(&'a self, ctx: &'a mut BidContext)
          -> BoxFuture<'a, anyhow::Result<()>>;
  }

  struct ErasedStageWrapper<S>(S);

  impl<S: Stage + Send + Sync> ErasedStage for ErasedStageWrapper<S> {
      fn execute_erased<'a>(...) -> BoxFuture<'a, ...> {
          Box::pin(self.0.execute(ctx))   // ← box the future here, once
      }
  }

  Vec<Box<dyn ErasedStage>>  ← pipeline storage, object-safe
```

The boxing cost (one heap allocation per stage invocation) is negligible compared to the I/O work each stage does.

---

## BidContext — per-request state carrier

```rust
struct BidContext {
    request:     BidRequest,    // parsed OpenRTB request
    started_at:  Instant,       // pipeline deadline reference
    outcome:     PipelineOutcome, // Pending → NoBid | Bid (Phase 4)
    segment_ids: Vec<SegmentId>, // populated by UserEnrichmentStage (Phase 3)
    catalog:     Option<Arc<CampaignCatalog>>, // snapshot for this request (Phase 3)
}
```

Owned, heap-allocated, not pooled. One per request, dropped when the handler returns. Arena allocation is a Phase 7 experiment — the lifetime across `.await` boundaries is fragile in async Rust.

---

## OpenRTB 2.6 type system decisions

**`NoBidReason` as `u32` newtype, not enum:**
Exchange-defined extension codes (AppNexus uses 300+, Google AdX uses its own set) make a closed enum unsafe — unknown values fail deserialization. A newtype with associated constants gives named access without exhaustiveness pressure and accepts arbitrary codes transparently.

```rust
pub struct NoBidReason(pub u32);
impl NoBidReason {
    pub const NO_ELIGIBLE_BIDS: Self = Self(2);
    pub const PIPELINE_DEADLINE: Self = Self(100);
    // ...
}
```

**`Site::ref_` field rename:**
OpenRTB uses `ref` as a field name (the referring URL). `ref` is a Rust keyword, so the struct field is `ref_` with `#[serde(rename = "ref")]` to round-trip correctly.

**`AdEvent` sum type defined in Phase 2:**
Phase 5 publishes events, but locking in the schema early means Phase 5 adds a publisher without touching the model. The variants (`Bid`, `Win`, `Impression`, `Click`, `VideoQuartile`, `Conversion`, `ViewabilityMeasured`) match the PLAN exactly.

---

## Per-stage budget enforcement

Each stage has a declared budget in `config.toml` under `[latency_budget]`. The pipeline reads this config at construction time. After each stage completes:

```
stage_duration > budget  →  counter!("bidder.pipeline.stage.budget_exceeded", stage = name)
always                   →  histogram!("bidder.pipeline.stage.duration_seconds", stage = name)
```

The pipeline deadline (40 ms) is checked **before** each stage starts. If elapsed > deadline, the outcome is set to `NoBid(PIPELINE_DEADLINE)` and the pipeline short-circuits. The stage itself never starts, so no partial execution.

---

## State threading in axum

Different routes need different state types:
- `/health/*` needs `HealthState` (just the ready flag)
- `/rtb/*` needs `AppState` (health + pipeline + later catalog/redis)

Axum requires one state type per router. Solution: nested routers merged before the middleware stack.

```rust
let health_router = Router::new()
    .route("/health/live",  get(liveness))
    .route("/health/ready", get(readiness))
    .with_state(health_state);           // ← HealthState

let bid_router = Router::new()
    .route("/rtb/openrtb/bid", post(bid))
    .with_state(app_state);              // ← AppState (contains HealthState)

Router::new()
    .merge(health_router)
    .merge(bid_router)
    .layer(metrics_layer)               // ← outermost
```

---

## PLAN.md audit — Phase 2

| Deliverable | Status |
|---|---|
| OpenRTB 2.6 full type system | ✓ |
| `BidContext` per-request state | ✓ |
| `Stage` trait + `Pipeline` orchestrator | ✓ |
| Per-stage budget enforcement via metrics | ✓ |
| Pipeline deadline (40 ms) enforcement | ✓ |
| `simd-json` for incoming parse | ✓ |
| `serde_json` for response write | ✓ |
| `NoBidReason` newtype (not enum) | ✓ |
| `RequestValidationStage` | ✓ |
| `ResponseBuildStage` (hardcoded no-bid) | ✓ |
| `tracing::instrument` on every stage | ✓ |
| `enum AdEvent` sum type | ✓ |
| Multi-impression `Vec<Imp>` from day one | ✓ |
| Latency budget table in `config.toml` | ✓ |

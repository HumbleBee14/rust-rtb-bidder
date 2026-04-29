# Phase 1 — Foundation

## What this phase delivers

A production-wired HTTP server that returns hardcoded 204s. Every cross-cutting concern that would be painful to retrofit later — observability, graceful shutdown, load shedding, warmup gating — is wired here at zero feature cost. Later phases drop real logic into the existing skeleton.

---

## Component map

```
                         ┌──────────────────────────────────────────────────┐
                         │  bidder-server (binary)                          │
                         │                                                  │
  TCP :8080              │  ┌──────────────┐    ┌─────────────────────────┐ │
 ──────────────────────► │  │ socket2      │    │ Warmup                  │ │
  SO_REUSEPORT           │  │ TcpListener  │    │  1. catalog load (stub) │ │
  SO_REUSEADDR           │  └──────┬───────┘    │  2. conn priming (stub) │ │
                         │         │            │  3. cache prepop (stub) │ │
                         │         ▼            │  4. mem pre-touch (stub)│ │
                         │  ┌──────────────────────────────────────────┐  │ │
                         │  │ Tower middleware stack (outermost→inner) │  │ │
                         │  │                                          │  │ │
                         │  │  MetricsLayer  ──► duration histogram    │  │ │
                         │  │  TraceLayer    ──► OTel span per request │  │ │
                         │  │  TimeoutLayer  ──► 50 ms hard deadline   │  │ │
                         │  │  ConcurrencyLimit ► 503 when queue full  │  │ │
                         │  │                                          │  │ │
                         │  │  Routes:                                 │  │ │
                         │  │   GET  /health/live   → 200              │  │ │
                         │  │   GET  /health/ready  → 200/503          │  │ │
                         │  │   POST /rtb/openrtb/bid → 204 (hardcode) │  │ │
                         │  └──────────────────────────────────────────┘  │ │
                         │                                                │ │
                         │  Metrics server :9090 (separate port)          │ │
                         │   GET /metrics → Prometheus exposition         │ │
                         └──────────────────────────────────────────────────┘
                         
  bidder-core (library)
  ├── config/    figment: TOML + BIDDER__* env overlay
  ├── telemetry/ OTel init → OTLP/HTTP → Tempo
  ├── metrics/   metrics-exporter-prometheus init
  ├── health/    HealthState (AtomicBool ready flag)
  └── error/     top-level error type
```

---

## Key decisions and why

### Metrics on a separate port (:9090), not /metrics on :8080

Prometheus scrape traffic shares nothing with bid traffic. On the same port it would consume a concurrency slot and add noise to the bid p99 histogram. Separate port means the scrape path has zero interaction with load-shed logic.

### Custom timeout middleware instead of tower-http TimeoutLayer

`tower-http`'s `TimeoutLayer` requires `ResBody: Default`, which `axum::body::Body` doesn't satisfy. Implemented as `axum::middleware::from_fn_with_state` with `tokio::time::timeout`. Emits `bidder.http.timeout_total` on expiry, returns 503. No boxing overhead.

### Warmup gates readiness probe

The readiness probe (`/health/ready`) returns 503 until all warmup steps pass. In Phase 1, steps 1–4 are stubs that log and return immediately. Step 5 sends 100 synthetic requests to the local bid endpoint and fails startup if >10 error. This means the Phase 1 server already has the correct startup contract — later phases fill in the stubs with real work.

### Layer ordering: MetricsLayer outermost

```
Request enters:  MetricsLayer → TraceLayer → TimeoutLayer → ConcurrencyLimit → handler
Response exits:  handler → ConcurrencyLimit → TimeoutLayer → TraceLayer → MetricsLayer
```

MetricsLayer is outermost so it measures the full round-trip including timeout overhead. TraceLayer is inside MetricsLayer so the OTel span captures only handler time, not scrape instrumentation overhead.

### SO_REUSEPORT wired from day one

Production deployment runs N processes per node (one per CPU core), each binding the same port. The kernel routes `accept(2)` round-robin. `SO_REUSEPORT` is set via socket2 before the listener is handed to axum. In Phase 1 there's only one process; the socket option is a no-op but it means Phase 7 multi-process deployment needs zero code changes.

---

## Startup sequence

```
main()
  │
  ├── Config::load()          TOML + env vars → Config struct
  ├── telemetry::init()       OTel provider, OTLP exporter, tracing subscriber
  ├── metrics::init()         Prometheus builder → background HTTP server on :9090
  ├── HealthState::new()      ready = false
  ├── build router            ConcurrencyLimit → Timeout → Trace → Metrics → routes
  ├── socket2 listener        SO_REUSEPORT + SO_REUSEADDR on cfg.server.bind
  │
  ├── tokio::spawn(axum::serve(...))    ← server accepting before warmup completes
  │                                       readiness probe returns 503 here
  │
  ├── warmup::run()
  │    ├── step 1–4: stubs (log + return)
  │    └── step 5:  100 synthetic POST /rtb/openrtb/bid, fail if >10 errors
  │         └── exponential backoff connect retry: 10 → 20 → 40 → 80 ms
  │
  ├── health.set_ready()       readiness probe now returns 200
  │
  └── server_handle.await()    blocks until SIGTERM / Ctrl-C
        └── graceful_shutdown()  drains in-flight requests, then exits
```

---

## What was deferred and why

| Deferred | Why | Phase |
|---|---|---|
| Warmup steps 1–4 | No Redis/Postgres/catalog yet | 3 |
| Per-stage budget enforcement | No pipeline stages yet | 2 |
| `bidder-protos` crate | No `.proto` files exist | 5 |
| `SO_BUSY_POLL` | Linux-only; needs profiling to justify | 7 |
| Multi-process `SO_REUSEPORT` deployment | Single process in dev | 7 |

---

## PLAN.md audit — Phase 1

| Deliverable | Status |
|---|---|
| Cargo workspace: server + core + bench | ✓ |
| jemalloc global allocator | ✓ |
| axum on tokio multi-threaded | ✓ |
| Tower stack: ConcurrencyLimit, Timeout, Trace, Metrics | ✓ |
| figment config (TOML + env) | ✓ |
| Structured JSON logs (Loki-compatible) | ✓ |
| OTel tracing → Tempo via OTLP/HTTP | ✓ |
| Head-based sampling (errors 100%, success 1%) | ✓ |
| Prometheus metrics exposition | ✓ (separate :9090) |
| Graceful SIGTERM shutdown | ✓ |
| Liveness / readiness probe split | ✓ |
| SO_REUSEPORT listener | ✓ |
| Warmup skeleton (steps 1–4 stubs, step 5 self-test) | ✓ |
| Multi-stage Dockerfile (distroless runtime) | ✓ |
| CI matrix (macOS + Linux x86 + Linux arm64) | ✓ |

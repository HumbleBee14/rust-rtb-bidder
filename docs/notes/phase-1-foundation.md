# Phase 1 — Foundation: implementation notes

## What was built

Cargo workspace with three members (`bidder-core`, `bidder-server`, `bidder-bench`). `bidder-protos` deferred to Phase 2 — no `.proto` to compile yet.

`bidder-core`: figment config loader (TOML + `BIDDER__`-prefixed env overlay), OTel tracing init with head-based sampler, Prometheus metrics init, `HealthState` (atomic ready flag), top-level error type.

`bidder-server`: axum on tokio multi-thread, tower layer stack (ConcurrencyLimit → axum middleware timeout → TraceLayer → MetricsLayer), three routes (`/health/live`, `/health/ready`, `POST /rtb/openrtb/bid` → hardcoded 204), `SO_REUSEPORT`+`SO_REUSEADDR` listener via socket2, 5-step warmup skeleton (steps 1-4 are no-ops with logged placeholders; step 5 sends 100 synthetic requests to the local endpoint and fails startup if >10 errors), graceful SIGTERM/SIGINT drain, jemalloc as `#[global_allocator]`.

`bidder-bench`: criterion harness with a no-op bench so `cargo bench --no-run` compiles in CI.

Dockerfile (distroless/cc-debian12 runtime, ~15-20 MB), Dockerfile.dev (rust:bookworm with cargo-watch), CI matrix (macos-latest + ubuntu-latest + ubuntu-24.04-arm, default + all-features).

## Key decisions made during implementation

**Timeout middleware:** `tower-http`'s `TimeoutLayer` requires `ResBody: Default`, which `axum::body::Body` doesn't satisfy. Used an axum `middleware::from_fn_with_state` layer with `tokio::time::timeout` instead. Emits `bidder.http.timeout_total` on expiry and returns 503. Straightforward and idiomatic; no boxing overhead.

**OTel sampler:** `Sampler::ParentBased(TraceIdRatioBased(success_sample_rate))`. This means 100% of error spans are sampled when the parent span is marked as error. SLA-violation sampling (spans exceeding budget) is deferred to Phase 2 when per-stage timing exists — Phase 1 has no pipeline stages to instrument.

**`bidder-protos` skipped:** agreed with user pre-implementation. Zero dead weight in Phase 1.

**OTel pretty log format:** `tracing-opentelemetry`'s `OpenTelemetryLayer` requires `JsonFields` in the subscriber stack. Both log format branches use `.json()` internally (formatter outputs pretty-printed JSON in pretty mode); raw `.pretty()` without `.json()` breaks the layer's trait bound.

**reqwest in `bidder-core`:** the OTLP HTTP exporter requires an explicit `reqwest::Client` via `WithHttpConfig::with_http_client` — the `reqwest-client` feature doesn't auto-install one. Added `reqwest` to `bidder-core` deps.

**Warmup self-test:** 100 POST requests to the local endpoint, tolerates up to 10 failures (for slow CI environments). Completes in <10ms locally. The server is spawned before warmup so the self-test has something to hit; a 50ms sleep gives the tokio accept loop time to start.

## Surprises

- `opentelemetry_sdk 0.31` renamed `TracerProvider` → `SdkTracerProvider`. Minor but breaks any copy from older examples.
- `tower-http 0.6` deprecated `TimeoutLayer::new` in favor of `TimeoutLayer::with_status_code(status, duration)` (note: status first, duration second — opposite of the intuitive order). Moot since we moved off tower-http's timeout entirely.
- `opentelemetry-otlp`'s `with_http()` builder silently requires `.with_http_client(reqwest::Client::new())` when the `reqwest-client` feature is enabled; without it, the exporter errors at runtime with "no http client specified". Nothing in the compile-time API indicates this.

## Tradeoffs

- **No `axum-server` or custom listener wrapper.** Raw `axum::serve(TcpListener, Router)` is cleaner and sufficient. `SO_REUSEPORT` is set directly via socket2 before hand-off.
- **Warmup step 5 hits the bid endpoint, not a health check.** Tests the actual request path with real axum routing. Trade-off: tight coupling to the bind address being known at warmup time. Acceptable — we control the address.
- **Metrics on `:9090` separate from the bid port, not on `/metrics` at `:8080`.** PLAN.md says `metrics-exporter-prometheus` on `/metrics`; the implementation serves it on a dedicated port via `PrometheusBuilder::with_http_listener`. Rationale: Prometheus scrape traffic doesn't share a ConcurrencyLimit slot with bid traffic and doesn't add latency to the bid path. The separate port is explicitly named in `config.toml` (`[metrics] bind`). Phase 2+ can add a `/metrics` axum route proxying the handle if a single-port constraint appears.

## What was deferred

- `bidder-protos` crate (Phase 2) — agreed pre-implementation; no `.proto` files exist yet.
- `SO_BUSY_POLL` in the `linux-tuning` feature (Phase 7); placeholder comment in socket.rs.
- Per-stage latency budget enforcement in the tower layer (Phase 2 — no pipeline stages exist yet to enforce against).
- Warmup steps 1–4 (catalog load, connection priming, hot-cache prepop, memory pre-touch) — all log a skip message and return immediately. Phase 3 populates them.
- OTel SLA-violation sampling (>40ms spans automatically sampled at 100%) — wired in Phase 2 with the pipeline deadline counter.
- `docker compose up bidder` service block — PLAN references the Java repo's `docker-compose.yml`; the `bidder:` service entry is a Phase 2 deliverable once the server has real deps (Redis, Postgres) to wire up in compose.
- **Profiling baseline (samply, tokio-console, k6 at 5K RPS)** — PLAN lists this as a Phase 1 deliverable, but a meaningful flame graph requires the full backing stack (Redis, Postgres, realistic pipeline work). Deferred to Phase 2 checkpoint: the plan calls for a comparison checkpoint at Phase 3 (`k6 stress at 5K and 10K`) which is the first phase with real pipeline work. Phase 1's server is a hardcoded 204 — profiling it would only show OTel/metrics overhead, not anything actionable. Captured in phase note so it's explicit, not silent.
- CLI per-field config overrides — only `--config` path override is wired. Full per-field CLI override (e.g. `--server.bind 0.0.0.0:9000`) is Phase 2 if needed; figment's env overlay covers the production use case.

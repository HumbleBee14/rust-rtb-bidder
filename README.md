# Rust RTB Bidder
A production-shaped Real-Time Bidding (RTB) DSP bidder, built in Rust.

It receives OpenRTB 2.6 bid requests over HTTP, decides whether to bid (and at what price) within a 50 ms p99 latency budget, and emits structured events for downstream attribution and analytics. Designed for the workload a real demand-side platform actually carries: tens of thousands of active campaigns, a hundred-million-user audience, and the kind of tail-latency discipline production ad-tech requires.

## Overview

A complete bidder - production scale workload. It includes:

- **OpenRTB 2.6 bid endpoint** with a pluggable exchange-adapter trait (multiple wire formats supported, e.g. Google AdX protobuf alongside generic JSON).
- **A staged pipeline** (~10 stages from request validation through scoring, frequency-capping, ranking, and response build), each with its own latency budget and independent observability.
- **A campaign catalog** loaded from Postgres at boot and refreshed atomically every 60 s, with inverted-index targeting on segment / geo / device / format / daypart for sub-millisecond candidate retrieval at 50K-campaign scale.
- **Hot-path Redis usage** for per-user segments and per-(user, campaign) frequency caps, with optional in-process caching that lifts the network round-trip off the bid path entirely.
- **Resilience layers** — circuit breakers, hedged Redis reads, load-shed-aware hedge budget, write-behind impression recording, fail-loud overflow counters.
- **ML scoring** via ONNX Runtime, including a cascade scorer (cheap rule-based pre-filter feeding ML on top-K survivors) and an A/B-test scorer.
- **Observability** — Prometheus metrics on every stage, OpenTelemetry traces (head sampling by default, opt-in tail sampling via collector), structured logs, win-notice HMAC verification with per-SSP secrets.
- **A complete load-test harness** — k6 stress sweep (5K → 50K RPS) with auto-generated per-tier targets, Markdown summary analyzer, and committed reference numbers.

## High-level architecture

```
                         ┌────────────────────────────────────┐
                         │        Exchange (SSP / AdX)        │
                         └──────────────┬─────────────────────┘
                                        │ HTTP POST /rtb/openrtb/bid
                                        │ HTTP GET  /rtb/win?...
                                        ▼
                  ┌─────────────────────────────────────────────┐
                  │  Transport (axum + tower)                   │
                  │  timeout · concurrency-limit · metrics      │
                  └────────────────────┬────────────────────────┘
                                       │
                  ┌────────────────────▼────────────────────────┐
                  │  Exchange adapter (OpenRTB / AdX / …)       │
                  │  decode → BidRequest                        │
                  └────────────────────┬────────────────────────┘
                                       │
                  ┌────────────────────▼────────────────────────┐
                  │  Pipeline (10 stages, each with budget)     │
                  │  validate → enrich → retrieve candidates →  │
                  │  score → freq-cap → pace → rank → build     │
                  └────────────────────┬────────────────────────┘
                                       │
                                       ▼
                  ┌──────────┐  ┌──────────┐  ┌──────────────┐
                  │ Postgres │  │  Redis   │  │    Kafka     │
                  │ catalog  │  │ hot data │  │   events     │
                  │ 60 s     │  │ per req  │  │ async        │
                  └──────────┘  └──────────┘  └──────────────┘
                       │            │              │
                       │ scrape ────┴──────────────┴── consumer
                       ▼                              pipelines
                  ┌──────────────┐
                  │ Prometheus + │
                  │ Grafana      │
                  └──────────────┘
```

Three classes of dependencies:

- **Hot path** (latency-critical, <1 ms budget per stage): Redis.
- **Cold path** (off the bid loop): Postgres catalog, refreshed every 60 s.
- **Sidecar streams** (fire-and-forget): Kafka events, Prometheus metrics, OpenTelemetry traces.

The bidder decides quickly using everything in process; everything else is buffered or asynchronous so dependency slowness never propagates to bid latency.

## Performance targets

| Metric | Target | Measured (single dev host) |
|---|---|---|
| Sustained RPS | 50K | 50K passes with p99 well under SLA |
| p99 bid latency | < 50 ms | 3.4 ms at 50K |
| p99.9 bid latency | < 200 ms | 39.5 ms at 50K |
| HTTP error rate | < 0.1% | 0.00% across 50M+ stress requests |
| Bid rate stability across tiers | flat | 70 % ± 0.5 % across 5K → 50K |

Full numbers + screenshots: [`docs/results/`](docs/results/).

## Where to start

| If you want to… | Read this |
|---|---|
| Run it locally and stress-test it | [`docs/USAGE.md`](docs/USAGE.md) |
| Understand how it's put together | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) |
| Understand the in-process freq-cap cache (L1/L2, sync, edge cases) | [`docs/IN-PROCESS-FREQUENCY-CACHE.md`](docs/IN-PROCESS-FREQUENCY-CACHE.md) |
| See the workload assumptions and design decisions | [`docs/PLAN.md`](docs/PLAN.md) |
| See real load-test numbers | [`docs/results/`](docs/results/) |
| Understand a specific design decision | [`docs/development-phases/`](docs/development-phases/) |
| Configure something | [`.env.example`](.env.example) |

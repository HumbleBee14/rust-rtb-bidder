# k6 Load Tests

Phase 0, artifact 0.5. The golden load-test script (`golden.js`) is the immutable benchmark contract for `rust-rtb-bidder`. Every phase comparison, regression hunt, monoio-vs-Tokio benchmark, and CI smoke test runs this exact script. If it changes, every prior result is invalidated.

## Purpose

A single source of truth for "what does sustained production-shaped load look like against this bidder." Other DSPs publish numbers from bespoke harnesses; we publish numbers from one script and reference its commit hash.

The script generates a 1000-variation corpus from `tests/fixtures/golden-bid-request.json` at startup and replays it in cycles under a `ramping-arrival-rate` scenario. Variations cover Zipfian user-id skew, segment-popularity skew, geo distribution, and device mix — enough realism to exercise the inverted-index hot path without inflating cache-miss rates artificially.

## Setup

Install k6 0.50+:

```
# macOS
brew install k6

# Linux
# https://grafana.com/docs/k6/latest/set-up/install-k6/
```

The bidder must be reachable on `BIDDER_URL`. For local runs:

1. `docker compose up` for Redis, Postgres, Kafka, ClickHouse, Prometheus, Grafana, Tempo.
2. `cargo run --release -p bidder-server` (or whichever workspace member you are testing).
3. Confirm health: `curl localhost:8080/health`.

## Usage

```
# default 5K RPS, 60s ramp + 300s hold + 60s ramp-down
k6 run k6/golden.js

# specific RPS
TARGET_RPS=10000 k6 run k6/golden.js

# remote bidder
BIDDER_URL=https://staging.bidder.example/rtb/openrtb/bid k6 run k6/golden.js

# JSON metrics for CI ingestion
k6 run --out json=results.json k6/golden.js

# shorter run for quick iteration (still uses the same corpus + thresholds)
RAMP_UP_S=20 HOLD_S=60 RAMP_DOWN_S=20 k6 run k6/golden.js
```

### Environment variables

| Var | Default | Purpose |
|---|---|---|
| `TARGET_RPS` | `5000` | Steady-state arrival rate. |
| `BIDDER_URL` | `http://localhost:8080/rtb/openrtb/bid` | Bid endpoint. Canonical OpenRTB only. |
| `RAMP_UP_S` | `60` | Linear ramp from 0 to `TARGET_RPS`. |
| `HOLD_S` | `300` | Steady-state hold duration. |
| `RAMP_DOWN_S` | `60` | Linear ramp back to 0. |
| `CORPUS_SIZE` | `1000` | Pre-generated request variations replayed in cycles. |

## What gets exercised

- **Zipfian user-id distribution.** 80% of requests target the hot 50K users (matches the Redis warm-set capacity from `docs/REDIS-KEYS.md`); 20% land in the long tail up to `100,000,000`. Inside the hot range, inverse-rank weighting concentrates further toward low ids.
- **Segment-popularity skew.** Each request carries 5-15 segments drawn from the 20-segment seed list in `migrations/0001_initial.sql`, biased toward the front of the list (demographics dominate, niche shopping/b2b segments appear less often).
- **Geo fan-out.** Ten US metros in rotation (NY, LA, Chicago, Dallas, Houston, Seattle, SF, DC, Orlando, Atlanta) — exercises the geo inverted index across distinct slot keys.
- **Device-type mix.** 70% PC (`devicetype=2`), 20% mobile-tablet (`1`), 10% phone (`4`) — exercises device-targeting inverted index branches.
- **Multi-impression handling.** Two imps per request (video with companion ads + banner — derived from IAB OpenRTB 2.6 spec §6.2.4 + §6.2.1). `imp[].id` unique per variation; `bidfloor` preserved.
- **Per-bid Redis MGET.** Each variation carries a fresh `user.id`, so freq-cap and segment cache miss/hit ratios reflect Zipfian access — not a single repeated user.

## What does NOT get exercised

Intentional gaps. The golden stays the happy-path baseline; specialized scripts go alongside under their own filenames.

- **PMP / private deals.** No `imp[].pmp` block.
- **Exchange-specific adapters.** Only `/rtb/openrtb/bid` is hit; `/rtb/adx/bid`, `/rtb/magnite/bid`, etc. get their own scripts.
- **Exotic device types.** No CTV, in-app, audio. Three desktop/mobile types only.
- **GDPR / multi-currency / COPPA.** US traffic, USD only, COPPA off — same persona as the seed fixture.
- **Malformed inputs.** Negative-path load (truncated bodies, oversized payloads, bad enums) lives in separate scripts.
- **Cold cache scenarios.** The 1000-variation corpus replayed in cycles deliberately lets the moka cache warm. Cold-cache scripts override `CORPUS_SIZE` upward and live under their own names.

See `tests/fixtures/README.md` for the broader fixture-vs-edge-case policy.

## Thresholds

```
http_req_failed:    rate < 0.001     # <0.1% non-2xx/3xx
http_req_duration:  p(99) < 80ms, p(99.9) < 200ms
checks:             rate > 0.999
```

The bidder's internal p99 SLA is **50 ms** (see latency budget in `docs/PLAN.md`). The k6 threshold is **80 ms p99** because k6 measures end-to-end wall-clock from the load generator: HTTP handshake, TCP send, server-internal pipeline, response, network return. The 30 ms gap absorbs RTT and client-side scheduling jitter so a passing k6 run does not over-report tail violations that the bidder's own histograms (`pipeline.stage.duration` exposed via Prometheus) would not flag. Server-internal p99 is the real gate; k6 is the external sanity check. p99.9 < 200 ms is the tail-discipline gate — anything beyond is a queue or GC-equivalent pathology that needs a flame graph.

## Per-RPS targets

Maps `TARGET_RPS` to which architectural tier the run validates. See `docs/PLAN.md` performance-targets table for the full tier definitions.

| `TARGET_RPS` | Tier | Phase gate |
|---|---|---|
| 5000 | Phase 1+ smoke | Pipeline correctness, baseline functionality. |
| 10000 | Phase 3 checkpoint | Inverted indices online, candidate retrieval under realistic catalog size. |
| 15000 | Phase 4 hot-path baseline | Freq-cap MGET + scoring + ranking under sustained load. |
| 20000 - 25000 | Tier 1 — baseline Tokio | Multi-threaded Tokio, default tuning. |
| 30000 - 40000 | Tier 2 — tuned | Kernel knobs, simd-json, hedged Redis, jemalloc/mimalloc, allocation hotspots closed. |
| 50000+ | Tier 3 — stretch / monoio | Thread-per-core, CPU pinning, NUMA-aware deployment. |

A run "passes" at a given tier when all three thresholds hold for the full hold window. Sustained sub-threshold latency without errors at the tier RPS is the only pass condition; brief blips during the ramp phases do not count against the gate (k6 reports them but the threshold is computed across the whole run, so a tight ramp + long hold dominates statistically).

## CI integration

The PR smoke test runs `TARGET_RPS=5000 RAMP_UP_S=20 HOLD_S=60 RAMP_DOWN_S=20 k6 run --out json=results.json k6/golden.js` against an ephemeral bidder + Docker stack. Failure on any threshold blocks the PR. Larger RPS sweeps (10K-50K) run on schedule against dedicated hardware, not on every PR.

## Modification policy

This script is immutable except via PR review with explicit baseline-recalibration. A change invalidates every benchmark and load-test number in the project, including every phase-results document that quoted them.

Any results document that quotes numbers from this script must reference the script's commit hash. PR descriptions for changes here must enumerate every consumer affected and link the new baseline numbers.

## Cross-references

- `docs/PLAN.md` — Phase 0 artifact 0.5 spec; latency budget; performance targets table; CI section.
- `tests/fixtures/README.md` — fixture-vs-edge-case policy; the persona the corpus inherits.
- `tests/fixtures/golden-bid-request.json` — the seed the corpus mutates.
- `migrations/0001_initial.sql` — the canonical 20-segment vocabulary.
- `docs/REDIS-KEYS.md` — the `{u:<userId>}` hash-tag rule that constrains user IDs to u32 numerics.
- `docs/SEGMENT-IDS.md` — wire-format segment strings; new names auto-register on first sight.

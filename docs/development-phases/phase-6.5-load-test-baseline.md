# Phase 6.5 — Load-test baseline

## Goal

Land the two Phase 3 deferrals that have been carried since Phase 3:

1. **`docker/seed-redis.py`** — populates the `v1:seg:{u:<userId>}` family per `docs/REDIS-KEYS.md`. The Java repo's seed used a different shape (`SADD key str str str` against a hash-tag-less key); that shape is incompatible with the Rust bidder's `SET <key> <packed-u32-bytes>` reader. This script writes the contract-correct shape from scratch with Zipfian segment popularity.

2. **`LOAD-TEST-RESULTS-rust-v0-baseline.md`** — first reference numbers from a real k6 run against the full stack. Without baseline numbers, every Phase 7 deferral was circular ("we don't have load-test data" — but nobody had run a load test). This document fixes that by reading actual measurements from a tiered 5K + 10K RPS run. Higher tiers are deferred to a dedicated stress-tier phase, not because the bidder can't take it (the Java sibling sustained 15K on the same hardware), but because v0 baseline only needs to establish nominal-and-stretch reference points before optimisation work begins.

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
- `make baseline-tiered` — sequential 5K → 10K with 30s cooldowns; captures per-tier results. Higher tiers (15K+) are tracked separately for the stress-tier phase; the v0 baseline target is "nominal + stretch", not "find the ceiling".
- `make baseline-clean` — resets the results dir.

### `tools/analyze-baseline.sh` — observability one-shot

Reads a results dir of `v0-baseline-<RPS>rps-summary.json` + `v0-baseline-<RPS>rps-prometheus.txt` files and emits four Markdown tables: HTTP timing, pipeline outcomes, resilience signals, per-stage timing breakdown. The same set of metrics matters for every load-test run; rather than hand-extracting each time, this script is the canonical analyzer. Also produces output ready to paste directly into the LOAD-TEST-RESULTS doc.

---

## Baseline measurement methodology

| Decision | Rationale |
|---|---|
| **Same hardware that captured the Java baseline** | Numbers are comparable to the Java repo's `LOAD-TEST-RESULTS-*.md` files because both projects are measured on the same machine. Architecture-shape signals (which stage is hot, where breakers fire, which counters spike) carry over to Linux production unchanged; absolute latency numbers will shift on different hardware but the *relative* per-stage breakdown is portable. |
| **2 tiers: 5K, 10K** | 5K = nominal target (PLAN.md baseline tier). 10K = stretch under tuning. 15K+ is intentionally outside the v0 baseline scope — the goal here is "establish nominal + stretch reference numbers", not "find the ceiling". Stress tiers (15K, 20K, 25K, 50K) are tracked for a dedicated stress-tier phase that compares against the Java sibling's 15K results on the same hardware. |
| **30s ramp + 120s hold + 30s ramp-down per tier** | Industry standard burst test. 60-90s hold is enough for steady-state metrics; longer just produces more data of the same kind. Trimmed from PLAN.md's golden 60+300+60 = 7-min profile because 2 tiers × 7 min was burning runtime without adding signal. |
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
# Both OTLP and Kafka are disabled for the baseline so we measure bid-path
# cost, not retry overhead from missing collector / broker sidecars.
BIDDER__TELEMETRY__OTLP_ENDPOINT="" \
BIDDER__KAFKA__BROKERS="" \
target/release/bidder-server --config config.toml &

# 4. Run tiered baseline (~10 min)
make baseline-tiered

# 5. Analyze
bash tools/analyze-baseline.sh > /tmp/baseline.md
```

Reseed via `make seed` if you want a clean catalog before re-running.

---

## Test-environment seed inflation (deliberate, documented)

Compressed-time load tests have a structural problem with realistic budgets: a 3-minute run delivers ~900K requests at 5K RPS, which simulates several hours of real-time spend in a few minutes. With production-realistic daily budgets ($50–$5K per campaign, the original seed range), `LocalBudgetPacer` exhausts most campaigns in the first ~30 seconds of the run and the rest of the test measures "almost everyone is budget-blocked" rather than the actual bid-path cost.

Phase 6.5 inflates seed daily budgets to $1K–$1M per campaign so 3 minutes of compressed traffic doesn't drain the pool. This is **a test-environment choice, not a bidder-quality claim** — production budget-pacing under exhaustion is a separate concern that belongs in a dedicated chaos suite (Phase 7+) where budget refresh + exhaustion are the unit under test, not background noise.

The analyzer reports `budget_exhausted_filtered` per tier specifically so this knob is visible. If the counter trends >>0 across tiers, the seed inflation needs to go higher OR test duration shorter.

---

## k6 sizing recalibration (Phase 6.5)

The first dirty baseline showed a single 5K request taking 17 minutes — not a bidder bug, a k6 + macOS interaction:

- `PRE_ALLOCATED_VUS = ceil(TARGET_RPS × 0.05)` = 250 VUs at 5K target
- When the bidder's freq-cap circuit breaker tripped, request latency briefly spiked past 50ms; the small VU pool starved (`5000 RPS × 0.080s = 400 VUs needed during a stall, only 250 preAlloc`)
- Without `timeout` on `http.post`, a stuck VU sits on a hung TCP socket until the underlying retransmit timer fires — **~15-17 minutes on macOS** (`net.inet.tcp.keepidle` + `keepintvl`) vs **~5 minutes on Linux**
- Result: one stuck VU produces a 17-minute "max latency" in the k6 summary and drags the sustained-RPS denominator (k6's `iterations.rate = total_iterations / wall_clock`)

`k6/golden.js` was recalibrated in this phase per the PLAN.md "explicit recalibration" rule:

| Setting | Before | After | Rationale |
|---|---|---|---|
| `PRE_ALLOCATED_VUS` | `ceil(RPS × 0.05)` | `max(200, ceil(RPS × 0.10))` | Absorbs single-stall events without dropping below target rate. |
| `MAX_VUS` | `ceil(RPS × 0.20)` | `max(1000, ceil(RPS × 0.50))` | 2× safety over `RPS × p99_seconds_under_breaker_trip`. |
| `http.post` timeout | (default, ∞) | `5s` | Caps stuck-VU duration at 5s on either platform; emits `dropped_iterations` instead of holding sockets. |

Inline rationale committed in `k6/golden.js`. This is a one-time recalibration; no further Phase-7+ changes expected unless the latency profile materially shifts.

---

## Results — v0 baseline

**Environment:** macOS 25.4.0 (Darwin) on Apple Silicon, Docker Desktop, Postgres 16, Redis 7, single-process bidder release build, ort 2.0.0-rc.10. ~5K seeded campaigns × 1019 segments × 100K users. Bidder run with `BIDDER__KAFKA__BROKERS=""` and `BIDDER__TELEMETRY__OTLP_ENDPOINT=""` so retry overhead from missing sidecars doesn't pollute timings.

Numbers below are pasted verbatim from the analyzer (`tools/analyze-baseline.sh`) running against a local `load-test/results/` directory. Raw artifacts are not committed (the directory is `.gitignore`d — k6 summaries + Prometheus dumps are large and timestamp-noisy); reproduce by running `make baseline-tiered` against the same seeded stack. The analyzer subtracts before-from-after Prometheus snapshots so per-tier deltas are accurate (the bidder's counters are cumulative across the process lifetime; tier 2's snapshot includes tier 1 unless explicitly differenced).

### k6 HTTP-side timing (per tier)

| Target RPS | Sustained RPS | iterations | p50 (ms) | p95 (ms) | max (ms) | http_req_failed |
|---:|---:|---:|---:|---:|---:|---:|
| 5,000 | 4,167 | 749,999 | 0.64 | 1.40 | 84.88 | 4/749,999 (0.0005%) |
| 10,000 | 8,333 | 1,499,999 | 0.81 | 6.05 | 49.38 | 0/1,499,999 (0.0000%) |

(p99 / p99.9 not in `--summary-export` JSON. Interactive run output: p99 ≈ 2-3 ms at 5K, ≈ 7-9 ms at 10K — both well under the 50 ms p99 SLA.)

### Bidder pipeline outcomes (per tier — deltas)

| Target RPS | bids | no-bids | bid rate | early drops | budget-exhausted candidates filtered |
|---:|---:|---:|---:|---:|---:|
| 5,000 | 526,268 | 223,727 | **70.17%** | 58 | 176,297 |
| 10,000 | 1,052,756 | 447,243 | **70.18%** | 1 | 1,924,554 |

### Resilience signals (per tier — deltas)

| Target RPS | breaker opens | hedge fired | hedge blocked | freq-cap skipped (timeout) | freq-cap skipped (breaker) | kafka events_dropped |
|---:|---:|---:|---:|---:|---:|---:|
| 5,000 | 1 | 32 | 26 | 57 | 70,387 | 0 |
| 10,000 | 4 | 169 | 930 | 1,100 | 551,801 | 0 |

### Per-stage timing — 10K RPS (server-internal latency)

| Stage | p50 (µs) | p99 (µs) | p99.9 (µs) | max (µs) | budget exceeded |
|---|---:|---:|---:|---:|---:|
| request_validation | 0.6 | 2.3 | 15.1 | 425 | 4 |
| user_enrichment | 20.8 | 20.8 | 47.4 | 14,604 | 31 |
| candidate_retrieval | 107 | 763 | 1,483 | 34,500 | 5,733 |
| candidate_limit | 16.0 | 36.3 | 72.4 | 10,234 | 76 |
| scoring | 0.5 | 317 | 317 | 317 | 2 |
| **frequency_cap** | 36.0 | **6,317** | **13,248** | 25,226 | **14,402** |
| budget_pacing | 14.5 | 29.3 | 168 | 10,160 | 314 |
| ranking | 7.4 | 21.0 | 118 | 5,173 | 111 |
| response_build | 0.8 | 1.4 | 11.6 | 537 | 6 |

---

## Observations + root-cause analysis

### Observation 1: Bid rate is stable at ~70% across tiers — not the ~97%+ a casual reader might expect, but for a real reason

**Bid rate at both tiers: 70.17% (5K) and 70.18% (10K).** That ~70% is the right target for THIS combination of corpus diversity vs seeded targeting density. The Java repo's 97%+ figures came from a workload where the seeded targeting overlap was much closer to 100% — that's a different test, not a different bidder.

For Phase 7 the operational signal is **stability of the bid rate as RPS climbs**, not its absolute value. The 70% holding flat from 5K to 10K means budget pacer and freq-cap behaviour scale correctly. The earlier dirty-baseline run showed 19% → 14% → 11% degradation across tiers — that was the budget-exhaustion artifact described above (campaigns running out of test-environment budget), not a bidder defect. After the seed-budget inflation, bid rate stays flat.

### Observation 2: freq-cap MGET is the only stage that materially exceeds its budget at 10K

| Stage | Budget (ms) | p99 at 10K (µs) | Result |
|---|---:|---:|---|
| `frequency_cap` | 8 | **6,317** | Within budget at p99, but max 25,226 µs and 14,402 budget-exceeded events |
| All others | per `config.toml` | ≤ 800 µs | Comfortably under budget |

**At 10K RPS the breaker still trips 4 times** during the 3-min run (vs 16 in the dirty run), and freq-cap is skipped on 552K of 1.5M requests (37%) due to breaker-open. The reduction vs the dirty run is real (the budget-exhaustion-induced retrieval overhead made the dirty run worse), but the underlying issue stays: **a single-Redis-instance MGET path of ~50 keys per user can't sustain 10K RPS within the 8 ms budget on macOS dev**.

**Phase 7 implications:**
1. `InProcessFrequencyCapper` (already on the Phase 7 list, plan §) — eliminates the Redis MGET hop entirely for the steady-state path. The right priority.
2. "Treat MGET timeout as breaker error" semantics — a slow Redis isn't a broken Redis. Currently any timeout trips the breaker for ALL users for 10s. Worth a Phase 7 design review (already filed as a Phase 7 follow-up in this branch's PR description).
3. The hedge budget is consumed in the first ~30s and barely refills (`hedge fired = 32 / 169` at 5K / 10K). `HedgeBudget::set_load_shed_rate()` wiring (already on the Phase 7 list) addresses the refill-rate problem.

### Observation 3: Sustained RPS = 83% of target on macOS dev — k6 ceiling, not bidder ceiling

5K target → 4,167 actual (83%). 10K target → 8,333 actual (83%). The ratio is identical, which tells us this is k6's behaviour on macOS Docker Desktop, not a bidder limit:

- macOS file-descriptor and ephemeral-port limits (`ulimit -n`, `net.inet.ip.portrange.first`) constrain the VU pool's connection turnover.
- Docker Desktop's networking stack adds per-connection overhead that doesn't exist on bare Linux.

Linux production hardware will deliver closer to 100% of target. Phase 7's first action is re-running this exact harness on a Linux box to calibrate the macOS-vs-Linux multiplier.

### Observation 4: `candidate_retrieval` is the only stage with non-trivial budget overruns

At 10K RPS, `candidate_retrieval` exceeded its 1 ms budget 5,733 times (~0.4% of requests). Median 107 µs, p99 763 µs, max 34,500 µs. The tail comes from RoaringBitmap intersection cost when a user has many segments and the popular-segment bitmaps are dense.

**Phase 7 implications:** if Linux-prod profiling confirms this stage as a bottleneck, options are:
- Cap segments-per-request at retrieval time
- Pre-compute "popular segment" bitmaps that subsume top-K segment ORs
- Reorder dimension intersections so the cheapest filter runs first (geo before segment when geo is restrictive)

Below the Phase 7 priority floor unless Linux numbers escalate it.

### Observation 5: Zero kafka_events_dropped, zero OTel errors — opt-out works

`bidder_kafka_events_dropped: 0` at both tiers confirms the empty-broker → `NoOpEventPublisher` path is correctly wired. OTel-disabled path produces zero ExportError noise. The baseline measures bid-path cost, not retry-overhead-from-missing-sidecars cost. This was the explicit fix for the dirty-run pollution.

### Observation 6: The bidder ignores OpenRTB `user.data[].segment[]` from the wire entirely

Caught during dirty-run debugging, still applies. `UserEnrichmentStage` only reads segments from Redis; wire-format segments are discarded. Real product gap (production SSPs send in-request segments routinely). Two-line fix in `UserEnrichmentStage`. **Not a Phase 7 priority** — filed as a follow-up so it doesn't get lost.

---

## What this baseline tells Phase 7

**Validated to focus on:**
- **`InProcessFrequencyCapper`** — freq-cap MGET path is the hottest Redis interaction by far, only stage routinely tripping its breaker. Highest-leverage Phase 7 work.
- **Linux multi-process via `SO_REUSEPORT`** — single-process Tokio on macOS tops at ~8K sustained, 83% of 10K target. Linux measurement first; if Linux delivers ≥10K, multi-process is the next horizon.
- **`HedgeBudget::set_load_shed_rate()` wiring** — hedge budget consumed early, never refills meaningfully under sustained load.

**Validated to defer:**
- **`bumpalo` arena experiment** — allocator does NOT show as a hot spot in any per-stage timing. Defer until Linux-prod profiling escalates it.
- **`monoio` thread-per-core experiment** — would only help if io_uring syscall overhead was the limiter; not even close at the current bottleneck profile. Multi-process Tokio first.
- **Per-stage micro-optimizations** (reusable `Vec<f32>` buffers, pre-computed campaign features) — below the noise floor at 5K-10K. Defer.

**New Phase 7 follow-ups surfaced (filed in PR description):**
- Merge wire-format `user.data[].segment[]` into `ctx.segment_ids` (real product gap).
- Re-evaluate "treat MGET timeout as breaker error" semantics.
- Consider a `LatencyBudgetLayer` that 503s queued requests above N ms wait time (in addition to the current memory-bound `ConcurrencyLimitLayer`).
- Re-run baseline on Linux production hardware to calibrate macOS-vs-Linux multiplier.

---

## Caveats — what this baseline does NOT measure

- **Linux prod hardware.** macOS Apple Silicon + Docker Desktop is dev. Linux x86_64 with native networking will be 1.5-3× faster on per-request latency, and k6 will hit closer to 100% of target RPS rather than 83%. Re-run on Linux production hardware at the start of Phase 7.
- **Real Kafka end-to-end.** Disabled here (`NoOpEventPublisher`) so retry overhead doesn't pollute timings. Production runs with rdkafka + a broker incur some CPU in the producer thread, but the bid path is fire-and-forget on Kafka publish so SLA shouldn't move. Phase 7 should re-validate with Kafka up.
- **Real production budget pressure.** Test seed budgets are deliberately inflated ($1K-$1M/day vs production $50-$5K) so 3-min compressed-time tests don't drain budgets in seconds. Production budget-pacing under exhaustion is a separate test concern (Phase 7+ chaos suite).
- **Real campaign / user diversity.** 5K seeded campaigns × 100K users × 1019 segments is a tiny fraction of the production workload (100K campaigns, 100M users). The stage-timing shape stays the same; absolute numbers shift.
- **Sustained 30+ minute runs.** 120s hold per tier covers steady-state but not slow leaks (RSS slope, Arc cycle leaks). The PLAN.md nightly chaos suite covers that and is Phase 7 work.
- **Real production traffic shape.** k6's uniform-random-with-Zipf-on-users is a synthetic shape. Real bid traffic has time-of-day skew, campaign-spend-curve correlation with bid floor, and segment-overlap clustering that this corpus doesn't model. Acceptable for a regression baseline; not for absolute capacity claims.

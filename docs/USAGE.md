# Usage

How to build, run, and load-test this bidder. All commands are `make` targets — see `make help` for the full list.

## Prerequisites

- Rust (stable), Docker, `python3`, `k6`, `sqlx-cli`
- macOS Apple Silicon or Linux (x86_64 / aarch64)

## 1. One-time setup

```bash
make install-ort        # vendor ONNX Runtime native lib
```

## 2. Bring up the infrastructure and seed data

```bash
make infra-up           # postgres + redis (waits for healthy)
make migrate            # apply schema
make seed               # campaigns + segments + users
```

`make seed` is idempotent and chains `infra-up + migrate + seed-postgres + seed-redis`. Catalog size knobs: `CAMPAIGNS=20000 SEGMENTS=5000 USERS=500000 make seed`.

## 3. Run the bidder

Two ways to run, pick what fits the workflow:

**Default (foreground, ctrl-c to stop, logs streaming live):**

```bash
make bidder-start       # cargo run --release, blocks the terminal
```

This is what you want when poking at the bidder interactively. Override env vars on the same line:

```bash
BIDDER__FREQ_CAP__IN_PROCESS_ENABLED=true make bidder-start
BIDDER__TELEMETRY__LOG_FORMAT=pretty   make bidder-start
```

**Background mode (`BG=1`):**

```bash
make bidder-start BG=1  # nohup, PID file under load-test/results/, terminal stays free
make health             # probe /health/ready
make bidder-stop        # graceful SIGTERM, SIGKILL after 10s
make bidder-restart     # stop + start (background; used by stress automation)
```

Stress targets (`make stress-Xk`, `stress-all`) call `bidder-restart` internally, which uses background mode automatically — you don't need to think about `BG=1` for stress runs.

## 4. Load tests

### Baseline (nominal + stretch reference numbers)

```bash
make baseline           # 5K RPS, 3 min
make baseline-tiered    # 5K then 10K with cooldown
```

### Stress tiers (find-the-ceiling sweep)

Each tier is `30s warmup + 30s ramp + 120s hold + 30s ramp-down ≈ 3.5 min`.

**Every stress-Xk target restarts the bidder first** — guarantees no carryover between tiers (freq-cap counters, breaker memory, hedge budget, fred pool growth, jemalloc warmth all reset). The k6 warmup phase runs against the freshly-restarted process at the target RPS, then the measure phase grades steady-state. Threshold gates apply only to the measure phase.

```bash
make stress-5k
make stress-10k
make stress-15k
make stress-20k
make stress-25k
make stress-30k
make stress-40k
make stress-50k
```

Or the whole sweep in one shot (≈30 min, with cooldowns between tiers):

```bash
make stress-all
```

Override timing per run: `make stress-10k HOLD_S=300`.

### Render a Markdown summary

```bash
make analyze                      # baseline numbers (default)
make analyze PREFIX=stress        # stress-tier numbers
```

Output lands at `load-test/results/<prefix>-summary.md` — four sections covering HTTP timing, pipeline outcomes, resilience signals, per-stage timing.

## 5. Reset between experiments

```bash
make stress-clean       # remove stress artifacts only
make baseline-clean     # remove baseline artifacts only
make infra-reset        # drop postgres + redis volumes (re-seed required)
```

## 6. Tear down

```bash
make infra-down         # stop containers, keep volumes
make infra-reset        # stop + drop volumes
```

## End-to-end first run

From a fresh checkout, single shell — `stress-Xk` manages the bidder for you:

```bash
make install-ort
make infra-up && make migrate seed
make stress-10k                      # restarts bidder + warmup + measure
make analyze PREFIX=stress
cat load-test/results/stress-summary.md
```

If you want the bidder running for ad-hoc smoke testing instead of a stress run:

```bash
# Shell A: foreground (default), ctrl-c to stop, logs visible in this terminal
make bidder-start

# Shell B
make health             # is /health/ready returning 200?
make bid                # fire one real bid request through the pipeline
```

`make bid` POSTs `tests/fixtures/golden-bid-request.json` to `/rtb/openrtb/bid` and prints the response body + status + timing. Useful sanity check after restarting the bidder, before kicking off a stress tier. Override `BID_FIXTURE=path/to/other.json` to send a different payload.

## Where things live

- `docs/PLAN.md` — architecture and phase plan
- `docs/notes/` — per-phase implementation notes and ADRs
- `docs/notes/` — per-phase development logs and ADRs (including historical load-test logs)
- `load-test/results/` — local run artifacts (gitignored)
- `k6/golden.js` — canonical load-test script (parametrised by `TARGET_RPS`, `WARMUP_S`, `HOLD_S`, `RAMP_UP_S`, `RAMP_DOWN_S`)
- `tools/analyze-baseline.sh` — analyzer (env: `PREFIX=<name>` to switch corpus)

## Common overrides

| Var | Default | Notes |
|---|---|---|
| `CAMPAIGNS` | `5000` | seed-postgres |
| `SEGMENTS` | `1000` | seed-postgres + seed-redis |
| `USERS` | `100000` | seed-redis |
| `WARMUP_S` | `30` | k6 warmup-phase seconds per tier (excluded from thresholds) |
| `HOLD_S` | `120` | k6 measure-phase steady-state seconds per tier |
| `RAMP_UP_S` / `RAMP_DOWN_S` | `30` | k6 ramp seconds |
| `TIER_COOLDOWN` | `30` | seconds between tiers in `stress-all` |
| `READY_TIMEOUT_S` | `60` | seconds `bidder-start` waits for `/health/ready` |
| `FD_LIMIT` | `65536` | per-process file-descriptor limit applied before launching bidder + k6. macOS defaults to 256 which fails at any meaningful RPS — see "Too many open files" under troubleshooting. |
| `RESULTS_DIR` | `load-test/results` | output directory |
| `METRICS_URL` | `http://localhost:9090/metrics` | bidder Prometheus endpoint |
| `HEALTH_URL` | `http://localhost:8080/health/ready` | bidder readiness probe |
| `BID_URL` | `http://localhost:8080/rtb/openrtb/bid` | bid endpoint for `make bid` |
| `BID_FIXTURE` | `tests/fixtures/golden-bid-request.json` | sample request body for `make bid` |
| `DATABASE_URL` | `postgres://bidder:bidder@localhost:5432/bidder` | sqlx + seed-postgres |

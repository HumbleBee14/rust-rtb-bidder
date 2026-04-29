# Usage

How to build, run, and load-test this bidder. All commands are `make` targets — see `make help` for the full list.

## Prerequisites

- Rust (stable), Docker, `python3`, `k6`, `sqlx-cli`
- macOS Apple Silicon or Linux (x86_64 / aarch64)

## 1. One-time setup

```bash
make install-ort        # vendor ONNX Runtime native lib
```

## 2. Bring up the stack and seed data

```bash
make stack-up           # postgres + redis (waits for healthy)
make migrate            # apply schema
make seed               # campaigns + segments + users
```

`make seed` is idempotent and chains `stack-up + migrate + seed-postgres + seed-redis`. Catalog size knobs: `CAMPAIGNS=20000 SEGMENTS=5000 USERS=500000 make seed`.

## 3. Run the bidder

In its own shell — leave it running while you load-test:

```bash
make baseline-bidder    # release build, Kafka + OTLP disabled
```

Wait for the `bidder ready` log line before driving load.

## 4. Load tests

### Baseline (nominal + stretch reference numbers)

```bash
make baseline           # 5K RPS, 3 min
make baseline-tiered    # 5K then 10K with cooldown
```

### Stress tiers (find-the-ceiling sweep)

Each tier is `30s ramp + 120s hold + 30s ramp-down ≈ 3 min`.

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
make stack-reset        # drop postgres + redis volumes (re-seed required)
```

## 6. Tear down

```bash
make stack-down         # stop containers, keep volumes
make stack-reset        # stop + drop volumes
```

## End-to-end first run

From a fresh checkout, in two shells:

```bash
# Shell A
make install-ort
make stack-up && make migrate seed && make baseline-bidder

# Shell B (after the bidder logs "ready")
make stress-10k
make analyze PREFIX=stress
cat load-test/results/stress-summary.md
```

## Where things live

- `docs/PLAN.md` — architecture and phase plan
- `docs/notes/` — per-phase implementation notes and ADRs
- `docs/LOAD-TEST-RESULTS-*.md` — committed reference numbers
- `load-test/results/` — local run artifacts (gitignored)
- `k6/golden.js` — canonical load-test script (parametrised by `TARGET_RPS`, `HOLD_S`, `RAMP_UP_S`, `RAMP_DOWN_S`)
- `tools/analyze-baseline.sh` — analyzer (env: `PREFIX=<name>` to switch corpus)

## Common overrides

| Var | Default | Notes |
|---|---|---|
| `CAMPAIGNS` | `5000` | seed-postgres |
| `SEGMENTS` | `1000` | seed-postgres + seed-redis |
| `USERS` | `100000` | seed-redis |
| `HOLD_S` | `120` | k6 steady-state seconds per tier |
| `RAMP_UP_S` / `RAMP_DOWN_S` | `30` | k6 ramp seconds |
| `TIER_COOLDOWN` | `30` | seconds between tiers in `stress-all` |
| `RESULTS_DIR` | `load-test/results` | output directory |
| `METRICS_URL` | `http://localhost:9090/metrics` | bidder Prometheus endpoint |
| `DATABASE_URL` | `postgres://bidder:bidder@localhost:5432/bidder` | sqlx + seed-postgres |

.PHONY: regen-test-model install-ort dev-env \
        stack-up stack-down stack-reset stack-status \
        seed seed-postgres seed-redis migrate \
        baseline baseline-tiered baseline-clean baseline-bidder

# ─── Build / one-time setup ─────────────────────────────────────────────────

# Regenerate the synthetic ONNX test fixture and its parity JSONL. The output
# files are committed to the repo; this target is for when the generator's
# constants change, not part of the build.
regen-test-model:
	cargo run -p gen-test-model -- tests/fixtures/test_pctr_model.onnx

# Vendor ONNX Runtime native lib into ./vendor/onnxruntime/<platform>/.
# Run once per fresh checkout. Idempotent.
install-ort:
	bash tools/install-onnxruntime.sh

# Print the ORT_DYLIB_PATH export for the current platform. Eval to apply:
#   eval $$(make dev-env)
dev-env:
	@bash tools/setup-ort-env.sh

# ─── Local infrastructure stack (docker compose) ────────────────────────────

# Start postgres + redis. Healthchecks gate readiness; this target waits for
# them to be healthy before returning so downstream targets (migrate, seed) can
# run immediately.
stack-up:
	docker compose up -d --wait

# Stop and remove containers. Volumes are kept (use stack-reset to also drop).
stack-down:
	docker compose down

# Full reset: containers + volumes. Use after schema changes that need a clean
# DB, or to reclaim disk after long-running test sessions.
stack-reset:
	docker compose down -v

stack-status:
	docker compose ps

# ─── Seeding ────────────────────────────────────────────────────────────────

# Run sqlx migrations against the dev postgres instance. Requires sqlx-cli:
#   cargo install sqlx-cli --no-default-features --features rustls,postgres
migrate:
	DATABASE_URL=postgres://bidder:bidder@localhost:5432/bidder sqlx migrate run

# Seed Postgres with campaign + segment data. Defaults: 5K campaigns, 1K
# segments. Override via env: CAMPAIGNS=20000 SEGMENTS=5000 make seed-postgres.
CAMPAIGNS ?= 5000
SEGMENTS  ?= 1000
seed-postgres:
	DATABASE_URL=postgres://bidder:bidder@localhost:5432/bidder \
	  python3 docker/seed-postgres.py --campaigns $(CAMPAIGNS) --segments $(SEGMENTS)

# Seed Redis with v1:seg:{u:<id>} keys. Defaults: 100K users matching the
# 1K segment ID space from seed-postgres. Override via USERS=N make seed-redis.
USERS ?= 100000
seed-redis:
	python3 docker/seed-redis.py --users $(USERS) --segments $(SEGMENTS) | \
	  docker exec -i bidder-redis redis-cli --pipe

# Convenience: bring stack up, run migrations, seed both stores.
seed: stack-up migrate seed-postgres seed-redis

# ─── Baseline load tests ────────────────────────────────────────────────────

# Single-tier baseline: 5K RPS for 3 minutes (30s ramp + 120s hold + 30s ramp-down).
# Bidder must already be running on :8080. Result + Prometheus metrics snapshot
# saved under load-test/results/.
#
# Captures both:
#   - k6 timing summary (HTTP latency: p50, p95, p99, p99.9)
#   - bidder Prometheus snapshot (per-stage timings, breaker events, hedge
#     fired/blocked, kafka events_dropped) at end of run
#
# IMPORTANT: launch the bidder with Kafka and OTLP disabled so the baseline
# numbers don't include connection-retry overhead from missing services:
#
#   BIDDER__KAFKA__BROKERS="" BIDDER__TELEMETRY__OTLP_ENDPOINT="" \
#     ./target/release/bidder-server --config config.toml
#
# (or use `make baseline-bidder` below).
baseline: load-test/results
	HOLD_S=120 RAMP_UP_S=30 RAMP_DOWN_S=30 TARGET_RPS=5000 \
	  k6 run --summary-export=load-test/results/v0-baseline-5000rps-summary.json \
	         k6/golden.js
	curl -fsS http://localhost:9090/metrics > load-test/results/v0-baseline-5000rps-prometheus.txt
	bash tools/analyze-baseline.sh > load-test/results/v0-baseline-summary.md
	@echo "summary: load-test/results/v0-baseline-summary.md"

# Convenience: start the bidder configured for clean baseline measurements.
# Foreground; ctrl-c to stop. For background runs use the explicit env-var
# launch in the comment above + `&`.
baseline-bidder:
	. tools/setup-ort-env.sh && \
	BIDDER__KAFKA__BROKERS="" BIDDER__TELEMETRY__OTLP_ENDPOINT="" \
	  cargo run --release --bin bidder-server -- --config config.toml

# Tiered baseline: two sequential runs at 5K and 10K RPS. Each uses a 30s
# inter-tier cooldown so circuit breakers / cap counters drain. Saves a
# per-tier summary + Prometheus snapshot.
#
# Captures a "before" Prometheus snapshot just before each tier starts, in
# addition to the "after" snapshot at the end. The analyzer subtracts before
# from after to produce per-tier deltas, since the bidder's counters are
# cumulative across the process lifetime — without the diff, tier N's totals
# would include all of tier 1..N-1's traffic.
#
# 15K+ is intentionally NOT a tier here: the v0 baseline target is "nominal
# + stretch reference numbers", not "find the ceiling". Stress tiers (15K,
# 20K, 25K, 50K) live in a dedicated stress-tier phase so the v0 baseline
# stays small + reproducible.
#
# Same Kafka/OTLP-disabled prerequisite as `make baseline` — launch the bidder
# via `make baseline-bidder`.
baseline-tiered: load-test/results
	@for tier in 5000 10000; do \
	  echo "=== tier $${tier} RPS ===" ; \
	  curl -fsS http://localhost:9090/metrics > load-test/results/v0-baseline-$${tier}rps-prometheus-before.txt ; \
	  HOLD_S=120 RAMP_UP_S=30 RAMP_DOWN_S=30 TARGET_RPS=$${tier} \
	    k6 run --summary-export=load-test/results/v0-baseline-$${tier}rps-summary.json \
	           k6/golden.js ; \
	  curl -fsS http://localhost:9090/metrics > load-test/results/v0-baseline-$${tier}rps-prometheus.txt ; \
	  echo "cooling down 30s before next tier..." ; \
	  sleep 30 ; \
	done
	bash tools/analyze-baseline.sh > load-test/results/v0-baseline-summary.md
	@echo "summary: load-test/results/v0-baseline-summary.md"

baseline-clean:
	rm -rf load-test/results/v0-baseline-*

load-test/results:
	mkdir -p $@

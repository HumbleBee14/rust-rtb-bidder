.DEFAULT_GOAL := help

# Overridable: `make seed-postgres CAMPAIGNS=20000`, `make stress-10k HOLD_S=300`.
CAMPAIGNS  ?= 5000
SEGMENTS   ?= 1000
USERS      ?= 100000

STRESS_TIERS  := 5000 10000 15000 20000 25000 30000 40000 50000
WARMUP_S      ?= 30
HOLD_S        ?= 120
RAMP_UP_S     ?= 30
RAMP_DOWN_S   ?= 30
TIER_COOLDOWN ?= 30

FD_LIMIT        ?= 65536
RESULTS_DIR     ?= load-test/results
BASELINE_PREFIX := v0-baseline
STRESS_PREFIX   := stress
PREFIX          ?= $(BASELINE_PREFIX)

DATABASE_URL ?= postgres://bidder:bidder@localhost:5432/bidder
METRICS_URL  ?= http://localhost:9090/metrics
HEALTH_URL   ?= http://localhost:8080/health/ready
BID_URL      ?= http://localhost:8080/rtb/openrtb/bid
BID_FIXTURE  ?= tests/fixtures/golden-bid-request.json
BIDDER_BIN   ?= target/release/bidder-server
BIDDER_PID   ?= $(RESULTS_DIR)/.bidder.pid
BIDDER_LOG   ?= $(RESULTS_DIR)/bidder.log
READY_TIMEOUT_S ?= 60

.PHONY: help install-ort \
        infra-up infra-down infra-reset infra-status \
        migrate seed seed-postgres seed-redis \
        bidder-start bidder-stop bidder-restart health bid \
        baseline baseline-tiered baseline-clean \
        stress-all stress-clean analyze

help:
	@awk 'BEGIN {FS = ":.*##"; printf "Targets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ { printf "  %-22s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo "  stress-{$(shell echo $(STRESS_TIERS) | tr ' ' ',' | sed 's/000//g')}k       Run one stress tier at the named RPS"
	@echo
	@echo "Overrides: CAMPAIGNS, SEGMENTS, USERS, WARMUP_S, HOLD_S, RAMP_UP_S, RAMP_DOWN_S, TIER_COOLDOWN"

install-ort: ## Vendor ONNX Runtime native lib (run once per checkout)
	bash tools/install-onnxruntime.sh

# ── infra ───────────────────────────────────────────────────────────────────
infra-up: ## Start postgres + redis and wait for healthy
	docker compose up -d --wait

infra-down: ## Stop containers (volumes kept)
	docker compose down

infra-reset: ## Stop + drop volumes (clean slate)
	docker compose down -v

infra-status: ## docker compose ps
	docker compose ps

# ── data ────────────────────────────────────────────────────────────────────
migrate: ## Run sqlx migrations
	DATABASE_URL=$(DATABASE_URL) sqlx migrate run

seed-postgres: ## Seed campaigns + segments
	DATABASE_URL=$(DATABASE_URL) \
	  python3 docker/seed-postgres.py --campaigns $(CAMPAIGNS) --segments $(SEGMENTS)

seed-redis: ## Seed user-segment keys
	python3 docker/seed-redis.py --users $(USERS) --segments $(SEGMENTS) | \
	  docker exec -i bidder-redis redis-cli --pipe

seed: infra-up migrate seed-postgres seed-redis ## infra-up + migrate + seed-postgres + seed-redis

# ── bidder runtime ──────────────────────────────────────────────────────────
#
# Default = foreground. `cargo run` blocks the terminal, logs stream live,
# ctrl-c stops it. This is what humans want when poking at the bidder.
#
# `make bidder-start BG=1` runs in the background instead: writes a PID file
# under RESULTS_DIR, redirects logs to a file, returns control to the shell.
# Used by stress automation (`bidder-restart` passes BG=1 internally).
#
# In BG mode `bidder-stop` and `bidder-restart` work via the PID file. In FG
# mode just ctrl-c — there is no PID file to manage.
BG ?= 0

bidder-start: $(RESULTS_DIR) ## Start bidder. Default foreground (ctrl-c to stop). BG=1 for background.
ifeq ($(BG),1)
	@$(MAKE) --no-print-directory bidder-stop
	@echo "building release binary..."
	@cargo build --release --bin bidder-server 1>&2
	@echo "starting bidder in background (log: $(BIDDER_LOG), fd limit: $(FD_LIMIT))..."
	@ulimit -n $(FD_LIMIT) && \
	. tools/setup-ort-env.sh && \
	  BIDDER__KAFKA__BROKERS="" BIDDER__TELEMETRY__OTLP_ENDPOINT="" \
	    nohup $(BIDDER_BIN) --config config.toml > $(BIDDER_LOG) 2>&1 & \
	  echo $$! > $(BIDDER_PID)
	@echo "waiting for /health/ready (timeout $(READY_TIMEOUT_S)s)..."
	@for i in $$(seq 1 $(READY_TIMEOUT_S)); do \
	  if curl -fsS $(HEALTH_URL) >/dev/null 2>&1; then \
	    echo "bidder ready (pid $$(cat $(BIDDER_PID)))" ; exit 0 ; \
	  fi ; \
	  sleep 1 ; \
	done ; \
	echo "bidder did not become ready within $(READY_TIMEOUT_S)s — see $(BIDDER_LOG)" ; \
	exit 1
else
	@$(MAKE) --no-print-directory bidder-stop
	@ulimit -n $(FD_LIMIT) && \
	. tools/setup-ort-env.sh && \
	BIDDER__KAFKA__BROKERS="" BIDDER__TELEMETRY__OTLP_ENDPOINT="" \
	  cargo run --release --bin bidder-server -- --config config.toml
endif

bidder-stop: ## Stop any bidder process (PID file or anything bound to our ports)
	@# Kill the PID file's process first if present
	@if [ -f $(BIDDER_PID) ]; then \
	  pid=$$(cat $(BIDDER_PID)) ; \
	  if kill -0 $$pid 2>/dev/null ; then \
	    echo "stopping bidder (pid $$pid from PID file)..." ; \
	    kill -TERM $$pid 2>/dev/null || true ; \
	    for i in 1 2 3 4 5 6 7 8 9 10 ; do \
	      kill -0 $$pid 2>/dev/null || break ; sleep 1 ; \
	    done ; \
	    kill -0 $$pid 2>/dev/null && kill -KILL $$pid 2>/dev/null || true ; \
	  fi ; \
	  rm -f $(BIDDER_PID) ; \
	fi
	@# Also catch anything bound to our ports (FG bidder, stale PID-file mismatch,
	@# or bidder started outside this Makefile). Kill by port not by name so we
	@# don't accidentally kill an unrelated `bidder-server` build artifact.
	@for port in 8080 9090; do \
	  pids=$$(lsof -ti :$$port 2>/dev/null || true) ; \
	  for pid in $$pids ; do \
	    if kill -0 $$pid 2>/dev/null ; then \
	      echo "stopping bidder (pid $$pid on port $$port)..." ; \
	      kill -TERM $$pid 2>/dev/null || true ; \
	      for i in 1 2 3 4 5 ; do \
	        kill -0 $$pid 2>/dev/null || break ; sleep 1 ; \
	      done ; \
	      kill -0 $$pid 2>/dev/null && kill -KILL $$pid 2>/dev/null || true ; \
	    fi ; \
	  done ; \
	done

bidder-restart: ## Stop + (re)start in background; used by stress automation
	@$(MAKE) --no-print-directory bidder-stop
	@$(MAKE) --no-print-directory bidder-start BG=1

health: ## Probe /health/ready and print response
	@curl -sS -w "\nHTTP %{http_code} in %{time_total}s\n" $(HEALTH_URL) || \
	  { echo "bidder is not reachable at $(HEALTH_URL)" ; exit 1 ; }

bid: ## POST one bid request from BID_FIXTURE; pretty-prints JSON via jq if available
	@response=$$(curl -sS -X POST $(BID_URL) \
	  -H 'Content-Type: application/json' \
	  -H 'x-openrtb-version: 2.6' \
	  --data @$(BID_FIXTURE) \
	  -w "\n__HTTP_STATUS__%{http_code} in %{time_total}s (size %{size_download}B)") || \
	  { echo "bid request failed at $(BID_URL)" ; exit 1 ; } ; \
	body=$$(echo "$$response" | sed -n '1p') ; \
	status_line=$$(echo "$$response" | grep "^__HTTP_STATUS__" | sed 's/^__HTTP_STATUS__/HTTP /') ; \
	if command -v jq >/dev/null 2>&1 && [ -n "$$body" ]; then \
	  echo "$$body" | jq . ; \
	else \
	  echo "$$body" ; \
	fi ; \
	echo "$$status_line"

# ── load tests ──────────────────────────────────────────────────────────────
#
# `run_tier` runs one tier end-to-end. The bidder must already be running:
#   1. Verify /health/ready is 200 — fail loudly if not.
#   2. When `RESTART=1`, cycle the bidder for clean state (used by stress-all).
#   3. Capture before-snapshot of /metrics.
#   4. Run k6.
#   5. Capture after-snapshot of /metrics.
#
# We do NOT auto-launch a missing bidder. The operator owns the bidder
# lifecycle — `make bidder-start` first, then run stress.
RESTART ?= 0

define run_tier
	@echo "=== $(1) tier $(2) RPS ==="
	@if ! curl -fsS $(HEALTH_URL) >/dev/null 2>&1; then \
	  echo "" ; \
	  echo "ERROR: bidder is not running (no response from $(HEALTH_URL))." ; \
	  echo "       Start it first:" ; \
	  echo "         make bidder-start          # foreground, logs in terminal" ; \
	  echo "         make bidder-start BG=1     # background daemon" ; \
	  exit 1 ; \
	fi
	@if [ "$(RESTART)" = "1" ]; then \
	  echo "RESTART=1: cycling bidder for fresh state..." ; \
	  $(MAKE) --no-print-directory bidder-restart ; \
	else \
	  echo "reusing running bidder (set RESTART=1 to force a fresh process)" ; \
	fi
	curl -fsS $(METRICS_URL) > $(RESULTS_DIR)/$(1)-$(2)rps-prometheus-before.txt
	@ulimit -n $(FD_LIMIT) && \
	HOLD_S=$(HOLD_S) RAMP_UP_S=$(RAMP_UP_S) RAMP_DOWN_S=$(RAMP_DOWN_S) WARMUP_S=$(WARMUP_S) TARGET_RPS=$(2) \
	  SUMMARY_PATH=$(RESULTS_DIR)/$(1)-$(2)rps-summary.json \
	    k6 run k6/golden.js
	curl -fsS $(METRICS_URL) > $(RESULTS_DIR)/$(1)-$(2)rps-prometheus.txt
endef

baseline: $(RESULTS_DIR) ## Single-tier baseline at 5K RPS
	$(call run_tier,$(BASELINE_PREFIX),5000)
	@$(MAKE) --no-print-directory analyze PREFIX=$(BASELINE_PREFIX)

baseline-tiered: $(RESULTS_DIR) ## Baseline at 5K then 10K with cooldown
	$(call run_tier,$(BASELINE_PREFIX),5000)
	@sleep $(TIER_COOLDOWN)
	$(call run_tier,$(BASELINE_PREFIX),10000)
	@$(MAKE) --no-print-directory analyze PREFIX=$(BASELINE_PREFIX)

baseline-clean: ## Remove baseline artifacts
	rm -rf $(RESULTS_DIR)/$(BASELINE_PREFIX)-*

# Generate stress-5k, stress-10k, … from STRESS_TIERS.
define stress_tier_template
.PHONY: stress-$(1)k
stress-$(1)k: $(RESULTS_DIR)
	$$(call run_tier,$(STRESS_PREFIX),$(2))
endef
$(foreach tier,$(STRESS_TIERS),\
  $(eval $(call stress_tier_template,$(shell echo $$(($(tier)/1000))),$(tier))))

stress-all: $(RESULTS_DIR) ## Run every tier in STRESS_TIERS with cooldowns + per-tier restart
	@# RESTART=1 forces a full bidder restart between tiers so each tier starts
	@# from a clean process (no carryover of freq-cap counters, breaker state,
	@# jemalloc warmth, etc.). Required for fair tier-to-tier comparison.
	@for tier in $(STRESS_TIERS); do \
	  $(MAKE) --no-print-directory stress-$$((tier/1000))k RESTART=1 ; \
	  echo "cooling down $(TIER_COOLDOWN)s..." ; \
	  sleep $(TIER_COOLDOWN) ; \
	done
	@$(MAKE) --no-print-directory analyze PREFIX=$(STRESS_PREFIX)

stress-clean: ## Remove stress artifacts
	rm -rf $(RESULTS_DIR)/$(STRESS_PREFIX)-*

analyze: ## Render Markdown summary (override PREFIX=stress)
	PREFIX=$(PREFIX) bash tools/analyze-baseline.sh $(RESULTS_DIR) \
	  > $(RESULTS_DIR)/$(PREFIX)-summary.md
	@echo "summary: $(RESULTS_DIR)/$(PREFIX)-summary.md"

$(RESULTS_DIR):
	@mkdir -p $@

.DEFAULT_GOAL := help

# Overridable: `make seed-postgres CAMPAIGNS=20000`, `make stress-10k HOLD_S=300`.
CAMPAIGNS  ?= 5000
SEGMENTS   ?= 1000
USERS      ?= 100000

STRESS_TIERS  := 5000 10000 15000 20000 25000 30000 40000 50000
HOLD_S        ?= 120
RAMP_UP_S     ?= 30
RAMP_DOWN_S   ?= 30
TIER_COOLDOWN ?= 30

RESULTS_DIR     ?= load-test/results
BASELINE_PREFIX := v0-baseline
STRESS_PREFIX   := stress
PREFIX          ?= $(BASELINE_PREFIX)

DATABASE_URL ?= postgres://bidder:bidder@localhost:5432/bidder
METRICS_URL  ?= http://localhost:9090/metrics

.PHONY: help install-ort \
        stack-up stack-down stack-reset stack-status \
        migrate seed seed-postgres seed-redis \
        baseline-bidder baseline baseline-tiered baseline-clean \
        stress-all stress-clean analyze

help:
	@awk 'BEGIN {FS = ":.*##"; printf "Targets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ { printf "  %-22s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo "  stress-{$(shell echo $(STRESS_TIERS) | tr ' ' ',' | sed 's/000//g')}k       Run one stress tier at the named RPS"
	@echo
	@echo "Overrides: CAMPAIGNS, SEGMENTS, USERS, HOLD_S, RAMP_UP_S, RAMP_DOWN_S, TIER_COOLDOWN"

install-ort: ## Vendor ONNX Runtime native lib (run once per checkout)
	bash tools/install-onnxruntime.sh

# ── stack ───────────────────────────────────────────────────────────────────
stack-up: ## Start postgres + redis and wait for healthy
	docker compose up -d --wait

stack-down: ## Stop containers (volumes kept)
	docker compose down

stack-reset: ## Stop + drop volumes (clean slate)
	docker compose down -v

stack-status: ## docker compose ps
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

seed: stack-up migrate seed-postgres seed-redis ## stack-up + migrate + seed-postgres + seed-redis

# ── bidder runtime ──────────────────────────────────────────────────────────
baseline-bidder: ## Start bidder with Kafka/OTLP disabled (foreground)
	. tools/setup-ort-env.sh && \
	BIDDER__KAFKA__BROKERS="" BIDDER__TELEMETRY__OTLP_ENDPOINT="" \
	  cargo run --release --bin bidder-server -- --config config.toml

# ── load tests ──────────────────────────────────────────────────────────────
# $(call run_tier,PREFIX,RPS): one tier with before/after Prometheus snapshots
# so the analyzer can subtract cumulative counters into per-tier deltas.
define run_tier
	@echo "=== $(1) tier $(2) RPS ==="
	curl -fsS $(METRICS_URL) > $(RESULTS_DIR)/$(1)-$(2)rps-prometheus-before.txt
	HOLD_S=$(HOLD_S) RAMP_UP_S=$(RAMP_UP_S) RAMP_DOWN_S=$(RAMP_DOWN_S) TARGET_RPS=$(2) \
	  k6 run --summary-export=$(RESULTS_DIR)/$(1)-$(2)rps-summary.json k6/golden.js
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

stress-all: $(RESULTS_DIR) ## Run every tier in STRESS_TIERS with cooldowns
	@for tier in $(STRESS_TIERS); do \
	  $(MAKE) --no-print-directory stress-$$((tier/1000))k ; \
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

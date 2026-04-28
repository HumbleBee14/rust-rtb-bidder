#!/usr/bin/env bash
# Summarise a load-test/results/ directory of v0-baseline-*.json + *.txt files
# into a single Markdown table. Run after `make baseline` or `make baseline-tiered`.
#
# Usage:  bash tools/analyze-baseline.sh [results-dir]
#         (defaults to load-test/results/)
#
# Output goes to stdout — pipe to a file or paste into the LOAD-TEST-RESULTS doc.

set -euo pipefail

RESULTS_DIR="${1:-load-test/results}"

if [ ! -d "$RESULTS_DIR" ]; then
    echo "Results directory not found: $RESULTS_DIR" 1>&2
    exit 1
fi

# Find tiers from filenames: v0-baseline-<RPS>rps-summary.json
tiers=()
for f in "$RESULTS_DIR"/v0-baseline-*rps-summary.json; do
    [ -f "$f" ] || continue
    base=$(basename "$f" -summary.json)
    rps="${base#v0-baseline-}"
    rps="${rps%rps}"
    tiers+=("$rps")
done

if [ ${#tiers[@]} -eq 0 ]; then
    echo "No v0-baseline-*rps-summary.json files in $RESULTS_DIR" 1>&2
    exit 1
fi

# Sort numerically.
IFS=$'\n' sorted=($(printf '%s\n' "${tiers[@]}" | sort -n))
unset IFS

cat <<'EOF'
## k6 HTTP-side timing (per tier)

| Target RPS | Sustained RPS | iterations | p50 (ms) | p95 (ms) | p99 (ms) | p99.9 (ms) | max (ms) | http_req_failed |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
EOF

for tier in "${sorted[@]}"; do
    f="$RESULTS_DIR/v0-baseline-${tier}rps-summary.json"
    python3 - "$f" "$tier" <<'PY'
import json, sys
path, tier = sys.argv[1], sys.argv[2]
d = json.load(open(path))
m = d['metrics']
http = m.get('http_req_waiting', m.get('http_req_duration', {}))
iters = m.get('iterations', {})
err = m.get('http_req_failed', {})
def get(k, default='n/a'):
    return http.get(k, default)
def fmt(x):
    if isinstance(x, (int, float)): return f"{x:.2f}"
    return str(x)
# k6 Rate-metric semantic: for `http_req_failed`, `passes` is the count of
# requests that "passed the failed-check" — i.e. requests that DID fail.
# `fails` is the count that did NOT fail (succeeded). `value` is passes/total,
# i.e. the actual failure rate. Counter-intuitive but verified against several
# of our own runs.
err_count = err.get('passes', 0)
err_total = err.get('passes', 0) + err.get('fails', 0)
err_rate = err.get('value', 0)
print(f"| {tier} | {iters.get('rate', 0):.0f} | {iters.get('count', 0):,} | "
      f"{fmt(get('med'))} | {fmt(get('p(95)'))} | {fmt(get('p(99)'))} | "
      f"{fmt(get('p(99.9)'))} | {fmt(get('max'))} | "
      f"{err_count}/{err_total} ({err_rate*100:.4f}%) |")
PY
done

# Per-tier counter delta: bidder Prometheus counters are cumulative across
# the bidder process lifetime, so tier N's snapshot includes tiers 1..N-1.
# `make baseline-tiered` writes a `*-prometheus-before.txt` snapshot just
# before each tier starts; this helper subtracts before-from-after so the
# table shows the actual per-tier numbers, not running totals.
counter_delta() {
    # $1 = after-file, $2 = before-file (may not exist), $3 = pattern (literal-prefix)
    local after_val before_val
    after_val=$(awk -v needle="$3" 'index($0, needle) == 1 { print $2; exit }' "$1")
    after_val=${after_val:-0}
    if [ -f "$2" ]; then
        before_val=$(awk -v needle="$3" 'index($0, needle) == 1 { print $2; exit }' "$2")
        before_val=${before_val:-0}
    else
        before_val=0
    fi
    python3 -c "print(int(float('$after_val') - float('$before_val')))"
}

echo
echo "## Bidder pipeline outcomes (per tier — deltas)"
echo
echo "| Target RPS | bids | no-bids | bid rate | early drops | budget-exhausted candidates filtered |"
echo "|---:|---:|---:|---:|---:|---:|"
for tier in "${sorted[@]}"; do
    f="$RESULTS_DIR/v0-baseline-${tier}rps-prometheus.txt"
    fb="$RESULTS_DIR/v0-baseline-${tier}rps-prometheus-before.txt"
    [ -f "$f" ] || continue
    bid=$(counter_delta "$f" "$fb" 'bidder_bid_requests_total{result="bid"}')
    nobid=$(counter_delta "$f" "$fb" 'bidder_bid_requests_total{result="no_bid"}')
    early=$(counter_delta "$f" "$fb" 'bidder_pipeline_early_drop ')
    # budget_exhausted_filtered: candidates that the BudgetPacingStage dropped
    # because their campaign's daily budget was exhausted. Climbing values
    # across tiers signal that compressed-time load tests are draining test
    # budgets faster than the seed accounted for — re-tune seed-postgres
    # daily_budget_cents range upward if this counter trends >>0.
    bex=$(counter_delta "$f" "$fb" 'bidder_budget_exhausted_filtered ')
    rate=$(python3 -c "print(f'{($bid)/(($bid)+($nobid))*100:.2f}%' if ($bid+$nobid)>0 else 'n/a')")
    printf "| %s | %s | %s | %s | %s | %s |\n" "$tier" "$bid" "$nobid" "$rate" "$early" "$bex"
done

echo
echo "## Resilience signals (per tier — deltas)"
echo
echo "| Target RPS | breaker opens | hedge fired | hedge blocked | freq-cap skipped (timeout) | freq-cap skipped (breaker) | kafka events_dropped |"
echo "|---:|---:|---:|---:|---:|---:|---:|"
for tier in "${sorted[@]}"; do
    f="$RESULTS_DIR/v0-baseline-${tier}rps-prometheus.txt"
    fb="$RESULTS_DIR/v0-baseline-${tier}rps-prometheus-before.txt"
    [ -f "$f" ] || continue
    bo=$(counter_delta "$f" "$fb" 'bidder_circuit_breaker_opened{name="redis"}')
    hf=$(counter_delta "$f" "$fb" 'bidder_redis_hedge_fired ')
    hb=$(counter_delta "$f" "$fb" 'bidder_redis_hedge_blocked ')
    fct=$(counter_delta "$f" "$fb" 'bidder_freq_cap_skipped{reason="timeout"}')
    fcb=$(counter_delta "$f" "$fb" 'bidder_freq_cap_skipped{reason="breaker_open"}')
    ked=$(counter_delta "$f" "$fb" 'bidder_kafka_events_dropped ')
    printf "| %s | %s | %s | %s | %s | %s | %s |\n" "$tier" "$bo" "$hf" "$hb" "$fct" "$fcb" "$ked"
done

echo
echo "## Per-stage timing (last tier — server-internal latency)"
echo
last_tier="${sorted[$((${#sorted[@]} - 1))]}"
last_f="$RESULTS_DIR/v0-baseline-${last_tier}rps-prometheus.txt"
echo "Source: \`$last_f\` (target ${last_tier} RPS)"
echo
echo "| Stage | p50 (µs) | p99 (µs) | p99.9 (µs) | max (µs) | budget exceeded |"
echo "|---|---:|---:|---:|---:|---:|"
stage_extract() {
    # $1 = file, $2 = literal pattern (caller pre-builds with stage name).
    # Use awk index() for literal-string match (the patterns contain `{}` and
    # `=` which are awk regex metachars).
    awk -v needle="$2" 'index($0, needle) == 1 { print $2; exit }' "$1"
}

for stage in request_validation user_enrichment candidate_retrieval candidate_limit scoring frequency_cap budget_pacing ranking response_build; do
    p50=$(stage_extract "$last_f" "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"0.5\"}")
    p99=$(stage_extract "$last_f" "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"0.99\"}")
    p999=$(stage_extract "$last_f" "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"0.999\"}")
    pmax=$(stage_extract "$last_f" "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"1\"}")
    bex=$(stage_extract "$last_f" "bidder_pipeline_stage_budget_exceeded{stage=\"$stage\"}")
    p50_us=$(python3 -c "print(f'{${p50:-0}*1e6:.1f}')")
    p99_us=$(python3 -c "print(f'{${p99:-0}*1e6:.1f}')")
    p999_us=$(python3 -c "print(f'{${p999:-0}*1e6:.1f}')")
    pmax_us=$(python3 -c "print(f'{${pmax:-0}*1e6:.1f}')")
    printf "| %s | %s | %s | %s | %s | %s |\n" "$stage" "$p50_us" "$p99_us" "$p999_us" "$pmax_us" "${bex:-0}"
done

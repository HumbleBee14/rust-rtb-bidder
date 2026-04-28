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
err_count = err.get('passes', 0)
err_total = err.get('passes', 0) + err.get('fails', 0)
err_rate = err.get('value', 0)
print(f"| {tier} | {iters.get('rate', 0):.0f} | {iters.get('count', 0):,} | "
      f"{fmt(get('med'))} | {fmt(get('p(95)'))} | {fmt(get('p(99)'))} | "
      f"{fmt(get('p(99.9)'))} | {fmt(get('max'))} | "
      f"{err_count}/{err_total} ({err_rate*100:.4f}%) |")
PY
done

echo
echo "## Bidder pipeline outcomes (per tier)"
echo
echo "| Target RPS | total bids | total no-bids | bid rate | early drops |"
echo "|---:|---:|---:|---:|---:|"
for tier in "${sorted[@]}"; do
    f="$RESULTS_DIR/v0-baseline-${tier}rps-prometheus.txt"
    [ -f "$f" ] || continue
    bid=$(grep -E '^bidder_bid_requests_total\{result="bid"\}' "$f" | awk '{print $2}')
    nobid=$(grep -E '^bidder_bid_requests_total\{result="no_bid"\}' "$f" | awk '{print $2}')
    early=$(grep '^bidder_pipeline_early_drop ' "$f" | awk '{print $2}')
    bid=${bid:-0}; nobid=${nobid:-0}; early=${early:-0}
    total=$(echo "$bid + $nobid" | bc)
    rate=$(python3 -c "print(f'{($bid)/(($bid)+($nobid))*100:.2f}%' if ($bid+$nobid)>0 else 'n/a')")
    printf "| %s | %s | %s | %s | %s |\n" "$tier" "$bid" "$nobid" "$rate" "$early"
done

echo
echo "## Resilience signals (per tier)"
echo
echo "| Target RPS | breaker opens | hedge fired | hedge blocked | freq-cap skipped (timeout) | freq-cap skipped (breaker) | kafka events_dropped |"
echo "|---:|---:|---:|---:|---:|---:|---:|"
for tier in "${sorted[@]}"; do
    f="$RESULTS_DIR/v0-baseline-${tier}rps-prometheus.txt"
    [ -f "$f" ] || continue
    extract() {
        local pattern="$1"
        grep -E "$pattern" "$f" | awk '{print $2}' | head -1
    }
    bo=$(extract '^bidder_circuit_breaker_opened\{name="redis"\}'); bo=${bo:-0}
    hf=$(extract '^bidder_redis_hedge_fired '); hf=${hf:-0}
    hb=$(extract '^bidder_redis_hedge_blocked '); hb=${hb:-0}
    fct=$(extract '^bidder_freq_cap_skipped\{reason="timeout"\}'); fct=${fct:-0}
    fcb=$(extract '^bidder_freq_cap_skipped\{reason="breaker_open"\}'); fcb=${fcb:-0}
    ked=$(extract '^bidder_kafka_events_dropped '); ked=${ked:-0}
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
for stage in request_validation user_enrichment candidate_retrieval candidate_limit scoring frequency_cap budget_pacing ranking response_build; do
    p50=$(grep "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"0.5\"}" "$last_f" | awk '{print $2}')
    p99=$(grep "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"0.99\"}" "$last_f" | awk '{print $2}')
    p999=$(grep "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"0.999\"}" "$last_f" | awk '{print $2}')
    pmax=$(grep "bidder_pipeline_stage_duration_seconds{stage=\"$stage\",quantile=\"1\"}" "$last_f" | awk '{print $2}')
    bex=$(grep "bidder_pipeline_stage_budget_exceeded{stage=\"$stage\"}" "$last_f" | awk '{print $2}')
    p50_us=$(python3 -c "print(f'{${p50:-0}*1e6:.1f}')")
    p99_us=$(python3 -c "print(f'{${p99:-0}*1e6:.1f}')")
    p999_us=$(python3 -c "print(f'{${p999:-0}*1e6:.1f}')")
    pmax_us=$(python3 -c "print(f'{${pmax:-0}*1e6:.1f}')")
    printf "| %s | %s | %s | %s | %s | %s |\n" "$stage" "$p50_us" "$p99_us" "$p999_us" "$pmax_us" "${bex:-0}"
done

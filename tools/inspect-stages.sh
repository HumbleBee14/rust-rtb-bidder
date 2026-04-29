#!/usr/bin/env bash
# inspect-stages — snapshot the bidder's /metrics endpoint and break down where
# pipeline time is going, plus key cache-health counters.
#
# Usage:
#   bash tools/inspect-stages.sh                    # snapshot now, no diff
#   bash tools/inspect-stages.sh --reset            # snapshot + remember it
#   bash tools/inspect-stages.sh --since-reset      # diff against last reset
#
# How it works:
#   The bidder publishes Prometheus histograms with `_sum` and `_count` per
#   stage. avg_ms = (sum / count) * 1000. No PromQL or Grafana required —
#   the raw text from `/metrics` is enough to compute everything.
#
#   Why the --reset/--since-reset pair: counters are cumulative since the
#   bidder process started. Run `--reset` immediately before starting a
#   stress run, then `--since-reset` after, and the diff shows ONLY the
#   work done during that run. Without --since-reset every snapshot
#   includes every request the bidder ever served.

set -euo pipefail

URL="${METRICS_URL:-http://localhost:9090/metrics}"
SNAPSHOT="/tmp/bidder-metrics-snapshot.txt"
RESET_FILE="/tmp/bidder-metrics-reset.txt"

mode="now"
case "${1:-}" in
  --reset)        mode="reset" ;;
  --since-reset)  mode="since-reset" ;;
  --help|-h)
    sed -n '2,18p' "$0" | sed 's/^# //'
    exit 0
    ;;
  "") ;;
  *)  echo "unknown arg: $1" >&2 ; exit 2 ;;
esac

# Always grab a fresh snapshot
if ! curl -fsS "$URL" > "$SNAPSHOT"; then
  echo "ERROR: bidder /metrics endpoint not reachable at $URL" >&2
  echo "       (is the bidder running? \`make health\`)" >&2
  exit 1
fi

if [ "$mode" = "reset" ]; then
  cp "$SNAPSHOT" "$RESET_FILE"
  echo "snapshot saved to $RESET_FILE — run --since-reset after your test"
  exit 0
fi

# For --since-reset we need the baseline; for "now" we don't
baseline=""
if [ "$mode" = "since-reset" ]; then
  if [ ! -f "$RESET_FILE" ]; then
    echo "ERROR: no baseline; run --reset first" >&2
    exit 1
  fi
  baseline="$RESET_FILE"
fi

python3 - "$SNAPSHOT" "$baseline" <<'PY'
import re, sys

after_path = sys.argv[1]
before_path = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else None

def parse(path):
    text = open(path).read()
    sums = {m.group(1): float(m.group(2))
            for m in re.finditer(r'bidder_pipeline_stage_duration_seconds_sum\{stage="([^"]+)"\}\s+([\d.]+)', text)}
    cnts = {m.group(1): float(m.group(2))
            for m in re.finditer(r'bidder_pipeline_stage_duration_seconds_count\{stage="([^"]+)"\}\s+([\d.]+)', text)}
    bex  = {m.group(1): int(m.group(2))
            for m in re.finditer(r'bidder_pipeline_stage_budget_exceeded\{stage="([^"]+)"\}\s+(\d+)', text)}
    counters = {}
    for line in text.splitlines():
        if line.startswith('#') or not line.strip():
            continue
        m = re.match(r'(bidder_freq_cap_in_process_[a-z_]+_total|bidder_freq_cap_recorder_dropped|bidder_redis_hedge_(?:fired|blocked)|bidder_circuit_breaker_opened\{name="redis"\})\s+([\d.]+)', line)
        if m:
            counters[m.group(1)] = float(m.group(2))
    return sums, cnts, bex, counters, text

a_s, a_c, a_b, a_k, a_text = parse(after_path)
if before_path:
    b_s, b_c, b_b, b_k, _ = parse(before_path)
else:
    b_s, b_c, b_b, b_k = {}, {}, {}, {}

# Per-stage breakdown using deltas
rows = []
for s in a_s:
    d_sum = a_s[s] - b_s.get(s, 0)
    d_cnt = a_c[s] - b_c.get(s, 0)
    d_bex = a_b.get(s, 0) - b_b.get(s, 0)
    if d_cnt > 0:
        avg_us = (d_sum / d_cnt) * 1_000_000
        rows.append((s, d_sum, d_cnt, avg_us, d_bex))

if not rows:
    print("no pipeline activity since reset (or bidder hasn't served any requests yet)")
    sys.exit(0)

rows.sort(key=lambda r: -r[1])
total_sum = sum(r[1] for r in rows)

label = "since reset" if before_path else "cumulative since process start"
print(f"\n  ── per-stage breakdown ({label}) ──\n")
print(f"  {'Stage':<22} {'Avg µs':>9} {'Count':>10} {'Sum (s)':>10} {'Share':>8} {'Budget over':>13}")
print(f"  {'-'*22} {'-'*9} {'-'*10} {'-'*10} {'-'*8} {'-'*13}")
for s, sm, cnt, avg, b in rows:
    pct = sm/total_sum*100 if total_sum > 0 else 0
    bar = '█' * int(pct/2)
    print(f"  {s:<22} {avg:>9.1f} {int(cnt):>10} {sm:>10.2f} {pct:>7.2f}% {b:>13}  {bar}")

# Cache-health counter deltas
interesting = [
    'bidder_freq_cap_in_process_cold_miss_total',
    'bidder_freq_cap_in_process_evictions_total',
    'bidder_freq_cap_in_process_eviction_flush_drops_total',
    'bidder_freq_cap_in_process_capacity_rejected_total',
    'bidder_freq_cap_in_process_redis_desync_total',
    'bidder_freq_cap_in_process_write_drops_total',
    'bidder_freq_cap_in_process_breaker_skipped_total',
    'bidder_freq_cap_in_process_fallback_unavailable_total',
    'bidder_freq_cap_recorder_dropped',
    'bidder_redis_hedge_fired',
    'bidder_redis_hedge_blocked',
]

shown = []
for k in interesting:
    a_v = a_k.get(k, 0)
    b_v = b_k.get(k, 0)
    delta = a_v - b_v
    if a_v == 0 and delta == 0:
        continue
    shown.append((k, a_v, delta))

if shown:
    print(f"\n  ── cache health counters ──\n")
    print(f"  {'Counter':<55} {'Value':>15} {'Delta':>15}")
    print(f"  {'-'*55} {'-'*15} {'-'*15}")
    for k, v, d in shown:
        # Format with comma separators for readability
        print(f"  {k:<55} {int(v):>15,} {int(d):>15,}")

# Total bid request count, for putting cold_miss into context
import re as _re
m = _re.search(r'bidder_bid_requests_total\{result="bid"\}\s+([\d.]+)', a_text)
m2 = _re.search(r'bidder_bid_requests_total\{result="no_bid"\}\s+([\d.]+)', a_text)
total_reqs_after = int(float(m.group(1)) + float(m2.group(1))) if m and m2 else 0
total_reqs_before = 0
if before_path:
    btext = open(before_path).read()
    bm = _re.search(r'bidder_bid_requests_total\{result="bid"\}\s+([\d.]+)', btext)
    bm2 = _re.search(r'bidder_bid_requests_total\{result="no_bid"\}\s+([\d.]+)', btext)
    total_reqs_before = int(float(bm.group(1)) + float(bm2.group(1))) if bm and bm2 else 0

reqs_delta = total_reqs_after - total_reqs_before
if reqs_delta > 0:
    cold_miss_delta = next((d for k, _, d in shown if k == 'bidder_freq_cap_in_process_cold_miss_total'), 0)
    if cold_miss_delta:
        per_req = cold_miss_delta / reqs_delta
        print(f"\n  cold misses per bid request:  {per_req:.1f}")
        print(f"  (one cold-miss = one cache lookup that fell through to Redis)")
PY

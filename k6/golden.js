// Golden load-test script for rust-rtb-bidder.
//
// Immutable benchmark contract. Every phase comparison, regression hunt,
// monoio-vs-Tokio benchmark, and CI smoke test runs THIS script. If this
// script changes, every prior result is invalidated. See k6/README.md.

import http from 'k6/http';
import { check } from 'k6';
import { SharedArray } from 'k6/data';
import { Counter, Rate } from 'k6/metrics';

// Custom counters so the k6 summary breaks "200 (bid)" out from
// "204 (no bid)". Both are HTTP successes — the default `checks` rate
// can't distinguish them — but the bid-vs-no-bid split is the headline
// business signal. Rates print as a fraction (0.0–1.0) and are tagged
// with phase so warmup samples don't pollute the measure-phase ratio.
const bidRate   = new Rate('bid_rate');     // 1 = 200 (bid), 0 = 204 (no bid)
const bidCount  = new Counter('bid_total'); // count of 200 responses
const nobidCount = new Counter('nobid_total'); // count of 204 responses

// Env-driven configuration. Defaults match Phase 1+ smoke targets.
const TARGET_RPS  = parseInt(__ENV.TARGET_RPS  || '5000', 10);
const BIDDER_URL  = __ENV.BIDDER_URL  || 'http://localhost:8080/rtb/openrtb/bid';
const RAMP_UP_S   = parseInt(__ENV.RAMP_UP_S   || '60', 10);
const HOLD_S      = parseInt(__ENV.HOLD_S      || '300', 10);
const RAMP_DOWN_S = parseInt(__ENV.RAMP_DOWN_S || '60', 10);
const WARMUP_S    = parseInt(__ENV.WARMUP_S    || '30', 10);
const CORPUS_SIZE = parseInt(__ENV.CORPUS_SIZE || '1000', 10);

// VU pool sizing — Phase 6.5 recalibration.
//
// The original 5%/20% formula starved at clustered tail-latency events:
// when the bidder's freq-cap circuit breaker tripped under sustained load,
// up to 30% of in-flight requests stalled past 50ms simultaneously, and a
// 250-VU preAlloc pool (5K × 5%) couldn't hold target rate during the stall.
// Empirically observed: at 5K target, sustained RPS dropped to ~45 with one
// request reporting 17-minute latency (k6 holding a stuck VU on a TCP
// socket past macOS retransmit timeout).
//
// New sizing: max ≈ TARGET_RPS × p99-tail-seconds × safety. With 80ms tail
// under a tripped breaker and 2× safety, that's TARGET_RPS × 0.16 ≈ 16% min.
// We round up to 50% max and 10% preAlloc with absolute floors so small-RPS
// runs (1K, smoke tests) still have enough VUs to absorb a single stall.
//
// 5K target → 500 preAlloc / 2500 max
// 10K target → 1000 preAlloc / 5000 max
const PRE_ALLOCATED_VUS = Math.max(200, Math.ceil(TARGET_RPS * 0.10));
const MAX_VUS           = Math.max(1000, Math.ceil(TARGET_RPS * 0.50));

// Seed JSON loaded once at init. open() is k6-only, executes at parse time.
const SEED = JSON.parse(open('../tests/fixtures/golden-bid-request.json'));

// Seed segment names from migrations/0001_initial.sql. These are the only
// names the bidder's SegmentRegistry resolves at startup; invented names
// would silently no-match.
const SEGMENT_VOCAB = [
  'auto-intender',
  'auto-luxury',
  'in-market-travel',
  'frequent-flyer',
  'cruise-shopper',
  'parents',
  'young-adults',
  'millennials',
  'gen-z',
  'homeowners',
  'high-income',
  'sports-fans',
  'gamers',
  'foodies',
  'fitness-enthusiasts',
  'tech-early-adopters',
  'fashion-shoppers',
  'back-to-school',
  'holiday-shoppers',
  'small-business-owners',
];

// Plausible US metros: Nielsen DMA + state pairs. Exercises the geo
// inverted index across ten distinct slots.
const GEO_POOL = [
  { metro: '501', region: 'NY', city: 'New York',     zip: '10013' },
  { metro: '803', region: 'CA', city: 'Los Angeles',  zip: '90012' },
  { metro: '602', region: 'IL', city: 'Chicago',      zip: '60601' },
  { metro: '623', region: 'TX', city: 'Dallas',       zip: '75201' },
  { metro: '618', region: 'TX', city: 'Houston',      zip: '77002' },
  { metro: '819', region: 'WA', city: 'Seattle',      zip: '98101' },
  { metro: '807', region: 'CA', city: 'San Francisco',zip: '94103' },
  { metro: '511', region: 'DC', city: 'Washington',   zip: '20001' },
  { metro: '534', region: 'FL', city: 'Orlando',      zip: '32801' },
  { metro: '524', region: 'GA', city: 'Atlanta',      zip: '30303' },
];

// User-id zone boundaries. Hot range matches the 50K warm-set capacity
// from REDIS-KEYS.md; long tail ends at 100M to mimic the production
// audience size.
const HOT_USER_MAX  = 50000;
const TAIL_USER_MAX = 100000000;
const HOT_USER_SHARE = 0.80; // 80% of traffic targets the hot 50K

// ---------------------------------------------------------------------------
// Corpus generation
// ---------------------------------------------------------------------------

// Pick a Zipf-like user id. Closed-form approximation: 80% of picks
// land in [1..HOT_USER_MAX], 20% in (HOT_USER_MAX..TAIL_USER_MAX].
// Within the hot range we use an inverse-rank weighting so id 1 is
// hotter than id 50000, matching DMP-observed access skew without
// the cost of a true Zipf CDF.
function pickZipfUser(rng) {
  if (rng() < HOT_USER_SHARE) {
    // Inverse-rank: u = floor(HOT_USER_MAX ^ r), r in [0,1).
    // This concentrates picks near id 1 with a long internal tail
    // toward HOT_USER_MAX. Cheap to compute, adequate for traffic shape.
    const r = rng();
    const id = Math.floor(Math.pow(HOT_USER_MAX, r));
    return Math.max(1, id);
  }
  // Long tail: uniform pick over (HOT_USER_MAX..TAIL_USER_MAX]. LCG-driven
  // so the corpus stays byte-identical across runs.
  return HOT_USER_MAX + 1 + Math.floor(rng() * (TAIL_USER_MAX - HOT_USER_MAX));
}

// Pick 5-15 segments. Bias toward the front of SEGMENT_VOCAB to mimic
// segment popularity skew (the first few names are demographic and
// appear on most users; niche shopping/b2b segments appear less often).
function pickSegments(rng) {
  const count = 5 + Math.floor(rng() * 11); // [5..15]
  const used = new Set();
  const out = [];
  while (out.length < count) {
    // Skewed pick: square the rng so low indices dominate.
    const r = rng();
    const idx = Math.floor(r * r * SEGMENT_VOCAB.length);
    if (used.has(idx)) continue;
    used.add(idx);
    const name = SEGMENT_VOCAB[idx];
    // Mix of certain (1) and probabilistic values, mirroring the seed fixture.
    const value = out.length < 4 ? '1' : (0.5 + rng() * 0.5).toFixed(2);
    out.push({ id: name, name: name, value: value });
  }
  return out;
}

// Device-type mix: 70% PC (2), 20% mobile-tablet (1), 10% phone (4).
function pickDeviceType(rng) {
  const r = rng();
  if (r < 0.70) return 2;
  if (r < 0.90) return 1;
  return 4;
}

// Build one corpus variation off the seed. The variation mutates only
// fields that affect bidder cache keys and targeting fan-out; static
// fields (bcat, site, source, regs) stay verbatim from the seed.
function buildVariation(index, rng) {
  // Deep clone via JSON round-trip. Cheap at 1K iterations during init.
  const v = JSON.parse(JSON.stringify(SEED));

  v.id = `gold-${index}`;

  // Per-imp unique id. tagid + bidfloor preserved.
  v.imp.forEach((imp, impIdx) => {
    imp.id = `imp-${index}-${impIdx}`;
  });

  const userId = pickZipfUser(rng);
  v.user.id = String(userId);
  v.user.data[0].segment = pickSegments(rng);

  const geo = GEO_POOL[Math.floor(rng() * GEO_POOL.length)];
  v.device.geo.metro = geo.metro;
  v.device.geo.region = geo.region;
  v.device.geo.city = geo.city;
  v.device.geo.zip = geo.zip;

  v.device.devicetype = pickDeviceType(rng);

  return JSON.stringify(v);
}

// Deterministic LCG so corpus generation is reproducible across runs.
function makeRng(seed) {
  let s = seed >>> 0;
  return function () {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

// SharedArray init runs once per process; corpus is read-only across VUs.
const corpus = new SharedArray('corpus', () => {
  const rng = makeRng(0xC0FFEE);
  const out = new Array(CORPUS_SIZE);
  for (let i = 0; i < CORPUS_SIZE; i++) {
    out[i] = buildVariation(i, rng);
  }
  return out;
});

// ---------------------------------------------------------------------------
// Scenario / thresholds
// ---------------------------------------------------------------------------

// Two-phase scenario layout — matches the Java sibling's k6-stress.js.
//
//   warmup  (tagged phase:warmup)  — absorbs per-tier transients (fred pool
//                                    growth, jemalloc warmth, breaker re-arm,
//                                    moka cache fill at the new RPS).
//                                    Excluded from threshold gates.
//   measure (tagged phase:measure) — steady-state, what we actually grade.
//
// The bidder process itself is also pre-warmed (catalog loaded, segment
// cache pre-populated, /health/ready flips green) — see warmup.rs. The k6
// warmup phase here is purely the per-tier load-shape ramp.
export const options = {
  scenarios: {
    warmup: {
      executor: 'constant-arrival-rate',
      rate: TARGET_RPS,
      timeUnit: '1s',
      duration: `${WARMUP_S}s`,
      preAllocatedVUs: PRE_ALLOCATED_VUS,
      maxVUs: MAX_VUS,
      tags: { phase: 'warmup' },
    },
    measure: {
      executor: 'ramping-arrival-rate',
      startRate: TARGET_RPS,
      startTime: `${WARMUP_S}s`,
      timeUnit: '1s',
      preAllocatedVUs: PRE_ALLOCATED_VUS,
      maxVUs: MAX_VUS,
      stages: [
        { duration: `${RAMP_UP_S}s`,   target: TARGET_RPS },
        { duration: `${HOLD_S}s`,      target: TARGET_RPS },
        { duration: `${RAMP_DOWN_S}s`, target: 0 },
      ],
      tags: { phase: 'measure' },
    },
  },
  // Thresholds gate ONLY the measure phase. Warmup samples are still
  // collected and visible in the per-phase summary but don't affect pass/fail.
  thresholds: {
    'http_req_failed{phase:measure}': [
      { threshold: 'rate<0.001', abortOnFail: true },
    ],
    // Latency budgets are k6-side (HTTP RTT — bidder processing + loopback +
    // k6 overhead). Aligned with the Java sibling's k6-baseline.js so
    // Rust-vs-Java numbers compare apples-to-apples on the same hardware.
    // p99 = 25 ms keeps the bidder's server-internal 50 ms SLA bounded with
    // loopback slack; abortOnFail on critical percentiles fails fast on
    // serious regressions instead of waiting for the full hold to finish.
    'http_req_duration{phase:measure,expected_response:true}': [
      { threshold: 'p(50)<5',    abortOnFail: true },
      { threshold: 'p(95)<10',   abortOnFail: true },
      { threshold: 'p(99)<25',   abortOnFail: true },   // SLA boundary
      { threshold: 'p(99.9)<50', abortOnFail: false },
      { threshold: 'max<100',    abortOnFail: false },
    ],
    'checks{phase:measure}': [
      { threshold: 'rate>0.999', abortOnFail: true },
    ],
    // Display-only thresholds so bid_rate / bid count / nobid count appear
    // in the THRESHOLDS block. `rate>=0` always passes — we're using
    // thresholds as a visibility vehicle, not a gate. Real bid-rate gates
    // belong on the bidder-side Prometheus metric, not here.
    'bid_rate{phase:measure}':    [{ threshold: 'rate>=0' }],
    'bid_total{phase:measure}':   [{ threshold: 'count>=0' }],
    'nobid_total{phase:measure}': [{ threshold: 'count>=0' }],
  },
  // Bodies are not inspected; discarding reduces memory/GC pressure under load.
  discardResponseBodies: true,
  // Compute every percentile we gate on. k6's default summaryTrendStats only
  // populates p(95) — without this list, p(50)/p(99)/p(99.9)/max come back
  // empty when the summary block tries to render them.
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(50)', 'p(95)', 'p(99)', 'p(99.9)'],
};

// ---------------------------------------------------------------------------
// VU function
// ---------------------------------------------------------------------------

const HEADERS = {
  'Content-Type': 'application/json',
  'x-openrtb-version': '2.6',
  'Accept': 'application/json',
  'User-Agent': 'k6-golden-loadtest/1.0',
};

export default function () {
  const body = corpus[__ITER % CORPUS_SIZE];
  // 5s timeout: well above the 50ms p99 SLA but below the macOS default TCP
  // retransmit window (~15min). When the bidder breaker trips and a request
  // genuinely stalls, k6 fails the iteration in 5s instead of holding the VU
  // on a hung socket. Critical for sustained-RPS measurement under tail
  // events. Phase 6.5 recalibration; see k6/README.md.
  const res = http.post(BIDDER_URL, body, { headers: HEADERS, timeout: '5s' });

  // Latency is enforced via http_req_duration thresholds (p99 / p99.9), not
  // here — a per-request check would gate every request at p99 budget.
  check(res, {
    'status 200 or 204': (r) => r.status === 200 || r.status === 204,
  });

  // 200 = bid placed, 204 = no bid (eligible request, no candidate matched
  // / all filtered). Both succeed at the HTTP layer; tracking the split
  // separately so the summary shows actual bid rate.
  if (res.status === 200) {
    bidRate.add(1);
    bidCount.add(1);
  } else if (res.status === 204) {
    bidRate.add(0);
    nobidCount.add(1);
  }
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------
// Custom breakdown panel printed above k6's default output. Makes the bid /
// no-bid / failure split explicit instead of leaving the operator to do
// mental math on raw counters.

export function handleSummary(data) {
  const m       = data.metrics;
  const bids    = (m.bid_total       && m.bid_total.values.count)         || 0;
  const nobids  = (m.nobid_total     && m.nobid_total.values.count)       || 0;
  const reqs    = (m.http_reqs       && m.http_reqs.values.count)         || 0;
  const failed  = (m.http_req_failed && m.http_req_failed.values.passes)  || 0;
  const measureBids   = (m['bid_total{phase:measure}']   && m['bid_total{phase:measure}'].values.count)   || bids;
  const measureNobids = (m['nobid_total{phase:measure}'] && m['nobid_total{phase:measure}'].values.count) || nobids;
  const measureReqs   = measureBids + measureNobids;

  // ANSI color helpers (only used inside the THRESHOLDS section).
  const GREEN = '\x1b[32m', RED = '\x1b[31m', RESET = '\x1b[0m';

  const pct = (n, d) => (d > 0 ? (n / d * 100).toFixed(2) : '0.00');

  // ── BLOCK 1: OUTCOME BREAKDOWN ───────────────────────────────────────────
  const row = (label, count, pctStr) =>
    `      ${label.padEnd(14)}${count.toLocaleString().padStart(12)}    (${pctStr}%)`;

  const outcomeBlock = `
  █ OUTCOME BREAKDOWN (measure phase)

    HTTP responses
${row('total:',        measureReqs,    '100.00')}
${row('200 (bid):',    measureBids,    pct(measureBids,   measureReqs))}
${row('204 (no-bid):', measureNobids,  pct(measureNobids, measureReqs))}
${row('errors:',       failed,         pct(failed, reqs))}
`;

  // ── BLOCK 2: THRESHOLDS ───────────────────────────────────────────────────
  // Emits each metric that has thresholds, with green ✓ / red ✗ per gate.
  // Order is explicit (not insertion-order) so related metrics group
  // visually: latency first (the headline SLO gates), then outcome counts
  // (bid + nobid adjacent so they read as a pair), then health rates.
  const ORDER = [
    'http_req_duration{phase:measure,expected_response:true}',
    'bid_total{phase:measure}',
    'nobid_total{phase:measure}',
    'bid_rate{phase:measure}',
    'checks{phase:measure}',
    'http_req_failed{phase:measure}',
  ];
  const seen = new Set();
  const orderedNames = [];
  // Pinned order first.
  for (const name of ORDER) {
    if (m[name] && m[name].thresholds && Object.keys(m[name].thresholds).length > 0) {
      orderedNames.push(name);
      seen.add(name);
    }
  }
  // Anything else with thresholds, in insertion order, after the pinned ones.
  for (const [name, metric] of Object.entries(m)) {
    if (seen.has(name)) continue;
    if (!metric.thresholds || Object.keys(metric.thresholds).length === 0) continue;
    orderedNames.push(name);
  }

  const thresholdLines = ['\n  █ THRESHOLDS\n'];
  for (const name of orderedNames) {
    const metric = m[name];
    thresholdLines.push(`    ${name}`);
    // Within a metric, sort threshold expressions explicitly. k6's runtime
    // doesn't always preserve source order for keys containing `(` etc., so
    // p(99) sometimes leaked above p(50). We force: p(50) → p(95) → p(99)
    // → p(99.9) → max → everything else.
    const exprOrder = (e) => {
      const m = e.match(/p\(([\d.]+)\)/);
      if (m) return [0, parseFloat(m[1])];           // percentiles, by N ascending
      if (e.startsWith('max')) return [1, 0];         // max after percentiles
      return [2, e];                                  // anything else, alphabetical
    };
    const sortedExprs = Object.entries(metric.thresholds).sort((a, b) => {
      const [aBucket, aKey] = exprOrder(a[0]);
      const [bBucket, bKey] = exprOrder(b[0]);
      if (aBucket !== bBucket) return aBucket - bBucket;
      return aKey < bKey ? -1 : aKey > bKey ? 1 : 0;
    });
    for (const [expr, result] of sortedExprs) {
      const ok = result.ok !== false;
      const mark = ok ? `${GREEN}✓${RESET}` : `${RED}✗${RESET}`;
      const v = metric.values || {};
      // Pick the value that matches the threshold expression. Falls back to
      // result.lastValue (k6 always populates this on the threshold object)
      // when metric.values doesn't have the percentile we asked about.
      let display = '';
      const fallback = (typeof result.lastValue === 'number') ? result.lastValue : null;
      if (expr.startsWith('p(')) {
        const q = expr.match(/p\(([\d.]+)\)/);
        const key = q ? `p(${q[1]})` : null;
        const val = (key && key in v) ? v[key] : fallback;
        if (val !== null) display = `${key}=${val.toFixed(2)}ms`;
      } else if (expr.startsWith('max')) {
        const val = ('max' in v) ? v.max : fallback;
        if (val !== null) display = `max=${val.toFixed(2)}ms`;
      } else if (expr.startsWith('rate')) {
        const val = ('rate' in v) ? v.rate : fallback;
        if (val !== null) display = `rate=${(val * 100).toFixed(2)}%`;
      } else if (expr.startsWith('count')) {
        const val = ('count' in v) ? v.count : fallback;
        if (val !== null) display = `count=${val}`;
      }
      thresholdLines.push(`    ${mark} '${expr}' ${display}`);
    }
    thresholdLines.push('');
  }
  const thresholdBlock = thresholdLines.join('\n');

  // ── BLOCK 3: TOTAL RESULTS ────────────────────────────────────────────────
  // The standard summary metrics that aren't gated as thresholds. Three
  // groupings: HTTP, EXECUTION, NETWORK. Plain text — no colors here.
  const fmtRate    = (v) => `${v.toFixed(2)}/s`;
  const fmtBytes   = (v) => v >= 1e9 ? `${(v/1e9).toFixed(1)} GB` : v >= 1e6 ? `${(v/1e6).toFixed(1)} MB` : `${(v/1e3).toFixed(1)} kB`;
  const fmtMs      = (v) => v >= 1 ? `${v.toFixed(2)}ms` : `${(v*1000).toFixed(0)}µs`;
  const dur        = m.http_req_duration ? m.http_req_duration.values : {};
  const iter       = m.iteration_duration ? m.iteration_duration.values : {};
  const iters      = m.iterations ? m.iterations.values : {};
  const recv       = m.data_received ? m.data_received.values : {};
  const sent       = m.data_sent ? m.data_sent.values : {};
  const httpReqs   = m.http_reqs ? m.http_reqs.values : {};
  const httpFailed = m.http_req_failed ? m.http_req_failed.values : {};

  const padLabel = (s) => s.padEnd(28, '.');

  const totalBlock = `
  █ TOTAL RESULTS

    HTTP
      ${padLabel('http_req_duration')}: avg=${fmtMs(dur.avg||0)}  med=${fmtMs(dur.med||0)}  p(95)=${fmtMs(dur['p(95)']||0)}  p(99)=${fmtMs(dur['p(99)']||0)}  max=${fmtMs(dur.max||0)}
      ${padLabel('http_req_failed')}: ${(httpFailed.rate*100||0).toFixed(2)}%       ${httpFailed.passes||0}/${reqs}
      ${padLabel('http_reqs')}: ${reqs.toLocaleString()}     ${fmtRate(httpReqs.rate||0)}

    EXECUTION
      ${padLabel('iterations')}: ${(iters.count||0).toLocaleString()}     ${fmtRate(iters.rate||0)}
      ${padLabel('iteration_duration')}: avg=${fmtMs(iter.avg||0)}  med=${fmtMs(iter.med||0)}  p(95)=${fmtMs(iter['p(95)']||0)}

    NETWORK
      ${padLabel('data_received')}: ${fmtBytes(recv.count||0)}      ${fmtBytes(recv.rate||0)}/s
      ${padLabel('data_sent')}: ${fmtBytes(sent.count||0)}      ${fmtBytes(sent.rate||0)}/s
`;

  return {
    stdout: outcomeBlock + thresholdBlock + totalBlock,
    [__ENV.SUMMARY_PATH || 'load-test/results/k6-summary.json']: JSON.stringify(data, null, 2),
  };
}

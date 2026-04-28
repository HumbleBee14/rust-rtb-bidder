// Golden load-test script for rust-rtb-bidder.
//
// Immutable benchmark contract. Every phase comparison, regression hunt,
// monoio-vs-Tokio benchmark, and CI smoke test runs THIS script. If this
// script changes, every prior result is invalidated. See k6/README.md.

import http from 'k6/http';
import { check } from 'k6';
import { SharedArray } from 'k6/data';

// Env-driven configuration. Defaults match Phase 1+ smoke targets.
const TARGET_RPS  = parseInt(__ENV.TARGET_RPS  || '5000', 10);
const BIDDER_URL  = __ENV.BIDDER_URL  || 'http://localhost:8080/rtb/openrtb/bid';
const RAMP_UP_S   = parseInt(__ENV.RAMP_UP_S   || '60', 10);
const HOLD_S      = parseInt(__ENV.HOLD_S      || '300', 10);
const RAMP_DOWN_S = parseInt(__ENV.RAMP_DOWN_S || '60', 10);
const CORPUS_SIZE = parseInt(__ENV.CORPUS_SIZE || '1000', 10);

// VU pool sized as a fraction of arrival-rate target. The arrival-rate
// executor allocates VUs on demand; preAllocated is the warm pool, max
// is the safety ceiling under tail-latency spikes.
const PRE_ALLOCATED_VUS = Math.ceil(TARGET_RPS * 0.05);
const MAX_VUS           = Math.ceil(TARGET_RPS * 0.20);

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

export const options = {
  scenarios: {
    bid: {
      executor: 'ramping-arrival-rate',
      startRate: 0,
      timeUnit: '1s',
      preAllocatedVUs: PRE_ALLOCATED_VUS,
      maxVUs: MAX_VUS,
      stages: [
        { duration: `${RAMP_UP_S}s`,   target: TARGET_RPS },
        { duration: `${HOLD_S}s`,      target: TARGET_RPS },
        { duration: `${RAMP_DOWN_S}s`, target: 0 },
      ],
    },
  },
  thresholds: {
    // <0.1% non-2xx/3xx. Includes 503 load-shed, which is a failure here.
    http_req_failed: ['rate<0.001'],
    // p99 < 80ms accounts for k6 client-side RTT on top of the 50ms
    // server-internal SLA. p99.9 < 200ms is the tail discipline gate.
    http_req_duration: ['p(99)<80', 'p(99.9)<200'],
    checks: ['rate>0.999'],
  },
  // Bodies are not inspected; discarding reduces memory/GC pressure under load.
  discardResponseBodies: true,
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
  const res = http.post(BIDDER_URL, body, { headers: HEADERS });

  // Latency is enforced via http_req_duration thresholds (p99 / p99.9), not
  // here — a per-request check would gate every request at p99 budget.
  check(res, {
    'status 200 or 204': (r) => r.status === 200 || r.status === 204,
  });
}

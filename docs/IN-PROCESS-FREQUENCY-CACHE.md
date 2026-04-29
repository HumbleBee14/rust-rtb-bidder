# In-process L1 cache for frequency capping

This document covers the in-process freq-cap cache (the moka layer in front of Redis) — what it is, why it isn't on by default, what it costs, and exactly how the cache stays consistent with Redis under heavy concurrent updates.

If you've seen `BIDDER__FREQ_CAP__IN_PROCESS_ENABLED` in `.env.example` and wondered why it's off by default given how dramatic the perf gain is — this is the doc.

---

## What is "frequency capping" and where does it live

A frequency cap is a contract: "show campaign C to user U at most N times per day (or per hour)." The bidder enforces this on every bid request — for each candidate campaign, it has to know how many impressions of that campaign user U has already seen today and this hour, then drop any candidate that would cross the cap.

The counters live in Redis under keys like:

```
v1:fc:{u:<userId>}:c:<campaignId>:d:<yyyymmdd>     ← daily counter
v1:fc:{u:<userId>}:c:<campaignId>:h:<yyyymmddHH>   ← hourly counter
```

Each counter is a small integer. The bidder reads the relevant pair for every candidate on every bid request.

**At 50K RPS with ~10–50 candidates per request**, that's somewhere in the order of half a million to two million Redis lookups per second. Even with pipelining and `MGET` batching, this is the single most expensive thing the bidder does.

## L1 / L2 caching, in plain terms

When people talk about "L1" and "L2" caches in distributed systems, they mean:

| | What | Where it lives | Speed | Cross-process visible? |
|---|---|---|---|---|
| **L1** | Process-local cache | In the bidder's own RAM | sub-microsecond | No — each pod has its own |
| **L2** | Shared cache | A separate service (Redis here) | ~1 ms over loopback, 1–5 ms over network | Yes — every pod sees the same data |

**Without an L1**, every bid pays for an L2 round-trip:

```
                bid request
                     │
                     ▼
              freq_cap stage
                     │
                     ▼
              Redis MGET ~50 keys     ← ~1 ms RTT every time
                     │
                     ▼
              decode, compare to caps
```

**With an L1** (moka in our case) sitting in front:

```
                bid request
                     │
                     ▼
              freq_cap stage
                     │
                     ▼
              moka.get(user_id)         ← ~0.5 µs hash + atomic load
                     │
        ┌────────────┴────────────┐
        ▼                         ▼
       HIT                       MISS
   (warm user)               (first time we've
        │                     seen this user, or
        │                     evicted under pressure)
        │                         │
        ▼                         ▼
   read counters            fall through to Redis
   from RAM                 once, cache the result,
                            then serve from L1 next time
```

The relevant numbers:

- moka L1 hit: ~**0.5 µs** (in-process atomic load, ~1 CPU cache line)
- Redis L2 hit: ~**1 ms** on Docker for Mac, ~**0.2–0.5 ms** on production native Linux

So a hit is **roughly 2,000× faster** than the L2-only path, and we hit the L2 only on cold-miss for users we haven't seen recently. With Zipfian access patterns (80% of traffic on the hot 50K user IDs), the cache warms up in seconds and hit-rate sits above 90% in steady state.

## Why moka, specifically

[moka](https://github.com/moka-rs/moka) is a Rust port of [Caffeine](https://github.com/ben-manes/caffeine), the JVM cache library. Both implement **TinyLFU** eviction — a frequency-aware admission filter that keeps "valuable" entries (frequent + recent) and evicts the rest. This matters because a naive LRU would evict useful entries during a burst of one-shot users; TinyLFU resists that pollution.

Other relevant moka properties for our usage:

- **Thread-safe atomic operations** on the entry — we can `compute()` against an entry without taking an external lock
- **Lock-free reads** on hits — no contention even at 50K RPS
- **Eviction listener** — fires when an entry is evicted; we use this to flush counter state to Redis before the entry vanishes

We use `moka::sync::Cache` (not the `future` variant) because the freq-cap stage is CPU-bound atomic ops, no `.await` needed.

---

# Improvement Fix 1:

## Caching the cold-miss verdict — the per-candidate fix

The cache as originally written had a subtle leak: it cached **per-user**, but freq-cap is checked **per-(user, campaign)**. With ~30 candidate campaigns per bid request and the average user having only seen a small subset of campaigns historically, every bid request hit the cache for the user but missed for ~25 of the 30 candidates — each cold miss falling through to a Redis hop.

The fix: when the fallback to Redis returned the actual day/hour counts, **insert those counts into moka** so the next bid for that (user, campaign) pair is a cache hit instead of another Redis trip. Conceptually small, structurally significant: it changed the cache hit rate at the granularity that actually matters.

### Measured impact

Same workload (~3,000 active campaigns, real seeded Redis data, 10K RPS measure phase, in-process cache enabled in both runs):

| Stage | Before fix | After fix | Change |
|---|---|---|---|
| **frequency_cap (avg)** | 1,034 µs | **9.2 µs** | **~112× faster** |
| frequency_cap share of pipeline | 75.7% | 2.8% | freed the bottleneck |
| candidate_retrieval (avg) | 307 µs | 297 µs | ≈ unchanged |
| Other stages combined | ~25 µs | ~24 µs | unchanged |

| Cache health | Before fix | After fix |
|---|---|---|
| Cold misses per bid request | **24.6** | **0.10** |
| Per-candidate hit rate | ~0% | ~99% |
| Bid rate (real data) | 51.9% | 70.3% |

The dominant stage shifted from `frequency_cap` to `candidate_retrieval` (RoaringBitmap intersections). That's the right architectural rebalancing — when the dominant stage gets fast enough, what *was* a 22% stage now looks like 90% of pipeline time, even though its absolute cost didn't change. The bidder is no longer Redis-network-bound on the hot path.

### Why the fix is correctness-safe

The original cold-miss path threw away the `(day_count, hour_count)` Redis returned, keeping only the boolean `capped` verdict. We extended `CapResult` so the Redis-backed capper plumbs the raw counts through, and the in-process capper inserts them into moka with `max()` semantics — never silently undoes a higher counter that a concurrent flow already wrote. Worst case under contention is "cache lags by 1 impression for ≤ 1 ms" — acceptable for freq-cap.

The full investigation that surfaced this fix (raw `/metrics` snapshots, per-stage attribution, cold-miss-counter analysis) is reproducible with `bash tools/inspect-stages.sh` — it computes per-stage avg µs and pipeline share directly from the bidder's Prometheus histograms. No PromQL or Grafana needed.

---

# Issue 2:

  ── per-stage breakdown (cumulative since process start) ──
```

  Stage                     Avg µs      Count    Sum (s)    Share   Budget over
  ---------------------- --------- ---------- ---------- -------- -------------
  candidate_retrieval        393.2     450443     177.13   90.90%          4526  █████████████████████████████████████████████
  budget_pacing               13.2     450441       5.92    3.04%           998  █
  candidate_limit              9.9     450441       4.46    2.29%            60  █
  frequency_cap                7.6     450441       3.40    1.75%             6  
  ranking                      7.1     310248       2.21    1.13%           391  
  user_enrichment              1.9     450443       0.84    0.43%             4  
  request_validation           0.8     450544       0.36    0.19%             5  
  scoring                      0.7     450441       0.33    0.17%             6  
  response_build               0.7     310248       0.21    0.11%             1  

  ── cache health counters ──

  Counter                                                           Value           Delta
  ------------------------------------------------------- --------------- ---------------
  bidder_freq_cap_in_process_cold_miss_total                        6,922           6,922
  bidder_freq_cap_in_process_fallback_unavailable_total                 1               1
  bidder_redis_hedge_fired                                              2               2

```
  What the data says

candidate_retrieval   avg 393 µs    SHARE 90.9%   over budget 4,526
393 µs avg means roughly half a millisecond per request just doing bitmap intersections. At 15K RPS = 5.9 seconds of CPU spent on this stage every single second. That doesn't fit on one core. The bidder is now CPU-bound on candidate retrieval, not Redis-bound.

Compare to the 10K passing run: candidate_retrieval was 297 µs avg / 22% share. Now it's 393 µs avg / 91% share. Same code, same bitmaps. Why is it 1.3× slower?


=====================================================================
---
## Q1: Is this how the industry does it? Why is it `false` by default?

### Yes, every serious DSP runs an L1 cache like this.

TheTradeDesk, Criteo, AppNexus, Magnite — all of them publish engineering posts describing variants of this pattern (in-process counter cache + write-behind to a shared store). It's not exotic; it's the only way to hit 50K+ RPS without throwing thousands of cores at Redis round-trips.

### But all of them ship one of two safeguards alongside it.

There's a failure mode that breaks the cap contract by definition if you don't address it. Concrete walkthrough:

```
   Two bidder pods (A and B), each with its own moka L1.
   User U has cap = 5 impressions/campaign/day. Already shown 4 today.

   Time   Pod A                              Pod B
   ────   ────────────────────────────────   ────────────────────────────────
   t=0    moka[U] = 4                        moka[U] = 4
   t=10   bid request for U arrives at A
          moka says 4, cap is 5 → OK
          win → ad shown → moka[U]++ = 5
                                             bid request for U arrives at B
                                             moka says 4, cap is 5 → OK
                                             win → ad shown → moka[U]++ = 5

   t=20   ✗ Cap violated.  User U has now seen 6 impressions of this campaign.
          Redis will eventually receive +1 from each pod, settling at 6.
          But the violation already happened on the wire.
```

The two pods independently approved the same impression because each was reading its own L1. The L2 was correct; the L1s were the problem.

Real DSPs solve this with one of three approaches:

| Approach | How it works | Trade-off |
|---|---|---|
| **Sticky routing** | LB consistent-hashes on `user_id` (Envoy `ring_hash`, NGINX `hash $user_id consistent`, ALB target groups). Each user is owned by exactly one pod at any moment. | Most efficient. Requires the LB layer to support consistent-hash routing; pod death triggers re-shuffling and a brief Redis cold-miss for the moved users. |
| **Atomic Lua in Redis** | Every bid runs a `EVAL` script in Redis that atomically reads + checks + increments. No L1 cache. | Cap contract is exact. But you've removed the cache, so you pay the L2 cost on every bid — back where we started. |
| **Eventual consistency, declared in the SLA** | Tell advertisers "we cap *at approximately* N per day, with a 5–10% tolerance window during traffic bursts." | Simplest. Some ad-tech contracts allow this; many don't. |

### Why we don't ship sticky routing

This project is the bidder, not the LB. We don't bundle Envoy/NGINX configs, and a new operator who clones this repo and deploys 3 pods behind a default round-robin LB would silently double-cap users without ever knowing.

So the safer default is `false` — every pod always reads from Redis, no L1 inconsistency between pods. Production deployments turn it on **after explicitly choosing** sticky routing or accepting the eventual-consistency window.

That's what the `.env.example` warning means when it says:

> REQUIRES single-instance deployment OR sticky LB routing per user_id. In a multi-pod fleet without sticky routing, two pods can independently approve impressions for the same (user, campaign) → cap violations.

---

## Q2: How does the cache stay in sync with Redis on writes?

You're right to worry about this. Two writes happen per impression — one to L1, one to L2. They have to agree. Here's exactly how that works in this codebase.

### The impression-write path

When a bid wins, the impression has to be counted. The bid path itself never blocks on this — it enqueues a record and moves on:

```
                bid wins → impression served
                          │
                          ▼
                ┌────────────────────────────┐
                │ Bid handler                │
                │ for each winner: enqueue   │
                └────────┬───────────────────┘
                         │ tokio::sync::mpsc
                         │ (bounded, depth 65 K)
                         ▼
                ┌────────────────────────────┐
                │ Worker pool                │
                │ (drains the channel)       │
                └────────┬───────────────────┘
                         │
                         ▼
                ┌────────────────────────────┐
                │ Two writes — same worker:  │
                │                            │
                │  1. moka L1                │  in-process atomic +1
                │     get_with(user_id)      │
                │       .compute(|map|       │
                │         counter[c]++)      │
                │                            │
                │  2. Redis L2               │  durable +1
                │     INCR v1:fc:…:d         │
                │     INCR v1:fc:…:h         │
                │     EXPIRE …               │
                └────────────────────────────┘
```

Important: **both writes happen in the same worker invocation, sequentially**. Not "L1 first, L2 lazily later." The worker increments moka and writes to Redis as part of one batch operation.

### Why concurrent updates don't tear

The bid-path is reading moka while the worker is writing it. Naively that would be a data race. moka avoids it by holding the counter as an `AtomicI64`, not a plain integer:

- **Worker writes**: `counter.fetch_add(1, Ordering::Relaxed)` — atomic increment
- **Bid path reads**: `counter.load(Ordering::Relaxed)` — atomic load

There's no lock to take, and the read either sees the pre-increment or the post-increment value — never a torn half-write. This is what "lock-free" means in practice for this kind of counter.

If two concurrent worker writes hit the same `(user, campaign)` counter (e.g. user has multiple wins in flight), `fetch_add` makes them serialise at the CPU level — both increments land, no lost update.

### The fast-update example you asked about

Walk through what happens when a single user gets three ads in 100 ms:

```
t=0       bid 1 wins                                  moka[(U,C)] = 0 → 1
t=20ms    bid 2 wins                                  moka[(U,C)] = 1 → 2
t=50ms    bid 3 wins                                  moka[(U,C)] = 2 → 3

t=60ms    new bid request for SAME user
            freq_cap stage reads moka[(U,C)] = 3
            cap is 5/day → not capped → bid OK
            (Redis still shows 0 here — worker hasn't flushed yet)

t=1000ms  worker flushes the queue → Redis INCR × 3
            Redis now has count = 3 for (U, C)
```

Two things to notice:

1. **Reads always see the freshest counter immediately.** The bid path at t=60ms gets count=3, even though Redis hasn't been updated. That's exactly what we want — the L1 is the authoritative read for the current process.

2. **Redis converges within `IN_PROCESS_FLUSH_INTERVAL_MS`** (default 1 s). During that window, *other bidder processes* would see the older Redis value. With sticky routing this doesn't matter (only one pod owns this user). Without sticky routing, this is the eventual-consistency window we discussed in Q1.

### Edge cases and what protects against them

| Scenario | What goes wrong | Protection |
|---|---|---|
| Write-queue overflows under burst load | Some increments are dropped before reaching the worker | Fail-loud counter `bidder.freq_cap.in_process.write_drops_total`. Tune `WRITE_BUFFER_SIZE` up if it's non-zero. |
| moka evicts a user's entry under capacity pressure | The counter would be lost — Redis only has what was flushed before the eviction | **Eviction listener** flushes the user's counters to Redis *before* moka drops the entry. Phase 8 invariant. |
| Bidder process crashes | In-flight increments still in the channel are gone | Redis has everything that was flushed up to the last drain (≤ `FLUSH_INTERVAL_MS` ago). On restart, moka rebuilds from cold reads against Redis. |
| moka and Redis disagree on a count | L1 has stale data after a crash; L2 was updated by another pod that's since died | First read for that user in the new process is a cold-miss → fetches Redis truth → moka is now correct. Subsequent reads serve from moka. The lag is bounded by how often each user gets touched. |
| Cache size cap hit (`IN_PROCESS_CAP_CAPACITY`) | Active users get evicted to make room for new ones | TinyLFU keeps the *valuable* entries (frequency × recency); idle users get dropped first. Eviction listener flushes their counters first so we never silently lose data. |
| Multiple bidder pods, no sticky routing | Each pod's L1 is independent → cap violations | No technical mitigation — this is the contract limitation. Don't enable in this deployment shape. |

### The honest trade-off

The L1 cache trades **read latency** (huge win, ~2000×) for two things:

1. **A 1-second eventual-consistency window** between L1 and L2. Acceptable for freq-cap because over-counting by a few impressions during a flush window is a much smaller harm than missing 50% of bids because Redis is too slow.
2. **Multi-pod cap correctness** — only true with sticky routing or single-instance deploy. Otherwise you accept a small percentage of over-cap.

Both of these are defensible production trade-offs, and they're exactly the conversations DSP architects have when adopting this pattern. The bidder code is correct; the deployment shape determines whether that correctness translates to a correct cap contract end-to-end.

---

## Operator-facing summary

| | Default (`IN_PROCESS_ENABLED=false`) | Enabled (`IN_PROCESS_ENABLED=true`) |
|---|---|---|
| **Per-bid freq-cap cost** | ~1 ms (Redis MGET round-trip) | ~0.5 µs (moka L1 hit) on warm users |
| **Multi-pod safe?** | Yes — Redis is the single source of truth | Only with sticky routing OR single instance |
| **Failure mode** | Slower at high RPS, but always correct | At 50K+ RPS, extremely fast — but multi-pod without sticky routing → cap violations |
| **When to use** | Default. Works in any deployment shape. | Only after you've decided how you'll route users to pods. |

If you're running a single bidder process, or your LB does consistent-hash on `user_id`, or your SLA tolerates approximate caps — turn it on. Otherwise, leave it off.

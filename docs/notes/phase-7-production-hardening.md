# Phase 7 — Production hardening + multi-exchange

Phase 7 closes the gap between "Phase 6.5 baseline runs cleanly under k6" and "this could plausibly accept live SSP traffic on a Linux fleet." The work splits into three buckets:

1. **Adaptive resilience.** Hedge feedback loop (load_shed_rate + redis_p95) wired end-to-end so the values that already had setters in Phase 5 actually move under load. Kafka `incident_mode` auto-flip from base policy to `random_sample` when sustained drop rate breaches threshold, with auto-revert.
2. **Multi-exchange seam.** `ExchangeAdapter` trait extracted from the bid handler, with `OpenRtbGenericAdapter` (default JSON) and `GoogleAdxAdapter` (binary protobuf) impls. Per-SSP HMAC secrets keyed by exchange id so one bidder process can serve multiple SSPs without a single shared secret.
3. **Operational surface.** Per-request ML pCTR surfaced in `Bid.ext` for downstream analytics. Optional `InProcessFrequencyCapper` to address the freq-cap MGET breaker-skip rate observed in the Phase 6.5 baseline. Prometheus + Grafana wired into `docker-compose.yml` with an auto-provisioned dashboard.

What's deferred (PLAN.md § Phase 7 calls these out as experiments — they need profiling justification we don't have):

- `bidder-server-monoio` (separate workspace member, io_uring, Linux-only). Stays as an experiment until profiling on multi-process Tokio + `SO_REUSEPORT` shows the syscall path is the limiter.
- `bumpalo` arena (`--features allocator-arena`). Phase 4–6 profiling did not show allocator contention as a top hot spot under jemalloc, so the arena stays off.
- Magnite + Index Exchange adapters. PLAN.md asks for "at least one alternate format"; GoogleAdx satisfies that.
- Helm chart implementation. The design lives in `docs/DEPLOYMENT.md` § 10; the chart itself is Phase 8.
- Per-RPS comparison matrix across all configurations. The Phase 7 baseline re-run extends `docs/LOAD-TEST-RESULTS-rust-v0-baseline.md` rather than authoring a new per-config document.

---

## 1. Hedge feedback loop

**Problem.** Phase 5 introduced `HedgeBudget::set_load_shed_rate()` and `RedisHedgeState::update_p95()` so the hedge trigger could adapt to actual conditions, but nothing called either method. The hedge trigger sat at the 8 ms floor regardless of real Redis p95, and the budget never contracted under load.

**Fix.** New module `bidder-core/src/hedge_feedback.rs` exposes two trackers fed from where the data actually lives:

- `LoadShedTracker` — accumulates `record_request()` and `record_shed()` from the HTTP timeout middleware (`bidder-server/src/server/routes.rs`). Every request increments `requests_total`; every 503 (concurrency-limit rejection or middleware timeout) increments `shed_total`.
- `RedisLatencyTracker` — accumulates `record(Duration)` calls from `RedisSegmentRepo::segments_for` and `RedisFrequencyCapper::check`, so the same Redis dependency the hedge budget protects is the one the p95 estimate is derived from.

`spawn_feedback_loop()` runs every 1 s, drains both trackers delta-style, and pushes:

- `load_shed_rate = shed_delta / requests_delta` into `HedgeBudget::set_load_shed_rate`. Budget contracts 10% → 2% → 0% as the HTTP layer sheds more.
- `redis_p95 ≈ mean × 1.7` into `RedisHedgeState::update_p95`. Coarse but cheap; a true streaming p95 estimator (TDigest) is overkill at 1 Hz tick.

The loop emits `bidder.hedge.load_shed_rate` and `bidder.hedge.redis_p95_ms` gauges so the Grafana dashboard shows the inputs the budget is reacting to.

**Why mean × 1.7 for p95.** Empirically, Redis call-time distributions look log-normal-ish; for a log-normal with σ≈0.5–0.7 (typical for healthy Redis), p95 ≈ 1.6–1.8 × mean. Good enough to drive a hedge trigger floor; not good enough to alert on. The TDigest path is documented as future work.

---

## 2. Kafka `incident_mode` monitoring loop

**Problem.** PLAN.md spec'd four drop policies (`newest`, `oldest`, `random_sample`, `incident_mode`) but `incident_mode` had no implementation. Phase 5 left a TODO that the policy field existed in config but wasn't auto-flipped.

**Fix.** New module `bidder-core/src/kafka_incident.rs` plus a small change to `KafkaEventPublisher`:

- `KafkaIncidentState` holds an `AtomicU8` effective policy (Newest/Oldest/RandomSample). The publisher reads it on every `publish()` call. When the policy is `RandomSample`, the publisher drops 9 of every 10 events upstream of rdkafka's queue (counter-based sampling — deterministic and lock-free; no `rand` crate added). The dropped events still increment `events_dropped_total` with `reason="incident_sample"` so they're visible in metrics.
- `spawn_monitor()` ticks every 30 s, computes the rolling drop rate over the last interval (`drop_delta / (publish_delta + drop_delta)`), and:
  - Flips effective policy from base → `RandomSample` after **10 consecutive ticks above 1%** (= 5 minutes, matching PLAN.md's spec).
  - Reverts to base after 10 consecutive ticks below the threshold.
- Auto-flip is gated on `cfg.kafka.drop_policy = "incident_mode"`. Static policies (`newest`, `oldest`, `random_sample`) skip the flip logic and only emit the gauges (`bidder.kafka.drop_rate`, `bidder.kafka.incident_mode_active`).

**Why counter-based sampling over rand.** Three reasons: (1) we already manage atomics elsewhere; adding `rand` for a single 1-in-10 selector is dependency creep, (2) the deterministic stride is uniform under high RPS and predictable under low RPS, (3) testing is trivial.

**Why a 30 s tick × 10 dwell ≈ 5 min.** Faster ticks oscillate when the upstream is on the edge; slower ticks miss real incidents. 5 min is the same dwell PLAN.md called out.

---

## 3. ExchangeAdapter + GoogleAdx scaffolding

**Trait.** `bidder-core/src/exchange/mod.rs` defines `ExchangeAdapter`:

```rust
fn id(&self) -> ExchangeId;
fn decode_request(&self, body: &mut Vec<u8>) -> Result<BidRequest>;
fn encode_response(&self, response: &BidResponse) -> Result<(Vec<u8>, ResponseContentType)>;
```

The mutable `body` is non-negotiable for `simd-json` (PLAN.md Stack Decisions § "External wire format"); adapters that don't need mutation just ignore it.

**OpenRtbGenericAdapter** is the JSON path lifted out of the bid handler verbatim. simd-json for decode, serde_json for encode, `application/json` content-type. No behaviour change vs Phase 6.5.

**GoogleAdxAdapter** is new. AdX speaks a Google-specific protobuf over `application/octet-stream`. The repo can't ship the upstream `realtime-bidding.proto` (license + size), and the project rule is "everything generated here, no external dependency," so:

- `bidder-protos/proto/adx.proto` is a slim in-repo schema. Field tags + names mirror the public AdX schema closely enough that swapping in the upstream proto later is a mechanical rename.
- The build script generates `bidder_protos::adx` types via `prost`.
- The adapter decodes protobuf → internal OpenRTB `BidRequest` (single imp from `adslot`, `user.id` from `google_user_id`, site/device/geo). The mapping reaches the internal model via `serde_json::from_value(json!({...}))` so we don't enumerate every Optional OpenRTB field — serde fills the rest from `#[serde(default)]`. Acceptable for scaffolding; once AdX traffic flows, the hot path mapping should be inlined to drop the JSON round-trip.
- Encode goes the other way: internal `BidResponse` → `adx::BidResponse` with one `Ad` per winning bid. `max_cpm_micros = bid.price * 1_000_000`, `nurl` carried through, `ext_json` preserves `bidder_pctr` so downstream analytics see scoring confidence per response.

**What this adapter does NOT do (explicit scope cut):**
- Hyperlocal targeting decryption (needs partnership-issued AES keys).
- Cookie matching / Google user-list reconciliation.
- Signed nurl macro substitution (Google supplies the macros at integration time).

The adapter is wire-compatible, not production-ready against live AdX traffic. The README on the GoogleAdx module documents this loudly.

---

## 4. Per-SSP HMAC secrets

**Problem.** Phase 5 assumed a single shared HMAC secret for win-notice authentication. Production has multiple SSPs and rotating one shouldn't require re-signing for the others.

**Fix.** `WinNoticeConfig` gains `secrets: HashMap<String, String>` keyed by exchange id (the same `ExchangeAdapter::id()` we already had). Lookup falls back to the default `secret` when the exchange has no per-SSP entry.

The win-notice flow now threads `exchange_id` through three places:
- `WinNoticeRequest` (so the URL builder picks the right secret to sign with).
- `ResponseBuildStage` (holds `exchange_id: Arc<str>` derived from the route's adapter at startup; constant per pipeline instance).
- The win endpoint's `WinParams` (SSP echoes back the `exchange_id` query param so the verifier picks the same secret).

The nurl emitted by the bidder now includes `&exchange_id=<id>` so the loop closes without an out-of-band protocol negotiation. SSPs that never look at the URL just pass it back verbatim. SSPs that strip unknown params verify under empty `exchange_id` and fall back to the default secret — same behaviour as Phase 6.5.

A new test (`per_ssp_secret_overrides_default`) asserts that signing under one exchange's secret does not verify under another, and that empty exchange id falls back to the default.

---

## 5. ML pCTR in Bid.ext

`ResponseBuildStage` now sets `Bid.ext = {"bidder_pctr": w.score, "bidder_score": w.score}` on every winning bid. Both fields are always present (never null) so analytics consumers don't branch on missing keys.

For the AdX adapter this propagates through `Ad.ext_json`. For OpenRTB it lands in the standard `bid.ext`. Schema is locked for the lifetime of this PR; adding more scoring features into ext is a follow-up doc-then-merge.

---

## 6. InProcessFrequencyCapper

The Phase 6.5 baseline showed `bidder.freq_cap.skipped{reason=breaker_open}` firing on ~37% of requests at 10K RPS — the freq-cap MGET round-trip was the dominant Redis pressure point. `InProcessFrequencyCapper` (off by default; `[freq_cap] in_process_enabled = true`) layers a `DashMap<UserId, AtomicCounter>` cache over the existing `RedisFrequencyCapper`:

- Read path: warm hits skip the MGET entirely. Cold misses fall through to the Redis-backed implementation; the result populates the DashMap.
- Write path: every `record_impression` updates the DashMap inline and queues a `WriteBehindOp` to a bounded mpsc channel. A drain task forwards the ops to the existing `ImpressionRecorder` (which already handles Redis EVAL with TTL), so there is exactly one path for Redis writes.
- The wrapper is **single-instance authoritative**. With multiple bidders behind an LB, the in-process counters diverge until the next Redis read. This is acceptable for an LB with user-stickiness or for short cap windows where transient over-count is dwarfed by the daily/hourly bucket.

The startup log warns when `in_process_enabled = true` so the trade-off isn't silent.

---

## 7. Prometheus + Grafana

Mid-phase the user pointed out that the existing observability story — bidder exports metrics, you scrape them somehow — was insufficient for "actually look at the system under load." Added to `docker-compose.yml`:

- Prometheus scraping `host.docker.internal:9090` every 5 s (matches the bidder's metrics export cadence).
- Grafana auto-provisioned with the Prometheus data source and a "Bidder — Live" dashboard at `/d/bidder-live`. Panels: bid RPS, bid rate, error rate, pipeline early drops, per-result outcomes, per-stage p99, breaker open/close events, hedge fired/blocked, freq-cap skips by reason, budget-exhausted filtered, kafka_events_dropped, win notices breakdown.

Linux Docker hosts get an `extra_hosts: host.docker.internal:host-gateway` mapping so the same compose file works on macOS dev and Linux CI.

---

## 8. End-of-phase checklist

- [x] `cargo build --workspace --all-features`
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [x] `cargo fmt --check`
- [x] `cargo test --workspace --features test-helpers` — 90+ tests pass (added: per-SSP HMAC, hedge feedback trackers, kafka incident state, GoogleAdx round-trip, InProcess freq cap)
- [ ] Phase 7 baseline re-run vs Phase 6.5 — pending; user will run after PR merges.

The baseline doc gets a Phase 7 column appended once the re-run completes; new metrics to capture are `bidder.hedge.load_shed_rate`, `bidder.hedge.redis_p95_ms`, and `bidder.kafka.incident_mode_active`.

---

## 9. What changed that affects callers

- `WinNoticeGateService::sign(message)` → `sign(message, exchange_id)`.
- `WinNoticeGateService::check(...)` gains a trailing `exchange_id: &str` argument.
- `WinNoticeRequest` gains `exchange_id: &'a str`.
- `ResponseBuildStage` gains `exchange_id: Arc<str>` field.
- `KafkaEventPublisher::new(cfg)` → `new(cfg, incident: Arc<KafkaIncidentState>)`.
- `RedisSegmentRepo::new` and `RedisFrequencyCapper::new` each gain a trailing `Option<Arc<RedisLatencyTracker>>` (None preserves previous behaviour; Some feeds the hedge feedback loop).
- `WinNoticeConfig` gains `secrets: HashMap<String, String>` (defaults to empty; behaviour unchanged when omitted).

All of these are additive at the type-system level and behaviour-compatible when the new fields are omitted/empty.

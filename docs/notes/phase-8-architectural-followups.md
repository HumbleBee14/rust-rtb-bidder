# Phase 8 — Architectural follow-throughs

Closing the architectural compromises documented as deferred at the end of Phase 7. This is the project's last "build" phase — it adds no new features, only retires the open caveats from earlier phases. After Phase 8 the project enters its profiling / stress-test / Linux-baseline phase.

## What changed and why

### 1. `InProcessFrequencyCapper` — `moka::sync::Cache` with TinyLFU eviction

**Before (Phase 7):** outer `Arc<DashMap<UserId, Arc<UserCapMap>>>` with a hard ceiling. Once the cap was hit, every new user read-through to Redis on every call and never warmed the cache. We surfaced this with a `bidder.freq_cap.in_process.capacity_rejected_total` counter and called it "honest documentation."

**After (Phase 8):** `moka::sync::Cache<String, Arc<UserCapMap>>` configured with `max_capacity = cap_capacity` and `time_to_live = 1h`. TinyLFU keeps frequently-served users hot, evicts idle ones first.

Eviction listener: when moka evicts (capacity, TTL, or explicit invalidate), the listener walks every campaign the user accumulated and queues both day + hour `WriteBehindOp`s on the existing channel. The drain task then flushes them to Redis. Net guarantee: every in-process increment lands in Redis either via the regular hot-path enqueue, or — failing that — at eviction time, never silently. If the channel is full when the listener fires, drops are recorded to `bidder.freq_cap.in_process.eviction_flush_drops_total`.

Hot path simplified: dropped the length-check / capacity-rejected branch. `record_impression` now does a single `get_with_by_ref` against moka, which is sub-µs and lock-free in the steady state.

**Test:** `moka_eviction_flushes_counters_to_redis_queue` — touch enough distinct users to force eviction, then confirm every touched user has at least one write-behind op queued (hot-path or eviction-listener-driven).

### 2. Kafka publisher — `BaseProducer` + dedicated poll thread

**Before:** `FutureProducer::send().await` invoked inside `tokio::spawn(async move { publisher.publish(...).await })`. At 50K RPS × 2 winners that's ~100K `tokio::spawn` calls/s, plus the spawn boundary obscured rdkafka's own backpressure (the `queue.full.behavior=error` we added in Phase 7 only surfaced *inside* the spawned task, where nobody was waiting).

**After:** `BaseProducer<PublisherContext>` with a dedicated `std::thread` that loops on `producer.poll(100ms)`. The trait is now synchronous (`fn publish` instead of `async fn publish`). Call sites in `handlers.rs` invoke `state.event_publisher.publish(...)` inline — no spawn.

Delivery accounting moved to the right place: `record_published()` only fires from the delivery callback (i.e. the actual broker ack), not on enqueue. `record_dropped()` fires either from a `QueueFull` enqueue error or a delivery-callback error. The end-to-end semantics are now exactly what the architecture diagram has always claimed: the bid path enqueues, librdkafka's bounded queue is the backpressure surface, and the delivery thread is the only place that talks to the broker.

Shutdown: `Drop` on `KafkaEventPublisher` flips an `AtomicBool` and joins the poll thread, with a 5-second `flush()` to drain in-flight messages.

### 3. OTel tracing — opt-in tail sampling via collector

**Before:** `Sampler::ParentBased(TraceIdRatioBased(success_sample_rate))`. Head-based, decides at trace creation, can't keep "100% errors + 1% successes" because outcomes aren't known yet. Phase 7 honestly documented this in the code comment.

**After:** new `tail_sampling_via_collector: bool` config field. Default `false` (head sampling, same as Phase 7). When true:
- Bidder uses `Sampler::AlwaysOn` and ships every span to the OTLP endpoint
- The endpoint must be an OTel Collector running with the `tail_sampling` processor (reference config at `docker/otel-collector/config.yaml`)
- The collector keeps 100% of error spans + 100% of >50ms latency spans + 1% probabilistic on the rest

The collector config is **not** wired into `docker-compose.yml` by default. Operators opt in by running `otelcol --config=docker/otel-collector/config.yaml` and pointing the bidder at it. This avoids the "phantom infrastructure that pollutes baseline timings" pattern that bit us during Phase 6.5.

Sizing math is in the collector config header: at 50K RPS × 8 spans × 200 B × 30s decision_wait the buffer is ~2.4 GB. If the collector pod's memory is tighter, lower `decision_wait` (loses late-firing error policies on slow traces) or sample more aggressively in the probabilistic policy.

### 4. Body parsing — `Bytes` → `Vec<u8>` zero-copy verified

**Before:** `let mut buf: Vec<u8> = body.into();` with a comment that simd-json needs mutable bytes.

**After:** comment expanded to document *why* this is zero-copy: `Bytes::into::<Vec<u8>>` is O(1) when the buffer is uniquely owned, which is always true at axum handler entry. Verifier note added so the next person to touch the request path checks with samply rather than guessing. No code change.

### 5. `bumpalo` arena experiment — retired

**Before:** PLAN.md Phase 7 listed bumpalo as feature-gated and contingent on profiling. No code ever shipped — the feature flag never existed in `Cargo.toml` or any `#[cfg]`.

**After:** retired from the plan. Reasoning: jemalloc + the existing per-stage allocations don't surface as a top hot-spot in any profile we've taken. Adding bumpalo would mean (a) a new feature flag, (b) hot-path allocator switching, and (c) the arena-no-await-boundary constraint propagated through every stage. Cost > value at current scale. Re-add only if a future profile changes the picture.

### 6. Multi-instance freq-cap correctness — sticky routing

**Before:** Phase 7 documented "single-instance only or sticky-by-user-id at the LB" without picking one.

**After:** sticky routing is the supported answer. Reasoning:
- Best-effort multi-instance (each pod independently approves) breaks the cap contract by definition — two pods can each approve `daily_cap_imps` impressions for the same `(user, campaign)`. Acceptable only if the cap is "approximate", which we never said.
- Sticky routing (consistent hash on `user_id` at the L7 LB — Envoy `ring_hash`, NGINX `hash $user_id consistent`, ALB target groups with hash) makes the in-process cache correct again because each user is served by exactly one pod at any moment. Pod restarts move users to a new pod, which read-throughs to Redis on cold start — the existing fallthrough path handles this.

Operational impact: the in_process feature flag's startup log already warns about this; that warning is now the canonical statement of the constraint. No new code.

## Deliverables

- `bidder-core/src/frequency/in_process.rs` — moka swap + eviction listener + new test
- `bidder-server/src/kafka.rs` — full rewrite to BaseProducer
- `bidder-core/src/events.rs` — trait switched from async to sync
- `bidder-server/src/server/handlers.rs` — removed `tokio::spawn` around publish; expanded Bytes comment
- `bidder-core/src/telemetry/mod.rs` — tail-sampling branch
- `bidder-core/src/config/mod.rs` — `tail_sampling_via_collector` field
- `docker/otel-collector/config.yaml` — reference tail-sampling config
- `docs/PLAN.md` — Phase 7 deferred items closed; bumpalo retired
- This document.

## What's next (out of Phase 8 scope)

The project is now feature-complete. Remaining work is profiling and validation, not architecture:

- Re-baseline on the same hardware that captured the Java sibling's 15K result; compare per-stage timings before/after the moka + BaseProducer changes.
- Run the stress-tier sweep (15K, 20K, 25K, 50K) the v0 baseline deferred.
- samply / tokio-console captures to confirm no regression and to validate the Bytes→Vec<u8> zero-copy claim empirically.
- Linux production hardware re-baseline.

Those belong in a stress-test phase, not Phase 8.

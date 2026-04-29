# External services — what they are, why we use them, how the bidder talks to each

This bidder talks to several external services. They sound similar (postgres / redis / kafka / clickhouse / prometheus / grafana) but each does a fundamentally different job. This doc explains each one *in the context of this codebase* — what we use it for, where in the code it shows up, and how they're connected.

If you're new to one or more of these, read top-to-bottom. If you already know what each is, skip to the per-service sections for the bidder-specific usage.

---

## The big picture

```
                           ┌──────────────┐
                           │   Postgres   │  campaigns, segments,
                           │  (catalog)   │  targeting tables
                           └──────┬───────┘
                                  │ load on boot
                                  │ refresh every 60s
                                  │ (off the hot path)
                                  ▼
                           ┌──────────────┐
                           │   Bidder     │
                  hot path │              │ hot path
   ┌──────────────────────►│  axum + tokio├────────────────►┌───────────┐
   │ bid request           │              │ per request     │   Redis   │
   │                       │              │  ◄─────────────►│ (segments,│
   │                       │              │  GET / MGET /   │ freq cap) │
   ▼                       │              │  HMAC dedup     └───────────┘
 client                    │              │
   ▲                       │              │ publish (async)
   │ 200 (bid) /            │              ├────────────────►┌───────────┐
   │ 204 (no bid)          │              │ BidEvent /      │   Kafka   │
   └───────────────────────┤              │ WinEvent        │  (events) │
                           │              │                 └─────┬─────┘
                           │              │                       │
                           │              │ scrape /metrics       │ consumer
                           │              │  every 15s            │  pipeline
                           │              ▼                       ▼
                           └─────► ┌────────────┐          ┌─────────────┐
                                   │ Prometheus │          │ ClickHouse  │
                                   │ (metrics)  │          │ (analytics) │
                                   └─────┬──────┘          └─────┬───────┘
                                         │                       │
                                         ▼                       ▼
                                   ┌────────────┐          ┌──────────────┐
                                   │  Grafana   │          │  SQL reports │
                                   │(dashboards)│          │  (BI / billing)│
                                   └────────────┘          └──────────────┘
```

Three classes of dependencies:

1. **Hot path** (every request, latency-critical, < 1 ms budget): Redis.
2. **Cold path** (startup + 60s refresh, off the bid loop): Postgres.
3. **Sidecar streams** (fire-and-forget, observability + analytics): Kafka, Prometheus, Grafana, ClickHouse.

The bidder must get hot-path right for SLA. Everything else is buffered or asynchronous so dependency slowness can't propagate to bid latency.

---

## Quick distinction table

| Tool | Question it answers | Granularity | Time horizon | Used by |
|---|---|---|---|---|
| **Postgres** | What campaigns exist? Who do they target? | One row per campaign | Long-term truth | Bidder catalog loader |
| **Redis** | What segments does user X have? Has campaign Y already been served to user X? | One key per user / per cap | Real-time, hot data only | Bidder hot path |
| **Kafka** | What did the bidder actually do? | One event per bid / win | Days (replayable) | Downstream consumers |
| **Prometheus** | How is the bidder behaving right now? | Counters + histograms | Last 15 days, 15s resolution | Operators (via Grafana) |
| **Grafana** | Show me the bidder's dashboards | Visual layer | — | Operators |
| **ClickHouse** | What's our total spend by campaign yesterday? | Aggregated event tables | Years, SQL-queryable | Analysts, billing |

---

## 1. Postgres — campaign catalog

**Role:** the source-of-truth for "what campaigns exist, what do they target, what budgets do they have." Static-ish data — changes once per minute at most.

**Bidder access pattern:** read-heavy, completely off the hot path.

- At startup, the bidder runs `tokio::try_join!` of 8 SELECTs (campaigns, creatives, segment-targeting, geo, device, format, daypart, budgets) — see [bidder-core/src/catalog/loader.rs](bidder-core/src/catalog/loader.rs).
- Every 60s a background task re-runs the same SELECTs. If the result is different from the in-memory snapshot, it builds a new `CampaignCatalog` and atomically swaps it via `ArcSwap`.
- In-flight requests holding the old `Arc` keep reading the old snapshot until they finish; new requests see the new snapshot. Zero coordination required.

**Why we don't query Postgres per request:** at 50K RPS, even a 1ms Postgres round-trip would dominate the 50ms SLA. We trade staleness (up to 60s) for predictable latency.

**Schema** (simplified — see [docs/POSTGRES-SCHEMA.md](POSTGRES-SCHEMA.md)):

```sql
campaign(id, advertiser_id, daily_budget_cents, status, ...)
creative(id, campaign_id, ad_format, w, h, ...)
campaign_targeting_segment(campaign_id, segment_id)
campaign_targeting_geo(campaign_id, geo_kind, geo_code)
campaign_targeting_device(campaign_id, device_type)
campaign_targeting_format(campaign_id, ad_format)
campaign_daypart(campaign_id, hours_active)
segment(id, name, category)
```

**Code reference:**

```rust
// bidder-core/src/catalog/loader.rs (sketch)
let (campaigns, creatives, seg, geo, dev, fmt, daypart, budgets) = tokio::try_join!(
    fetch_campaigns(&pool),
    fetch_creatives(&pool),
    fetch_segment_targeting(&pool),
    // ...
)?;
let new_catalog = build_catalog(campaigns, creatives, seg, geo, dev, fmt, daypart);
catalog_arc.store(Arc::new(new_catalog));   // atomic swap
```

**Why postgres specifically:** SQL relational shape fits the targeting schema naturally; we get `JOIN` and `IN` for free; sqlx gives us compile-time-checked queries.

---

## 2. Redis — hot-path KV store

**Role:** the data the bidder reads on **every** request, plus the small bookkeeping state it writes after each request. Performance-critical.

**Bidder access patterns:**

| Operation | Key | When |
|---|---|---|
| `GET v1:seg:{u:<userId>}` | per user | Every request — fetch user's segments |
| `MGET v1:fc:{u:<userId>}:c:<id>:[h\|d]` | per (user, campaign, hour/day) | Frequency-cap stage |
| `SET NX EX v1:winnotice:dedup:<reqid>` | per win notice | Win-notice handler dedup |
| `SET v1:warm:users` | global | Optional pre-warm list (single blob of user IDs) |

The full key contract is in [docs/REDIS-KEYS.md](REDIS-KEYS.md). The `{u:<userId>}` braces are **Redis Cluster hash-tags** — they force keys for the same user to land on the same shard so a freq-cap MGET stays fast.

**Layered cache (important):** Redis is the hot store, but in front of it sits an **in-process cache** (`moka`):

```
bid request → moka (sub-µs) ──HIT──> serve
                  │
                  └──MISS──> Redis (~1ms) → store in moka → serve
```

After ~30 seconds of traffic, the working set of hot users lives in moka and 90%+ of requests skip Redis entirely. moka is *inside* the bidder process, not a separate service.

**Code reference:**

```rust
// bidder-server/src/segment_repo.rs (sketch)
let key = format!("v1:seg:{{u:{user_id}}}");
let bytes: Option<Bytes> = redis.get(&key).await?;
// raw little-endian u32 segment IDs, packed
let segments: Vec<u32> = bytes.unwrap_or_default()
    .chunks_exact(4)
    .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    .collect();
```

**What's stored** (raw bytes, not strings):

- `v1:seg:{u:42}` → `[0x01,0x00,0x00,0x00, 0x07,0x00,0x00,0x00, ...]` (u32 segment IDs, packed)
- `v1:fc:{u:42}:c:7:d` → `"3"` (counter — user 42 was shown campaign 7's creative 3 times today)

**Why redis specifically:** sub-millisecond reads, atomic counter ops (`INCR`), built-in TTL, hash-tag sharding for cluster mode. The whole hot path is read-by-key — Postgres would be 100× slower.

---

## 3. Kafka — event stream (the bidder's "outbox")

**Role:** every business action the bidder takes (every winning bid, every win notice received) is written to Kafka as an event. Downstream services consume the events for billing, attribution, ML training, audit, replay.

**Important distinction:** Kafka does **not** receive metrics. It receives **events** — full records of one thing happening. Metrics go to Prometheus; events go to Kafka.

**Bidder access pattern:** synchronous enqueue, asynchronous delivery.

- Bid handler builds an `AdEvent { Bid { request_id, campaign_id, ..., bid_price_micros } }` and calls `event_publisher.publish(...)`. The call returns in microseconds — it just pushes the event onto rdkafka's bounded internal queue.
- A dedicated OS thread (`kafka-poll`, see [bidder-server/src/kafka.rs](../bidder-server/src/kafka.rs)) drains delivery callbacks from librdkafka. The bid path never blocks on broker I/O.
- If the queue is full (broker slow / down), `publish()` records `bidder.kafka.events_dropped` in Prometheus and moves on. The bid response still goes out within SLA.

**Code reference (from real handlers.rs):**

```rust
// bidder-server/src/server/handlers.rs:148-166
for winner in &ctx.winners {
    let event = AdEvent {
        body: Some(ad_event::Body::Bid(BidEvent {
            request_id: request_id.clone(),
            campaign_id: winner.campaign_id,
            creative_id: winner.creative_id,
            bid_price_micros: winner.bid_price_cents as i64 * 10_000,
            user_id: user_id.clone(),
            // ...
        })),
    };
    let key = winner.campaign_id.to_le_bytes();
    state.event_publisher.publish(&topic, &key, event);  // synchronous, ~µs
}
```

**Example event** (deserialised):

```json
{
  "request_id": "abc-123",
  "imp_id": "imp-1",
  "campaign_id": 7,
  "creative_id": 200,
  "bid_price_micros": 1500000,
  "timestamp_ms": 1714370000000,
  "user_id": "user-42",
  "device_type": 2
}
```

**What's NOT in Kafka:**
- Latency measurements (those are Prometheus histograms)
- Counter rates (Prometheus)
- Process-level state (Prometheus gauges)

**Current state in this repo:** the **code is fully wired** — `KafkaEventPublisher` (Phase 8 BaseProducer rewrite) handles this. The compose file does not run a broker by default; the baseline harness sets `BIDDER__KAFKA__BROKERS=""` so the publisher falls back to `NoOpEventPublisher`. Adding a broker to compose is a 10-line change.

**Why kafka specifically:** ordered append-only log with multiple consumers, per-partition ordering by key (we partition by `campaign_id` so all events for one campaign land together), durable storage with configurable retention.

---

## 4. Prometheus — metrics scraper

**Role:** every 15 seconds, Prometheus does an HTTP `GET /metrics` against the bidder. The bidder responds with all its current counters, histograms, and gauges. Prometheus stores them as time-series for ~15 days.

**Bidder access pattern:** the bidder doesn't talk to Prometheus directly. It just *exposes* a `/metrics` endpoint. Prometheus pulls.

```
Prometheus ──GET /metrics──▶ bidder
            ◀──text──────── (key-value pairs of current counters/histograms)
```

**Three metric types** (with real examples from this codebase):

```rust
// Counter — monotonically increasing
metrics::counter!("bidder.bid.requests_total", "result" => "bid").increment(1);
metrics::counter!("bidder.kafka.events_dropped", "reason" => "queue_full").increment(1);

// Histogram — distribution (used for p50/p95/p99 calculations)
metrics::histogram!("bidder.pipeline.stage.duration_seconds", "stage" => name)
    .record(elapsed.as_secs_f64());

// Gauge — point-in-time value, can go up or down
metrics::gauge!("bidder.hedge.redis_p95_ms").set(p95_ms as f64);
metrics::gauge!("bidder.hedge.feedback_tick_drift_seconds").set(drift);
```

**What the `/metrics` endpoint actually returns** (excerpt):

```
# HELP bidder_bid_requests_total Total bid requests by outcome
# TYPE bidder_bid_requests_total counter
bidder_bid_requests_total{result="bid"} 543210
bidder_bid_requests_total{result="no_bid"} 234567
bidder_bid_requests_total{result="parse_error"} 12

# HELP bidder_pipeline_stage_duration_seconds Per-stage latency
# TYPE bidder_pipeline_stage_duration_seconds histogram
bidder_pipeline_stage_duration_seconds_bucket{stage="frequency_cap",le="0.005"} 432101
bidder_pipeline_stage_duration_seconds_bucket{stage="frequency_cap",le="0.01"} 543200
...
```

**Querying** is via PromQL (Prometheus Query Language) — not SQL:

```promql
# Bid rate over the last 5 minutes
rate(bidder_bid_requests_total{result="bid"}[5m])
  / rate(bidder_bid_requests_total[5m])

# p99 latency of the frequency_cap stage
histogram_quantile(0.99,
  rate(bidder_pipeline_stage_duration_seconds_bucket{stage="frequency_cap"}[5m]))

# Alert if p99 > 50ms for 5 minutes straight
histogram_quantile(0.99, rate(bidder_bid_duration_seconds_bucket[5m])) > 0.050
```

**Why prometheus specifically:** designed for high-cardinality counters and histograms; pull-based (collector decides when to scrape, never overwhelms producer); native histogram quantile aggregation; standard alerting rule language; integrates with Grafana out of the box.

---

## 5. Grafana — dashboards on top of Prometheus

**Role:** the visual layer. Operators look at Grafana dashboards 99% of the time; nobody types raw PromQL queries day-to-day.

**Connection model:**

```
   ┌────────────┐    PromQL query    ┌────────────┐
   │  Grafana   ├───────────────────►│ Prometheus │
   │ dashboards │◄───────time-series─┤            │
   └────────────┘                    └────────────┘
```

Grafana doesn't store anything itself. It runs PromQL queries against Prometheus and renders the results as graphs, gauges, heatmaps, alerts, etc.

**This project ships a dashboard:** [docker/grafana/dashboards/bidder.json](../docker/grafana/dashboards/bidder.json) — auto-provisioned when you `make infra-up`. 13 panels covering bid path RPS, bid rate, per-stage p99, circuit breaker state, hedge fired/blocked, freq-cap skips, budget exhaustion.

**You access it at** `http://localhost:3000` (default `admin/admin`) once the stack is up.

**Why grafana specifically:** the standard. Anything else looks like 2010.

---

## 6. ClickHouse — analytics warehouse (NOT in this project's compose)

**Role:** when someone needs to ask "what's our total spend by campaign yesterday?" or "show me the top 10 advertisers by impressions over the last quarter," that query runs against ClickHouse, not Prometheus or Kafka.

**Why not Prometheus?** Prometheus stores aggregates (counters), not individual events. You can't ask "show me bid prices for campaign 7" — Prometheus has no concept of individual bids; it just has the *count* and the *histogram*.

**Why not Kafka?** Kafka is a log; you can read events sequentially but you can't query them with SQL. To answer "sum bid_price_micros where campaign_id=7," you'd have to read every event and aggregate yourself.

**The flow** (in a real production deployment, not currently wired in this repo):

```
                                  ┌──────────────┐
                                  │    Kafka     │
                                  │  bid_events  │
                                  └──────┬───────┘
                                         │
                                  consumer pipeline
                                  (KsqlDB / Flink /
                                   ClickHouse-Kafka engine)
                                         │
                                         ▼
                                  ┌──────────────┐
                                  │  ClickHouse  │
                                  │  bid_events  │  ← columnar table
                                  │  (table)     │
                                  └──────┬───────┘
                                         │
                                         ▼
                                  ┌──────────────┐
                                  │  SQL queries │
                                  └──────────────┘
```

**Example query:**

```sql
SELECT
  campaign_id,
  count(*) AS bid_count,
  sum(bid_price_micros) / 1e6 AS spend_usd
FROM bid_events
WHERE timestamp_ms >= toUnixTimestamp64Milli(yesterday())
GROUP BY campaign_id
ORDER BY spend_usd DESC
LIMIT 10;
```

**Why clickhouse specifically:** columnar storage (sums and aggregates over billions of rows in seconds); compresses 10-100×; standard SQL; built for analytical workloads.

**Why we don't have it in this repo:** the bidder's job is to publish events to Kafka. Reading Kafka and writing to ClickHouse is a *separate service* that we treat as out-of-scope. Production deployments use a Kafka-to-ClickHouse pipeline (KsqlDB, ClickHouse's built-in Kafka engine, or a Flink job).

---

## How they fit together — a single bid request, traced

Imagine a bid request arrives at `t=0`.

| Time | Event | Where |
|---|---|---|
| `t=0.0ms` | HTTP POST `/rtb/openrtb/bid` arrives | `axum` |
| `t=0.05ms` | Decode JSON (simd-json, in-place) | bidder process |
| `t=0.1ms` | Look up user segments in `moka` cache | in-process |
| `t=0.1ms` | (cache miss) `GET v1:seg:{u:42}` | **Redis** |
| `t=1.5ms` | Filter campaigns by RoaringBitmap intersection | in-process |
| `t=2.0ms` | `MGET v1:fc:{u:42}:c:7:h ...` for freq cap | **Redis** |
| `t=8.0ms` | Pick winner via FeatureWeighted scoring + ML | in-process |
| `t=8.5ms` | Build `BidResponse` JSON | in-process |
| `t=8.5ms` | Increment `bidder.bid.requests_total{result="bid"}` | **Prometheus** (in-memory atomic) |
| `t=8.5ms` | Record `bidder.bid.duration_seconds.observe(0.0085)` | **Prometheus** (in-memory histogram) |
| `t=8.5ms` | `event_publisher.publish(BidEvent { ... })` | **Kafka** (rdkafka internal queue) |
| `t=8.5ms` | Send 200 response with bid body | client receives |
| `t=8.5ms` to `t=∞` | librdkafka batches the BidEvent and flushes to broker | async, out of band |
| `t=15s` | Prometheus scrapes `/metrics`, picks up the new counter values | scheduled |

Three observation points worth noticing:

1. **The bid response goes out at `t=8.5ms`.** Everything after that — Kafka delivery, Prometheus scrape — is fire-and-forget and has zero impact on bid latency.
2. **Both Redis hops are inline and on the SLA budget.** Slow Redis = slow bid.
3. **Postgres is nowhere on this path.** It was queried 60 seconds ago, not now.

---

## When something goes wrong, where do you look?

| Symptom | First place to look | Why |
|---|---|---|
| p99 latency spike | Grafana → "Per-stage p99" panel | Identify which stage is slow |
| Bid rate dropped | Grafana → "Bid rate" + "Budget exhausted" panels | Catalog issue or budget burnt |
| Bidder is unreachable | `make health` | Process up? `/health/ready` returning 200? |
| Kafka events missing in downstream | `bidder.kafka.events_dropped` counter | Drop attribution: queue_full / kafka_error / incident_sample |
| Redis "slow call" warnings | `bidder.redis.requests_total` rate vs `bidder.hedge.redis_p95_ms` | Network or Redis-side problem |
| Want to know what bids ran yesterday | ClickHouse SQL (when wired) | Kafka has the events; ClickHouse aggregates |
| Specific bid investigation by request_id | Bidder logs (`tracing::error!` events) | Logs are the only place individual request_ids show up by default |

---

## Common confusions cleared up

**"Don't all the bid events go to Kafka, then Kafka feeds Grafana?"**
No. Kafka feeds *downstream consumers* (analytics, billing). Grafana reads from *Prometheus*, not Kafka. They're parallel paths.

**"Then what does Prometheus actually receive?"**
Counters, histograms, gauges. Numbers describing the bidder's behaviour — not the contents of any individual bid.

**"Why store events in Kafka if no one reads them?"**
You're right — there's no point if no consumer exists. In this repo we publish them anyway because the publish path is part of what we want to load-test. In production they'd be consumed by analytics, billing, ML training, audit/replay.

**"Why not put the metrics in Kafka too?"**
Volume. At 50K req/s, a metric increment is ~10ns. A Kafka publish is ~µs and produces ~100 bytes of payload. Multiply by every metric increment per request and you're producing GB/s of Kafka traffic to convey what Prometheus does in a 15s scrape.

**"Why three different stores instead of one?"**
Each is good at exactly what it does. Prometheus can't store events. Kafka can't run SQL queries. ClickHouse can't accept high-frequency writes on the bid path. Pick the right tool for each access pattern; live with three.

---

## Code map — where each integration lives

| Service | Code |
|---|---|
| Postgres | [bidder-core/src/catalog/loader.rs](../bidder-core/src/catalog/loader.rs) (load + 60s refresh) |
| Redis (hot path) | [bidder-server/src/segment_repo.rs](../bidder-server/src/segment_repo.rs), [bidder-core/src/frequency/](../bidder-core/src/frequency/) |
| Redis (config / contract) | [docs/REDIS-KEYS.md](REDIS-KEYS.md) |
| Kafka publisher | [bidder-server/src/kafka.rs](../bidder-server/src/kafka.rs) |
| Kafka event types | [bidder-protos/proto/events.proto](../bidder-protos/proto/events.proto) |
| Prometheus exposition | wired automatically by the `metrics` crate; endpoint is `/metrics` on the metrics server |
| Grafana dashboard | [docker/grafana/dashboards/bidder.json](../docker/grafana/dashboards/bidder.json) |
| ClickHouse | (not in this project; would be a separate consumer service) |

---

## TL;DR

- **Postgres** owns the catalog. Read on boot + every 60s, never on the hot path.
- **Redis** owns the hot per-user data. Read on every request. Cached aggressively in moka in front.
- **Kafka** is the bidder's outbox: every business action becomes an event. Async, fire-and-forget.
- **Prometheus** scrapes the bidder's `/metrics` endpoint every 15s. Stores counters / histograms / gauges as time-series.
- **Grafana** is the visual layer on top of Prometheus. No data of its own.
- **ClickHouse** is the analytics warehouse for Kafka-derived event tables. Not in this repo's compose, would be a downstream service.

Each tool answers a different question. Use the question table at the top of this doc to figure out which one to reach for.

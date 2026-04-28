# Redis Key Schema

Phase 0.1 lock-in. Defines every Redis-backed key family used by the bidder: format, hash-tag scope, value encoding, TTL, access pattern, and memory footprint at production scale (100M users, 100K campaigns, 25K-50K RPS per instance).

This document is the contract that Phase 1+ implementations reference. Schema changes after Phase 1 require a versioned dual-write migration (see [Versioning](#versioning--migration-strategy)).

Reference targets in `PLAN.md`:
- Workload assumptions table (top of plan).
- Phase 3 — *Redis throughput discipline*.
- Phase 4 — *Frequency capping*, *Hedged Redis MGET* guardrails.
- Latency budget table (Redis-backed stages share an envelope).

## Conventions

All keys carry a schema-version prefix. v1 is the only version in scope today.

```
v1:<family>:<scope-or-hashtag>:<discriminators...>
```

- `v1:` is the migration anchor. Every future schema change rolls forward via `v2:` dual-write, never in-place mutation. See [Versioning](#versioning--migration-strategy).
- `<family>` is one of: `seg`, `fc`, `bud`, `warm`, `cat`. Short on purpose — at 100M users a 4-byte saving per key is ~400 MB across the keyspace.
- Hash-tags use the standard Redis Cluster `{...}` syntax. Anything inside `{...}` is the slot key. Outside `{...}` does not affect routing.
- Identifiers in keys are decimal ASCII for readability and `redis-cli` ergonomics. Binary-encoded IDs save ~30% on key bytes but break operability; not worth it at this keyspace size.
- All numeric IDs are u32 internally (`UserId`, `CampaignId`, `CreativeId`, `SegmentId`, `DeviceTypeId`).

### Hash-tag strategy summary

Redis Cluster routes a key to a slot via `CRC16(hashtag) mod 16384`. Keys sharing a hash-tag land on the same slot, which is what makes per-user MGET viable in one round-trip (Phase 4 frequency capping). Hash-tag scope is chosen per family to match the dominant access pattern.

| Family | Hash-tag scope | Why |
|---|---|---|
| User segments (`seg`) | `{u:<userId>}` | Single-key reads; per-user co-location is irrelevant for `seg` itself but lets it co-locate with `fc:*` for future per-user EVAL scripts. |
| Frequency caps (`fc`) | `{u:<userId>}` | **Non-negotiable.** All freq-cap counters for a user must share a slot so per-bid MGET is a single round-trip. Drives the entire MGET batch design. |
| Distributed budget (`bud`) | `{c:<campaignId>}` | Budget is per-campaign; co-location with other per-campaign data (none today, reserved for v2). |
| Hot-user warm-set (`warm`) | none (single key) | One key total; hash-tag would be inert. |
| Catalog reload signal (`cat`) | none (pub/sub channel + single key) | Single key / channel; cluster-slot affinity does not apply meaningfully. |

The non-negotiable invariant: **all `fc:` keys for one user share the `{u:<userId>}` hash-tag**. Freq-cap MGET correctness on Cluster depends on it.

### Cluster vs single-node

v1 deploys to a single Redis instance for simplicity (one process, no cluster proxy, no resharding ops). The key design is nonetheless Cluster-correct from day one:

- All multi-key commands stay within one hash-tag scope (one user for `fc:`, one campaign for `bud:`).
- No `KEYS`, no `SCAN`-with-MGET-fanout. Hot path uses only commands that are slot-local.
- Pub/sub for catalog reload uses a single channel; cluster pub/sub is broadcast across the shard map, which is acceptable here (one event per minute at most).

Migrating from single-node to Cluster is a topology change with zero key-schema work.

---

## Family: User segments — `seg`

Set of segment IDs the user belongs to. Read on every bid request after the moka cache miss path. Hottest read in the system.

### Key

```
v1:seg:{u:<userId>}
```

Example: `v1:seg:{u:42}`

### Value encoding

**Raw binary: little-endian packed `u32` segment IDs, no header, no length prefix.**

A user with 120 segments occupies 480 bytes of value payload. JSON would be ~1.2 KB after the digit expansion of `[123456,234567,...]`; MessagePack ~600 bytes. Raw packed u32 is the smallest dense format and decodes at memcpy speed into a `Vec<u32>` (then handed to the `SegmentRegistry` for inverted-index lookup).

Segment IDs are globally assigned u32s (see `SEGMENT-IDS.md`). Order in the value is unsorted; the decoder builds a `RoaringBitmap` from the slice.

Schema evolution: a magic byte is **not** prepended in v1. The `v1:` key prefix carries the schema version; payload format changes go through a `v2:` dual-write rollover, not an in-band header.

### TTL

**14 days** sliding (refreshed on write by the upstream DMP / segment ingestion pipeline). Users absent from the audience for two weeks expire naturally. Bidder never writes this key — it is read-only on the bid path.

### Access pattern

| Operation | Command | Frequency |
|---|---|---|
| Read on cache miss | `GET v1:seg:{u:42}` | ~5K/sec at 30K RPS with 80% moka cache hit rate |
| Bulk read (warmup) | `MGET v1:seg:{u:1} v1:seg:{u:2} ...` | Rare; only at startup against the hot-user warm-set |

The bid path issues at most one `seg` GET per request (after moka miss). MGET across users is **not** used on the hot path — different users land on different slots. Warmup is the only place batch user-segment reads happen, and it is cluster-aware (groups MGET batches by slot).

### Memory footprint

| Quantity | Value |
|---|---|
| Users | 100M |
| Avg segments per user | 100 (mid-point of 50-200, Zipfian distribution) |
| Avg payload | 400 bytes |
| Redis key overhead per entry | ~80 bytes (key string + dict overhead + TTL slot) |
| Per-user cost | ~480 bytes |
| **Total** | **~48 GB** |

This is the dominant Redis memory consumer. A single 64 GB instance fits this with headroom for `fc:` and `bud:` (below). Beyond ~150M users or 200+ avg segments, Cluster sharding is required — the key schema already supports it.

---

## Family: Frequency caps — `fc`

Counters tracking how many times user U has been shown campaign C, creative R, on device D, in daypart P, within window W. Read+write on every bid (the second-hottest path after `seg`). MUST be batchable in one MGET per user.

### Key

```
v1:fc:{u:<userId>}:<dim>:<dimVal>:<window>
```

Where `<dim>` is one of:

| Dim | Meaning | `<dimVal>` |
|---|---|---|
| `c` | Campaign-level cap | campaign id (u32) |
| `r` | Creative-level cap | creative id (u32) |
| `d` | Device-level cap | device type id (u8) |
| `p` | Daypart-level cap | daypart bucket (0-23 hour-of-day) |

`<window>` is one of `h` (hour), `d` (day), `w` (week). Per-window counters are independent keys; they expire on different cadences.

Examples:

```
v1:fc:{u:42}:c:9001:d
v1:fc:{u:42}:c:9001:h
v1:fc:{u:42}:r:55501:d
v1:fc:{u:42}:d:3:h
v1:fc:{u:42}:p:14:d
```

All five keys above share hash-tag `{u:42}` → same slot → single MGET round-trip.

### Value encoding

**Raw integer string** (Redis native counter format).

Stored as the ASCII representation Redis itself uses for `INCR`/`DECRBY` operands. No serialization layer. Decode is `parse::<u32>` on the returned bulk string; encode is implicit on `INCRBY`. JSON or binary packing is wrong here — `INCR`/`EVAL` arithmetic require Redis to interpret the value as an integer.

Counter width is u32 (max 4.29B). A user hitting 4B impressions on one creative-day is impossible; u32 is overkill but matches the language type and removes any decode branching.

### TTL

| Window | TTL | Set by |
|---|---|---|
| `h` (hour) | 3600 s | `EVAL` script on first INCR |
| `d` (day) | 86400 s | `EVAL` script on first INCR |
| `w` (week) | 604800 s | `EVAL` script on first INCR |

TTL is set atomically with the INCR via a Lua script (`INCR` then `EXPIRE` if `ttl == -1`). Phase 4 Component: `frequency::ImpressionRecorder` runs this script through `EVAL` on a bounded write queue (mpsc 65536, drop-on-overflow).

### Access pattern

| Operation | Command | Notes |
|---|---|---|
| Read all caps for user (bid path) | `MGET v1:fc:{u:42}:c:9001:d v1:fc:{u:42}:c:9001:h ... v1:fc:{u:42}:r:55501:d` | Up to 50 keys per user, single round-trip per user |
| Increment after impression | `EVAL incr-with-ttl 1 v1:fc:{u:42}:c:9001:d 86400` | Async via `ImpressionRecorder` queue, off the bid path |

Concrete example: reading user 42's freq caps at bid time:

```
MGET v1:fc:{u:42}:c:9001:d v1:fc:{u:42}:c:9001:h v1:fc:{u:42}:r:55501:d v1:fc:{u:42}:d:3:h v1:fc:{u:42}:p:14:d
```

Result is a `Vec<Option<i64>>` aligned 1:1 with the candidate-cap list assembled by the targeting stage. Missing keys (counter never incremented or already expired) decode as `None` → treated as zero.

The `ImpressionRecorder` write Lua script (committed in Phase 4):

```
local current = redis.call('INCR', KEYS[1])
if current == 1 then
  redis.call('EXPIRE', KEYS[1], ARGV[1])
end
return current
```

Single round-trip, atomic. EXPIRE is set only on first INCR to avoid resetting the window on subsequent increments.

### Hedged read constraint

Per Phase 4 guardrails, hedged Redis applies only to idempotent reads. `MGET v1:fc:{u:...}:*` is hedge-eligible; the `EVAL` write path never hedges. The trait split (`IdempotentRedisRead` vs `RedisWrite`) enforces this at compile time.

### Memory footprint

| Quantity | Value |
|---|---|
| Active users (with at least one fc counter live) | 30M (Zipfian — most cap activity concentrated in daily-active users) |
| Avg counters per active user | 25 (mid-point of 10-50) |
| Counter value size | 4-8 bytes (Redis encodes small ints inline) |
| Key string size | ~30 bytes |
| Per-counter Redis overhead | ~70 bytes (entry + TTL + dict) |
| Per-counter cost | ~100 bytes |
| **Total** | **~75 GB** |

This is the second-largest consumer. At 100M users active, this would balloon to ~250 GB and **forces Cluster sharding by default**. The 30M active-user assumption holds for typical DSP traffic shapes; an instance with broader user activity must shard.

A `MAXMEMORY allkeys-lru` policy on the Redis instance is the safety net; counters past their window TTL are evicted naturally before the LRU floor triggers.

---

## Family: Distributed budget pacing — `bud`

Remaining budget (in micro-currency units) for a campaign in the current pacing window. Atomic `DECRBY` on the bid path when `DistributedBudgetPacer` is selected (multi-instance deployments). `LocalBudgetPacer` does not touch Redis.

### Key

```
v1:bud:{c:<campaignId>}:<window>
```

Where `<window>` is `h` (hour) or `d` (day). Most campaigns pace daily; a subset paces hourly.

Examples:

```
v1:bud:{c:9001}:d
v1:bud:{c:9001}:h
```

Hash-tag is the campaign id. There is no batch-read across campaigns on the hot path — each candidate's budget is checked independently after scoring, and the candidate set is small (top-K, K ≈ 50). Scattering across slots costs nothing at K=50; co-locating per-campaign keeps future per-campaign EVAL scripts (e.g. atomic `check-and-decrement`) viable on Cluster.

### Value encoding

**Raw integer (Redis native).** Currency stored as `i64` micro-units (1 unit = 1 millionth of the configured currency, e.g. micro-USD). Allows `DECRBY` to be atomic without a Lua script for the common case.

Negative values are explicitly allowed and meaningful: a campaign that overspent its window by a small amount due to in-flight bids decrementing past zero. The pacer treats any value `<= 0` as exhausted.

Refill is a daily/hourly batch job that issues `SET v1:bud:{c:9001}:d <amount>` at window roll. Atomic with respect to in-flight bids: an `IF NOT EXISTS`-style guard is unnecessary because the refill writer is single-instance (the catalog refresh job).

### TTL

**No TTL.** Persistent. Budget keys are managed by the refill job, not the bidder. A missing key for an active campaign is a configuration error → bidder treats it as "no budget available" and skips the candidate, with a warn-level log and a `bidder.budget.missing_key` counter.

### Access pattern

| Operation | Command | Frequency |
|---|---|---|
| Read remaining budget (DistributedBudgetPacer) | `GET v1:bud:{c:9001}:d` | Up to K per bid (K ≈ 50 candidates after top-K) |
| Atomic spend decrement | `DECRBY v1:bud:{c:9001}:d 1500` | Once per won bid |
| Window refill (off-path) | `SET v1:bud:{c:9001}:d 100000000` | Per campaign per window roll |

For `LocalBudgetPacer` (single-instance deployment, the v1 default), Redis is not touched at all — `AtomicI64` per campaign in process, periodic flush back to Redis for visibility-only. `DistributedBudgetPacer` is the multi-instance deployment switch.

The K candidate budget reads are issued as a pipeline (not MGET, because they span K different slots on Cluster). At K=50 with `fred`'s native pipelining this is sub-millisecond on a healthy Redis.

### Memory footprint

| Quantity | Value |
|---|---|
| Active campaigns | 100K |
| Windows per campaign | 1.5 avg (most daily-only, some both daily and hourly) |
| Per-key cost | ~80 bytes |
| **Total** | **~12 MB** |

Trivial. Budget is not a memory concern; it is a write-contention concern (atomic DECRBY on hot campaigns), addressed by `LocalBudgetPacer` for single-instance and by sharded counters (deferred — out of scope for v1) for multi-instance deployments where one campaign's QPS exceeds Redis single-key throughput.

---

## Family: Hot-user warm-set — `warm`

List of the most-active user IDs, used by the warmup phase to pre-populate the moka cache before the pod is added to LB rotation. Written periodically by a previous bidder instance or a background job; read once at startup.

### Key

```
v1:warm:users
```

Single key. No hash-tag (one key = one slot, hash-tag is inert). Cluster operability: all instances read the same key; in a multi-shard Cluster the writers all converge on the same slot.

### Value encoding

**rkyv-archived `Vec<u32>` of user IDs**, sorted by recent activity descending. Decoded via zero-copy `rkyv::access` into a `&Archived<Vec<u32>>` and iterated directly into the moka warmup loop. No allocation for the user-id list itself — only for the moka entries created downstream.

rkyv (vs raw packed u32 like `seg`): the warm-set is read once per startup, not per bid. The slight rkyv overhead vs raw bytes is irrelevant; the schema-evolution and validation properties matter more (a corrupted warm-set crashes at decode, not at use).

### TTL

**24 hours.** Refreshed daily by a background job (Phase 7 component, simpler version in Phase 1: a sidecar that pulls activity from ClickHouse / Kafka and rewrites this key). If the writer is down for >24h, warmup falls back to the cold path: pod still becomes ready, just slower for the first ~30s.

### Access pattern

| Operation | Command | Frequency |
|---|---|---|
| Read at warmup | `GET v1:warm:users` | Once per pod start |
| Write by background job | `SET v1:warm:users <archived-bytes> EX 86400` | Once per hour (or per the writer cadence) |

Pod warmup pseudocode (from Phase 1 warmup contract): GET this key, decode the user-id vector, issue grouped MGETs against `v1:seg:{u:<id>}` (grouped by slot), populate moka.

### Memory footprint

| Quantity | Value |
|---|---|
| Warm-set size | 50K user IDs |
| rkyv-archived size | ~250 KB (4 bytes per id + small archival header) |
| Redis overhead | ~80 bytes |
| **Total** | **~250 KB** |

Trivial.

---

## Family: Catalog reload signal — `cat`

Signal that the campaign catalog (in Postgres) has changed and the in-process catalog should refresh ahead of its 60s tick. Used by the catalog refresh job to react to admin updates within seconds rather than at the next poll boundary.

### Implementation

**Pub/sub channel** (primary) + **versioned key** (fallback for missed messages).

```
Channel:  v1:cat:reload
Key:      v1:cat:version
```

Bidder behavior:
1. Subscribe to channel `v1:cat:reload` at startup.
2. On any message, schedule a catalog rebuild on the next loop iteration (do not rebuild synchronously — preserves the Phase 3 non-blocking rebuild contract).
3. On the 60s scheduled tick, also `GET v1:cat:version` and compare against last seen. If newer, rebuild. This handles the case where a pub/sub message was missed (network blip, subscriber not yet connected at startup).

Catalog updater behavior (out of scope for v1 bidder; lives in the catalog management service):
1. Apply changes to Postgres.
2. `INCR v1:cat:version`.
3. `PUBLISH v1:cat:reload <newVersion>`.

### Value encoding

`v1:cat:version`: integer (Redis native counter). Decoded as `u64`.
`v1:cat:reload` channel payload: ASCII version number. Bidder logs the value; only the *event of receipt* matters, not the payload contents.

### TTL

`v1:cat:version`: **no TTL.** Persistent; monotonically increasing.
Channel: pub/sub has no persistence by design.

### Access pattern

| Operation | Command | Frequency |
|---|---|---|
| Subscribe | `SUBSCRIBE v1:cat:reload` | Once per bidder startup |
| Version poll | `GET v1:cat:version` | Once per 60s catalog tick |
| Publish (catalog updater) | `PUBLISH v1:cat:reload <version>` | Per catalog change event (low rate, <1/min normal) |
| Increment (catalog updater) | `INCR v1:cat:version` | Per catalog change |

Cluster note: pub/sub on Cluster fans out across shards by default; this is fine for our rate. The version key lives on one slot and all bidders converge on it.

### Memory footprint

A handful of bytes. Not material.

---

## Memory budget total

| Family | Footprint | Cluster-required at v1 scale? |
|---|---|---|
| `seg` (user segments) | ~48 GB | No |
| `fc` (frequency caps) | ~75 GB | **Yes** (>64 GB single-node) |
| `bud` (budgets) | ~12 MB | No |
| `warm` (warm-set) | ~250 KB | No |
| `cat` (catalog reload) | <1 KB | No |
| **Total** | **~123 GB** | **Yes** |

**Honest sizing: 100M users × 50-200 segments × full freq-cap activity does not fit one Redis instance.** The single-node v1 deployment is viable only for a smaller working set: e.g. 30M users × 100 segments × 25 active fc counters fits in a 64 GB instance with ~30% headroom. Production scale (100M+ users with broad fc activity) requires Cluster from day one or aggressive eviction tuning (`MAXMEMORY allkeys-lru` with a 64 GB cap, accepting that the cold tail of users falls out and gets refetched from the DMP path on next bid).

The v1 plan deploys a single instance for simplicity and validates the key schema is Cluster-correct. Sharding is a deployment-topology change, not a schema change.

### Per-RPS round-trip budget

At 30K RPS with 80% moka cache hit on `seg`:

| Family | RPS to Redis |
|---|---|
| `seg` GET (cache miss) | ~6K |
| `fc` MGET (per bid) | ~30K |
| `bud` GET pipeline (per bid, distributed pacer only) | ~30K (off when LocalBudgetPacer) |
| `bud` DECRBY (per win) | ~3K (10% win rate) |
| `fc` EVAL (per impression, async queue) | ~0.3K (post-win events) |
| `cat` SUBSCRIBE | trivial |

Per-bid Redis cost in the v1 single-instance LocalBudgetPacer config: 1 `seg` GET (cache miss) + 1 `fc` MGET = 2 commands, both slot-local on a Cluster topology. Within the Phase 4 Redis latency envelope (8 ms p99 for the freq-cap MGET; `seg` GET on cache miss shares the envelope per the Phase 1 latency budget table).

---

## Versioning / migration strategy

Schema changes are unavoidable across phases (new dimensions, encoding shifts, new families). The `v1:` prefix exists specifically to make migrations rolling-deployable.

### Migration protocol

1. **Add v2 alongside v1.** Update the key-builder helpers to produce both `v1:...` and `v2:...` shapes.
2. **Dual-write phase.** All writers (impression recorder, budget refill, segment ingester) write both `v1:` and `v2:` keys. Reads continue to come from `v1:`.
3. **Cutover.** Deploy a bidder build that reads from `v2:`. Bake. Roll back is `revert deploy`; both schemas are still being written.
4. **Stop v1 writes.** New writer build writes only `v2:`. v1 keys age out via TTL where applicable, or get manually deleted in batch where TTL is absent (`bud`, `cat:version`).
5. **Drop v1 prefix support** from the key-builder helpers in the next release.

The dual-write window costs roughly 2× memory for the affected family during migration. For `fc` this is ~75 GB additional headroom — must be planned and provisioned. For `seg`, ~48 GB additional. For everything else, negligible.

### Forbidden migration shortcuts

- **No in-place value rewrites.** Don't `GET` v1, transform, `SET` v1. Race conditions with concurrent writers cause data loss; rolling deploys cause readers to see torn states.
- **No Lua-script "live migration."** Same race issue plus pinning shard load on the migrator.
- **No silent encoding changes within v1.** If `seg` payload format changes, it goes to v2. Period.

### When NOT to bump version

- Adding a new key family (e.g. introducing `pix:` for pixel-tracking counters in a future phase): no migration needed, just deploy.
- Adding a new dimension to `fc` (e.g. `fc:{u:...}:s:<sectionId>:d`): no migration; old keys stay valid, new dimension just has no historical data for the first window.
- TTL changes: no migration; affects new writes only.

---

## Decisions, locked

These are the decisions Phase 1+ depends on. Re-opening any of them is a v2 schema migration.

| Decision | Choice | Locked because |
|---|---|---|
| Schema version prefix | `v1:` on every key | Migration anchor; cheap insurance. |
| User-segment encoding | Raw little-endian packed u32 | Smallest dense format; memcpy-speed decode; size dominates Redis cost at 100M users. |
| Freq-cap value | Redis native integer string | INCR / EVAL atomicity requires it. |
| Freq-cap hash-tag scope | `{u:<userId>}` | Single-MGET-per-user is the entire reason hash-tagging exists in this design. |
| Freq-cap dimensions | campaign, creative, device, daypart × hour/day/week | Mid-point of 10-50 counters per user; matches Phase 4 page size. |
| Budget value | Redis native i64 micro-units | Atomic DECRBY without Lua. |
| Budget hash-tag scope | `{c:<campaignId>}` | Per-campaign atomicity; future per-campaign EVAL viability. |
| Warm-set encoding | rkyv-archived `Vec<u32>` | One-shot read, schema-validated, zero-copy. |
| Catalog reload mechanism | pub/sub + version key | Pub/sub for latency, version key for missed-message recovery. |
| Identifier encoding in keys | Decimal ASCII | Operability beats the 30% byte saving from binary. |

Anything not in this table is implementation detail and may change without a schema migration.

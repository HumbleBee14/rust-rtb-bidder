# Segment ID Mapping

Phase 0, artifact 0.3. The contract for how a string segment name (e.g. `"in-market-travel"`) becomes a stable integer `SegmentId` used in `RoaringBitmap`-based inverted indices, where the mapping lives, and how it propagates across Postgres, Redis, Kafka, and the bidder's in-memory state.

This document does not duplicate `PLAN.md`. See PLAN.md Phase 0 (artifact 0.3 summary), Phase 3 (data layer, `SegmentRegistry`, bitmap-mutation safety contract), and the Stack decisions table (RoaringBitmap row) for upstream rationale.

---

## Decision summary

- **ID type:** `u32`. Type-aliased as `SegmentId` in the bidder.
- **Source of truth:** Postgres `segment` table. `id SERIAL PRIMARY KEY`, `name TEXT UNIQUE NOT NULL`.
- **Registration policy:** auto-register on first sight via UPSERT performed by a sidecar ingestion path, *not* the bidder. The bidder is read-only against the table at runtime.
- **Scope:** single-tenant, globally-flat ID space. Multi-tenant scoping is a deferred decision.
- **Persistence:** IDs are append-only and immutable. A segment is never re-numbered, never deleted; lifecycle is tracked by a `status` column.

---

## ID encoding

### Why `u32`

| Property | Value | Why it matters |
|---|---|---|
| Native bitmap width | `RoaringBitmap` stores `u32` natively | No truncation, no widening, no per-element conversion on the hot path. |
| Address space | 4,294,967,295 distinct segments | At our worst-case forecast (millions of segments), still leaves >1000x headroom. |
| Memory | 4 bytes per ID in any `Vec<SegmentId>` or hashmap key | Half of `u64`. Tighter cache lines on per-request working sets and inverted-index payloads. |
| Wire size | 4 bytes in protobuf `fixed32` / Kafka payloads | Half of `u64`; matters at Kafka tail event volume. |

`u64` is rejected: 2x memory and wire cost for headroom we will never use, and `roaring::RoaringTreemap` (the `u64` variant) has a different perf profile that we have not benchmarked. `u16` is rejected: 65K segment cap is below our 100K production target.

### Reserved IDs

| ID | Meaning |
|---|---|
| `0` | `INVALID` / `UNKNOWN_SEGMENT` sentinel. Never assigned by Postgres `SERIAL` (which starts at 1). Returned by registry lookups for unknown names. Never inserted into a bitmap. |
| `1..=u32::MAX` | Real segments, dense, monotonically assigned by Postgres `SERIAL`. |

Density matters: `RoaringBitmap` compression depends on values clustering into 2^16 containers. Dense, monotonically-assigned IDs cluster naturally; sparse / hashed IDs do not.

---

## Source of truth

The Postgres `segment` table (defined by sibling artifact 0.2) is canonical.

```
segment
  id        SERIAL      PRIMARY KEY     -- the SegmentId, u32 in the bidder
  name      TEXT        UNIQUE NOT NULL -- the wire-level string from OpenRTB
  category  TEXT                        -- e.g. "demographic", "intent", "behavioral"
  status    TEXT        NOT NULL        -- 'active' | 'retired'
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
```

The bidder runs SELECT-only against this table. It does not INSERT, UPDATE, or DELETE. Any process that mints IDs (the ingestion sidecar) is a separate component with its own credentials.

This separation is load-bearing: if multiple bidder instances were allowed to mint IDs locally, two instances seeing the same new segment name in the same second could assign different IDs, and bitmaps written by one would be unreadable by the other. The single-writer rule eliminates that class of bug.

---

## In-memory representation

The running bidder holds the registry as:

```
Arc<ArcSwap<SegmentRegistry>>

SegmentRegistry {
    name_to_id: HashMap<String, SegmentId>,
    id_to_name: HashMap<SegmentId, String>,
}
```

- **Loaded at startup** from `SELECT id, name FROM segment WHERE status = 'active'`.
- **Refreshed every 60s** by the same background task that rebuilds the campaign catalog. The refresh constructs a fresh `SegmentRegistry`, then `ArcSwap::store`s it. In-flight requests continue against the previous `Arc`.
- **Read-only on the hot path.** Lookups never block, never allocate.
- **No per-request locking.** `ArcSwap::load` is a single atomic pointer load.

### Memory footprint

For 100K segments with an average name length of ~30 bytes:

| Component | Estimate |
|---|---|
| `name_to_id` keys (interned `String`) | 100K × ~30 B = ~3 MB |
| `name_to_id` values (`u32`) | 100K × 4 B = 0.4 MB |
| `id_to_name` keys (`u32`) | 100K × 4 B = 0.4 MB |
| `id_to_name` values | another ~3 MB (separately owned) |
| `HashMap` overhead (load factor, buckets) | ~2 MB |
| **Total** | **~9 MB** |

Roughly 10 MB. Negligible against the bidder's overall footprint. Not worth deduplicating the name strings between the two maps in v1.

---

## Auto-registration of new segments

When a bid request arrives carrying a segment name the bidder doesn't know:

1. The bidder MUST NOT mint an ID itself. (Split-brain, see above.)
2. The bidder treats the unknown name as **no match** for the current request — same outcome as if no campaign targeted that segment.
3. The bidder increments a counter: `bidder.segment.unknown_seen{name="..."}`. (Cardinality is bounded by debouncing inside the metrics layer; see ops doc.)
4. A separate **ingestion sidecar** is responsible for promoting unknown names to canonical IDs. Implementation choices (any one of):
   - A periodic Postgres job that scans a `segment_seen` log table and UPSERTs into `segment`.
   - A small side service that consumes a Kafka `segment.observed` topic and UPSERTs.
   - The DMP / audience pipeline that already feeds user-segment data, doing UPSERT before publishing user data.
5. The bidder picks up the new segment on the next 60s catalog refresh.

### Cross-instance consistency tradeoff

We explicitly trade **immediate availability** for **cross-instance consistency**.

- A brand-new segment is *not* targetable for up to ~60s after it first appears on the wire.
- During that window, every bidder instance behaves identically: unknown name → no match.
- After the refresh, every bidder instance has the same `SegmentId` for that name.

This is the right tradeoff: a segment is only useful once a campaign targets it, and campaign-catalog updates already operate on the same 60s cadence. The window of "segment exists on wire but no campaign targets it yet" is the steady state, not a degraded mode.

### Example

```
T+0:    Bid request arrives at bidder-A with user.data.segment[].name = "auto-intender-2026"
        bidder-A: name_to_id.get("auto-intender-2026") -> None
        bidder-A: counter bidder.segment.unknown_seen{name="auto-intender-2026"} += 1
        bidder-A: treats as no-match, continues with remaining known segments
T+5s:   Same request shape arrives at bidder-B. Same outcome.
T+12s:  Ingestion sidecar UPSERTs ("auto-intender-2026") into segment, gets id=82431
T+60s:  All bidder instances refresh their SegmentRegistry. Every instance now resolves
        "auto-intender-2026" -> 82431. From this moment, any campaign targeting
        SegmentId 82431 will match.
```

---

## Per-tenant scoping (deferred)

v1 is **single-tenant**: one DSP, one segment namespace.

If multi-tenancy is ever needed, the surface area looks like:

- IDs become a `(tenant_id: u32, segment_id: u32)` pair, not a flat `u32`.
- Bitmaps shard per tenant (one inverted index per tenant).
- The `segment` table grows a `tenant_id` column and the unique constraint becomes `(tenant_id, name)`.
- The wire format on Kafka grows a `tenant_id` field.

To keep that path open without committing to it now, v1 enforces:

- Segment names are **global**. Do not encode tenant identity into segment names (no `acme:auto-intender` style prefixes). If a future migration needs to re-bucket existing names under a default tenant, names without a tenant prefix migrate cleanly; names with one do not.
- The `segment` table does not yet have a `tenant_id` column, but adding one with a default of `0` (the implicit "default tenant") is an additive, non-breaking change.

This artifact does not specify the multi-tenant design. It specifies that v1 does not preclude it.

---

## Backward compatibility and migration

Hard rules:

| Rule | Rationale |
|---|---|
| **IDs never change.** A given `SegmentId` always refers to the same segment for the lifetime of the system. | Every `RoaringBitmap` ever written (in the bidder, in offline pipelines, in archived event streams) references IDs by value. Re-numbering invalidates all of them. |
| **IDs are append-only.** SERIAL guarantees monotonic, dense, no-reuse assignment. | Density preserves Roaring container locality. Append-only enables incremental refresh. |
| **Renaming changes the name, not the ID.** `UPDATE segment SET name = ? WHERE id = ?` is permitted. | Names are descriptive; IDs are identity. Decoupling them is the point of having an ID at all. |
| **Deleting a segment is forbidden in production.** Use `UPDATE segment SET status = 'retired' WHERE id = ?` instead. | A `DELETE` would leave orphan IDs in every bitmap that references them. Marking retired keeps the ID resolvable for debugging and historical replay; the campaign-targeting layer simply stops including retired segments in new campaigns. |
| **Reusing an ID for a different name is forbidden.** | Same reason as deletion. Even if Postgres `SERIAL` would never do this, this rule must hold across any future migration tooling. |

A retired segment's bitmap is still valid; it just stops being referenced by active campaigns over time. Cleanup of orphaned IDs is not a v1 concern — at 100K segments and ~10 MB registry footprint, dead IDs cost nothing.

---

## Wire format

Where segments cross a boundary, the representation is fixed by the boundary type.

| Boundary | Representation | Why |
|---|---|---|
| External — OpenRTB bid request (`user.data[].segment[].name`) | **String.** | The OpenRTB spec says strings; we don't control SSP encoding. The bidder resolves to `SegmentId` at the entry point. |
| External — DMP user-segment ingestion | **String** at ingestion, **u32** once written to Redis. | Same: ingestion path resolves once, then propagates IDs. |
| Internal — Kafka events (impression, win, click) | **u32** (`fixed32` in protobuf). | Smaller, schema-stable, faster to parse, language-agnostic. |
| Internal — Redis values (user-segment lists, freq-cap counters keyed by segment) | **u32**, packed (e.g. `Vec<u32>` little-endian, or compact protobuf). | Redis bandwidth is a hot-path cost; strings would inflate it ~6-8x for typical segment names. |
| Postgres — `segment` table | **u32 id** + canonical **TEXT name**. | The only place the string lives long-term. |
| Postgres — joined targeting tables (`campaign_targeting_segment`, etc.) | **u32 id only** (foreign key). | Names live in exactly one table. |

### Example: a name resolved at the OpenRTB entry point

```
Incoming bid request fragment:
  "user": {
    "data": [{ "segment": [
      { "name": "auto-intender" },
      { "name": "in-market-travel" },
      { "name": "millennial" }
    ]}]
  }

At the request-decode stage:
  let registry = catalog.segment_registry.load();
  let user_seg_ids: RoaringBitmap = req.user.segments.iter()
      .filter_map(|s| registry.name_to_id.get(s.name).copied())
      .collect();

Result for known names: RoaringBitmap{ 12, 4471, 88 }
"auto-intender"     -> 12
"in-market-travel"  -> 4471
"millennial"        -> 88
"some-unknown-name" -> filtered out, counter incremented
```

From this point onward — through targeting, scoring, freq-cap, win-notice emit, Kafka publish — only `u32` IDs travel.

---

## Lookup performance

`HashMap<String, SegmentId>` lookup with the default `ahash`-style hasher is ~10-30 ns per call on modern x86.

Worst-case load:

```
30K RPS × ~10 segments/request = ~300K lookups/sec
At 30 ns/lookup           = ~9 ms/sec of CPU across the fleet
```

That's <0.001% of one core. Lookup cost is not on the optimization path.

### Alternatives considered and rejected

| Option | Why not |
|---|---|
| FST (finite-state transducer) over segment names | Optimized for ordered range queries; we only do exact lookup. Build cost > HashMap; lookup cost similar. |
| Perfect hash (e.g. `phf` crate, generated at startup) | Faster lookup (~5 ns), but rebuild on every 60s registry refresh adds latency we don't need. Requires regenerating the PHF on every newly-registered segment. |
| String interning + pointer comparison | Doesn't help because the string comes off the wire fresh per request — there's no shared identity to compare. |
| Skip the registry, use names everywhere | Would inflate every bitmap, every Redis value, every Kafka event by ~6-8x. The registry exists to avoid exactly this. |

`HashMap<String, SegmentId>` wins on simplicity at a cost we cannot measure.

---

## Open questions / future work

**At >1M total segments, revisit.** The u32 type still fits comfortably, but two things shift:

- `RoaringBitmap` perf characteristics change: with a million-plus distinct IDs, the inverted-index payloads become much sparser per-segment, and intersection performance depends on container density rather than cardinality. Benchmarks should be re-run.
- Registry memory footprint scales linearly. At 1M segments × ~30 B name × 2 maps ≈ ~60 MB. Still cheap, but worth measuring.
- Per-segment metrics with `name` as a label become high-cardinality and need to be downsampled or replaced with id-keyed metrics resolved at query time.

Out of v1 scope. Revisit when the segment count crosses 1M, or when a multi-tenant requirement lands — whichever comes first.

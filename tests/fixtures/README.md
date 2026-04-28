# Test Fixtures

Canonical OpenRTB 2.6 bid-request fixtures used by every unit test, integration test, criterion benchmark, and k6 load script in this repository.

## Source and provenance

The golden fixtures are derived from the **IAB Tech Lab OpenRTB 2.6 specification examples** — the most authoritative possible source for bid-request shape. They are not synthesized from scratch.

| Source | Section | Used for | Modification |
|---|---|---|---|
| [IAB OpenRTB 2.6 spec, §6.2.4](https://github.com/InteractiveAdvertisingBureau/openrtb2.x/blob/main/2.6.md) — Video example | Base of `imp[0]` (video with companion ads) and the `site`, `device`, `user.data[].segment[]` blocks | `golden-bid-request.json` | Segment names mapped to project seed list (see below); `device.geo` block added; second imp added |
| [IAB OpenRTB 2.6 spec, §6.2.1](https://github.com/InteractiveAdvertisingBureau/openrtb2.x/blob/main/2.6.md) — Simple Banner example | Source of the banner shape used in `imp[1]` | `golden-bid-request.json` | Embedded as second impression alongside the video |

**License:** IAB Tech Lab spec content is published under [Creative Commons Attribution 3.0 (CC BY 3.0)](https://creativecommons.org/licenses/by/3.0/). Copying and modification are permitted with attribution. This README and the inline provenance above constitute the attribution.

### What was modified vs the IAB samples

1. **Segment names.** The IAB sample uses generic placeholder segment names (`auto intenders`, `auto enthusiasts`). The bidder's `SegmentRegistry` only resolves names that exist in its Postgres `segment` table — the seed list in `migrations/0001_initial.sql`. Segment names in the fixture are mapped to ones from that seed list so the fixture exercises real candidate retrieval rather than silently no-matching.
2. **`device.geo` block added.** The IAB §6.2.4 sample omits `device.geo`. Real exchange traffic almost always carries geo, and our targeting indices include geo as a first-class dimension, so we add a US/NY/501 (NYC metro) geo block.
3. **Second impression added.** The IAB §6.2.4 sample is single-imp. Multi-imp is a Phase 2 invariant of this project, so we add a banner imp from §6.2.1 as `imp[1]`.
4. **`user.id`** is set to `"42"` — a small numeric u32-stringified value that fits the `{u:<userId>}` Redis hash-tag pattern from `docs/REDIS-KEYS.md`. The IAB sample uses a long opaque hash; we keep that shape valid (string user IDs are spec-legal) but pick a hash-tag-friendly value so the `seg` and `fc` MGET round-trips stay slot-local on Cluster.

The no-DMP-data variant (`golden-bid-request-no-user.json` — filename retained for stability; the file keeps `user.id` and only omits `user.data`) is identical to the main fixture except for the missing DMP segment data. It exercises the no-DMP-data code path, not a fully absent `user` object.

## Files

| File | Purpose | Code paths exercised |
|---|---|---|
| `golden-bid-request.json` | Multi-impression OpenRTB 2.6 request with populated DMP user data. Two imps: video (with companion ads) + banner. | Full pipeline: decode, segment resolution, candidate retrieval per imp, scoring, freq-cap MGET, response build with multi-imp `seatbid.bid[]`. |
| `golden-bid-request-no-user.json` | Same shape, `user.data` absent. | Decode, segment-resolution short-circuit (empty user-segment bitmap), candidate retrieval that depends only on geo / device / format, response build (typically empty `seatbid` per OpenRTB no-bid semantics, still 200 OK). |

Both fixtures share `imp[]`, `site`, `device`, identifying root `id` differs by suffix (`-NOUSR`).

## What the fixtures test

- **Multi-impression handling.** `imp[]` carries two impressions. Per-imp candidate selection, per-imp winner, response `seatbid.bid[]` matched 1:1 with `imp.id`.
- **Multi-format.** Video (with companion banners), and a standalone banner.
- **OpenRTB 2.6 compliance.** All required fields per spec; spec-published shape preserved from IAB.
- **Hash-tag-compatible user ID.** `user.id = "42"` parses as `u32`, slots cleanly into `v1:seg:{u:42}` and `v1:fc:{u:42}:*`.
- **DMP-segment vs no-DMP-segment paths.** The two fixtures cover the with-data and without-data branches.
- **Geo-targeted candidate retrieval.** `device.geo.country/region/metro` populated.

## What they do NOT cover

Specialized fixtures for each will live in this directory in later phases.

- **PMP / private deals.** No `imp[].pmp` block.
- **Auction extensions.** No `ext` blocks.
- **Native ad format.** §6.2.6 IAB Native sample is a separate fixture if/when needed.
- **Exotic device types.** Desktop-shaped (`devicetype: 2`); CTV, in-app, audio, mobile shapes are deferred.
- **Multi-currency requests.** `cur: ["USD"]` only.
- **GDPR / COPPA.** Not flagged. EU consent-string handling is a separate fixture.
- **Malformed / adversarial inputs.** No truncated JSON, no out-of-range integers, no unknown enums. Negative-path fixtures live alongside under their own filenames.

## Modification policy

These files MUST NOT be modified casually. A change to either golden fixture invalidates every benchmark and load-test result that referenced the old shape — including k6 corpora generated from it, criterion baselines, and any phase-results document that quotes throughput or latency numbers.

Changes go through PR review and ship with a baseline-recalibration note in the affected phase results doc. The PR description must enumerate every consumer affected and link the new baseline numbers.

## Cross-references

- `docs/PLAN.md` — Phase 0 artifact 0.4 spec; Phase 2 OpenRTB 2.6 model decisions; Phase 3 segment-registry contract.
- `docs/REDIS-KEYS.md` — `seg` and `fc` key shapes; the `{u:<userId>}` hash-tag rule that constrains `user.id`.
- `docs/POSTGRES-SCHEMA.md` — `segment` table seed list; targeting-table layout.
- `docs/SEGMENT-IDS.md` — wire-format segment strings; resolution from string to `u32 SegmentId` at the OpenRTB entry point.
- `migrations/0001_initial.sql` — the canonical 20-segment seed list.
- [IAB OpenRTB 2.6 spec](https://github.com/InteractiveAdvertisingBureau/openrtb2.x/blob/main/2.6.md) — upstream source; CC BY 3.0.

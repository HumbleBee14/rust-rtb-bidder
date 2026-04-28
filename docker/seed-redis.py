#!/usr/bin/env python3
"""
Seed Redis with user→segment data conforming to the v1 schema in
docs/REDIS-KEYS.md § "Family: User segments".

CONTRACT (non-negotiable, do not deviate):
  Key:   v1:seg:{u:<userId>}            (literal '{' '}' braces — Redis Cluster hash-tag)
  Value: raw little-endian packed u32 segment IDs, no header, no length prefix
  TTL:   14 days (sliding; bidder treats this key as read-only)

The Java repo's seed used `SADD key str str str` with hash-tag-less keys and string
values. That shape is NOT compatible with the Rust bidder. This script writes the
contract-correct shape from scratch.

Volume: 100K users by default (production target is 100M; the dev box doesn't have
the 48 GB Redis budget, but 100K is more than the bid load can hit at 5K RPS for
5 min, so the cache-miss path is exercised against real keys throughout the run).

Distribution: Zipfian segment popularity (alpha=1.1) so a handful of broad
segments dominate the population, matching real DSP traffic. Per-user segment
count uniform in [50, 200] per the Phase 0 workload-assumption table.

Usage (preferred — RESP-pipelined, ~200K users/sec):
    python3 docker/seed-redis.py | redis-cli --pipe

Usage (alternative — slower, plain commands; useful for debugging):
    python3 docker/seed-redis.py --plain | redis-cli

Override defaults:
    BIDDER_SEED_USERS=10000  python3 docker/seed-redis.py | redis-cli --pipe
    BIDDER_SEED_SEGMENTS=500 python3 docker/seed-redis.py | redis-cli --pipe
"""

import argparse
import os
import random
import struct
import sys

# ── Defaults — tunable via env vars or CLI flags ─────────────────────────────

# 100K users on a dev box. Production target (per PLAN.md workload table) is
# 100M but that requires a 48 GB Redis. Dev value gives ~50 MB Redis usage
# at the default 100-segs-per-user mid-Zipf, which fits trivially.
DEFAULT_USERS = int(os.environ.get("BIDDER_SEED_USERS", 100_000))

# Segment ID space. Postgres `segment` table assigns IDs starting at 1.
# Production target is 10K-100K segments (PLAN.md). This script writes IDs
# 1..N where N matches the Postgres seed segment count for consistency.
DEFAULT_SEGMENTS = int(os.environ.get("BIDDER_SEED_SEGMENTS", 1000))

# Per-user segment count. Production avg is 50-200 (PLAN.md). Uniform within
# that range — the per-user count distribution is independent of segment-
# popularity Zipf.
SEGS_PER_USER_MIN = 50
SEGS_PER_USER_MAX = 200

# Zipfian exponent for segment popularity. α=1.1 produces ~80% of weight in the
# top-10% of segments — matches the Postgres seed convention and real DSP data.
ZIPF_ALPHA = 1.1

# 14-day TTL per REDIS-KEYS.md. Use EXPIRE (in seconds) rather than EXPIREAT
# to keep the seed deterministic — bidders connecting later still see the
# data for 14 days from seed time, not until a fixed wall-clock instant.
TTL_SECONDS = 14 * 24 * 3600

# Random seed for reproducibility — fixed by default so two runs produce
# byte-identical Redis state, useful for diff-based debugging.
RNG_SEED = int(os.environ.get("BIDDER_SEED_RNG", 42))

# RESP pipeline flush boundary (number of users per write). 50K matches the
# Java seed; tuned so the Python-side bytearray doesn't grow unbounded but
# stdout buffer fills aren't paid per-user.
FLUSH_EVERY = 50_000


# ── Zipfian sampler ──────────────────────────────────────────────────────────

def build_zipf_cdf(n: int, alpha: float) -> list[float]:
    """
    Returns the cumulative distribution over ranks 1..n where
    P(rank=k) ∝ 1/k^α. Used by random.random()-driven inverse-CDF sampling
    for O(log n) per draw via bisect.

    Computed once for the run; reused across all users.
    """
    weights = [1.0 / (k ** alpha) for k in range(1, n + 1)]
    total = sum(weights)
    cdf: list[float] = []
    acc = 0.0
    for w in weights:
        acc += w / total
        cdf.append(acc)
    return cdf


def sample_zipf(cdf: list[float], rng: random.Random) -> int:
    """Return a 1-based rank drawn from the Zipfian CDF via inverse sampling."""
    import bisect
    r = rng.random()
    rank = bisect.bisect_left(cdf, r) + 1
    return min(rank, len(cdf))


# ── RESP encoding helpers ────────────────────────────────────────────────────

def resp_bulk(b: bytes) -> bytes:
    """Encode one bulk-string argument: $<len>\\r\\n<bytes>\\r\\n."""
    return b"$" + str(len(b)).encode() + b"\r\n" + b + b"\r\n"


def resp_command(*args: bytes) -> bytes:
    """Encode a full RESP command: *<argc>\\r\\n + bulk args."""
    out = bytearray()
    out += b"*" + str(len(args)).encode() + b"\r\n"
    for a in args:
        out += resp_bulk(a)
    return bytes(out)


# ── Seeding ──────────────────────────────────────────────────────────────────

def emit_user(user_id: int, segment_ids: list[int], ttl: int, plain: bool) -> bytes:
    """
    Emit the two commands needed to seed one user:
        SET v1:seg:{u:<userId>} <packed-u32-bytes>
        EXPIRE v1:seg:{u:<userId>} <ttl>

    SET + EXPIRE rather than SET ... EX <ttl> because the redis-cli --pipe
    path doesn't preserve `SET KEY VAL EX TTL` ordering with binary values
    cleanly under all RESP versions; SET-then-EXPIRE is universally safe.
    """
    key = f"v1:seg:{{u:{user_id}}}".encode()
    payload = struct.pack(f"<{len(segment_ids)}I", *segment_ids)

    if plain:
        # Plain text — readable, slow. Useful for one-off debugging:
        #     python3 docker/seed-redis.py --plain --users 5 | redis-cli
        # Binary payload escaped via Python repr; redis-cli interprets escapes.
        # NOTE: not byte-safe for arbitrary u32 values that contain CRLF;
        # included only for human inspection of small seeds.
        out = bytearray()
        out += f"SET v1:seg:{{u:{user_id}}} ".encode()
        out += b'"' + payload.hex().encode() + b'"' + b"\n"
        out += f"EXPIRE v1:seg:{{u:{user_id}}} {ttl}\n".encode()
        return bytes(out)

    # RESP pipeline path — what production seeds use. Binary-safe.
    return resp_command(b"SET", key, payload) + resp_command(b"EXPIRE", key, str(ttl).encode())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--users", type=int, default=DEFAULT_USERS,
                        help=f"Number of users to seed (default {DEFAULT_USERS})")
    parser.add_argument("--segments", type=int, default=DEFAULT_SEGMENTS,
                        help=f"Total segment ID space, IDs are 1..N (default {DEFAULT_SEGMENTS})")
    parser.add_argument("--ttl", type=int, default=TTL_SECONDS,
                        help=f"TTL in seconds (default {TTL_SECONDS} = 14 days)")
    parser.add_argument("--seed", type=int, default=RNG_SEED,
                        help=f"RNG seed for reproducibility (default {RNG_SEED})")
    parser.add_argument("--plain", action="store_true",
                        help="Emit plain `SET`/`EXPIRE` text (slow; debugging only)")
    args = parser.parse_args()

    if args.users < 1 or args.segments < 1:
        sys.stderr.write("--users and --segments must be >= 1\n")
        return 2

    rng = random.Random(args.seed)
    cdf = build_zipf_cdf(args.segments, ZIPF_ALPHA)

    sys.stderr.write(
        f"seeding {args.users:,} users × ~{(SEGS_PER_USER_MIN + SEGS_PER_USER_MAX) // 2} segs each "
        f"over an ID space of {args.segments:,} (Zipf α={ZIPF_ALPHA}, TTL {args.ttl}s)\n"
    )

    buf = bytearray()
    for user_id in range(1, args.users + 1):
        n = rng.randint(SEGS_PER_USER_MIN, SEGS_PER_USER_MAX)
        # Sample WITH REPLACEMENT from the Zipf distribution, then dedupe —
        # popular segments will appear multiple times in the draw and dedupe
        # to a smaller realised set than `n`. This is faithful to how real DMP
        # data builds: an audience pipeline emits popular segments more often,
        # the user-segment table dedupes on insert.
        seg_set: set[int] = set()
        while len(seg_set) < n:
            seg_set.add(sample_zipf(cdf, rng))
            # Backstop: if Zipf is so skewed we can't reach n unique IDs, accept
            # what we have rather than loop forever. Won't trigger at α=1.1
            # with segments >= 200 but defensive.
            if len(seg_set) >= args.segments:
                break

        segments = list(seg_set)
        rng.shuffle(segments)  # Order is unsorted per the contract.

        buf += emit_user(user_id, segments, args.ttl, args.plain)

        if user_id % FLUSH_EVERY == 0:
            sys.stdout.buffer.write(buf)
            buf = bytearray()
            pct = user_id * 100 // args.users
            sys.stderr.write(f"\r  {pct}% ({user_id:,} / {args.users:,} users)...")
            sys.stderr.flush()

    if buf:
        sys.stdout.buffer.write(buf)

    sys.stderr.write(f"\r  100% ({args.users:,} / {args.users:,} users)... done\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())

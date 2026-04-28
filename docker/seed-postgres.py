#!/usr/bin/env python3

import argparse
import io
import os
import random
import sys

import psycopg2
import psycopg2.extras

# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

parser = argparse.ArgumentParser()
parser.add_argument("--campaigns", type=int, default=10_000)
parser.add_argument("--segments", type=int, default=1_000)
args = parser.parse_args()

DATABASE_URL = os.environ.get("DATABASE_URL", "postgres://bidder:bidder@localhost:5432/bidder")

# ---------------------------------------------------------------------------
# Segment corpus
# ---------------------------------------------------------------------------

CATEGORIES = {
    "auto": [
        "auto-intender", "auto-luxury", "auto-truck", "auto-suv", "auto-ev",
        "auto-commuter", "auto-performance", "auto-family", "auto-used-car",
        "auto-parts-buyer", "auto-lease-shopper", "auto-brand-honda",
        "auto-brand-toyota", "auto-brand-ford", "auto-brand-bmw",
        "auto-brand-tesla", "auto-brand-chevrolet", "auto-new-model",
        "auto-financing", "auto-insurance-shopper",
    ],
    "travel": [
        "in-market-travel", "frequent-flyer", "cruise-shopper",
        "business-traveler", "luxury-traveler", "budget-traveler",
        "beach-vacationer", "ski-traveler", "road-tripper",
        "international-traveler", "domestic-traveler", "hotel-loyalty",
        "airbnb-user", "travel-deal-seeker", "adventure-traveler",
        "family-vacationer", "solo-traveler", "europe-traveler",
        "asia-traveler", "caribbean-traveler",
    ],
    "demographic": [
        "parents", "young-adults", "millennials", "gen-z", "homeowners",
        "high-income", "renters", "college-students", "retirees",
        "new-parents", "empty-nesters", "urban-dwellers", "suburban-parents",
        "rural-residents", "hispanic-american", "african-american",
        "asian-american", "veteran", "single-household", "dual-income-household",
    ],
    "interest": [
        "sports-fans", "gamers", "foodies", "fitness-enthusiasts",
        "tech-early-adopters", "music-fans", "movie-fans", "book-readers",
        "outdoor-enthusiasts", "home-garden", "diy-crafters", "pet-owners",
        "wine-lovers", "coffee-drinkers", "yoga-practitioners", "runners",
        "cyclists", "golfers", "fishing-hunting", "esports-fans",
    ],
    "shopping": [
        "fashion-shoppers", "back-to-school", "holiday-shoppers",
        "luxury-buyers", "deal-hunters", "online-shoppers", "amazon-prime",
        "big-box-shoppers", "subscription-box", "electronics-buyers",
        "furniture-shoppers", "beauty-buyers", "apparel-buyers",
        "footwear-buyers", "grocery-online", "cpg-buyers",
        "toy-buyers", "sporting-goods-buyers", "jewelry-buyers",
        "home-appliance-buyers",
    ],
    "b2b": [
        "small-business-owners", "c-suite-executives", "it-decision-makers",
        "hr-professionals", "finance-professionals", "marketing-managers",
        "sales-professionals", "entrepreneurs", "startup-founders",
        "procurement-managers", "logistics-managers", "healthcare-administrators",
        "legal-professionals", "architects-engineers", "real-estate-professionals",
        "restaurant-owners", "retail-store-owners", "franchise-owners",
        "manufacturing-buyers", "saas-evaluators",
    ],
    "finance": [
        "in-market-finance", "mortgage-shoppers", "refinance-intenders",
        "credit-card-shoppers", "personal-loan-seekers", "auto-loan-seekers",
        "student-loan-borrowers", "investing-enthusiasts", "crypto-interested",
        "retirement-planners", "tax-prep-seekers", "insurance-shoppers",
        "life-insurance-seekers", "wealth-management-prospects",
        "banking-switchers", "savings-maximizers", "fintech-early-adopters",
        "debt-consolidation-seekers", "business-loan-seekers",
        "financial-news-readers",
    ],
    "health": [
        "health-conscious", "prescription-drug-users", "otc-health-buyers",
        "weight-loss-seekers", "nutrition-enthusiasts", "supplement-buyers",
        "telehealth-users", "mental-health-aware", "diabetes-related",
        "heart-health-aware", "allergy-sufferers", "vision-care-buyers",
        "dental-care-buyers", "senior-health", "womens-health",
        "mens-health", "pregnancy-parenting-health", "fitness-app-users",
        "wellness-retreat-seekers", "alternative-medicine-interested",
    ],
    "entertainment": [
        "streaming-subscribers", "cord-cutters", "binge-watchers",
        "live-sports-viewers", "movie-theatergoers", "podcast-listeners",
        "gaming-console-owners", "pc-gamers", "mobile-gamers",
        "live-event-attendees", "concert-goers", "comedy-fans",
        "true-crime-fans", "reality-tv-fans", "documentary-fans",
        "anime-fans", "sci-fi-fans", "superhero-fans",
        "news-readers", "social-media-heavy-users",
    ],
    "sports": [
        "nfl-fans", "nba-fans", "mlb-fans", "nhl-fans", "mls-fans",
        "college-football-fans", "college-basketball-fans", "ufc-fans",
        "boxing-fans", "formula1-fans", "nascar-fans", "tennis-fans",
        "golf-fans", "soccer-fans", "fantasy-sports-players",
        "sports-bettors", "marathon-runners", "crossfit-enthusiasts",
        "swimming-enthusiasts", "basketball-players",
    ],
}

# Build the full segment list, deduplicating against migration seeds.
MIGRATION_SEEDS = {
    "auto-intender", "auto-luxury", "in-market-travel", "frequent-flyer",
    "cruise-shopper", "parents", "young-adults", "millennials", "gen-z",
    "homeowners", "high-income", "sports-fans", "gamers", "foodies",
    "fitness-enthusiasts", "tech-early-adopters", "fashion-shoppers",
    "back-to-school", "holiday-shoppers", "small-business-owners",
}


def _build_segment_pool(target: int) -> list[tuple[str, str]]:
    """Return up to target (name, category) pairs, migration seeds excluded."""
    base: list[tuple[str, str]] = []
    for cat, names in CATEGORIES.items():
        for name in names:
            if name not in MIGRATION_SEEDS:
                base.append((name, cat))

    # If the corpus is smaller than target, generate synthetic names.
    if len(base) < target:
        adjectives = [
            "premium", "value", "budget", "luxury", "mass-market",
            "urban", "suburban", "rural", "coastal", "midwest",
        ]
        for cat, names in CATEGORIES.items():
            for adj in adjectives:
                for name in names:
                    synthetic = f"{adj}-{name}"
                    if synthetic not in MIGRATION_SEEDS:
                        base.append((synthetic, cat))
                    if len(base) >= target:
                        break
                if len(base) >= target:
                    break
            if len(base) >= target:
                break

    return base[:target]


# ---------------------------------------------------------------------------
# Geo constants
# ---------------------------------------------------------------------------

TOP_METROS = [
    ("metro", "501"),   # New York
    ("metro", "803"),   # Los Angeles
    ("metro", "602"),   # Chicago
    ("metro", "618"),   # Houston
    ("metro", "753"),   # Phoenix
    ("metro", "504"),   # Philadelphia
    ("metro", "641"),   # San Antonio
    ("metro", "825"),   # San Diego
    ("metro", "623"),   # Dallas
    ("metro", "807"),   # San Jose
]

COUNTRY_CODES = [
    "US", "CA", "GB", "AU", "DE", "FR", "JP", "BR", "MX", "IN",
    "ES", "IT", "NL", "SE", "NO", "DK", "FI", "PL", "KR", "SG",
]

EXTRA_METROS = [
    ("metro", "524"),   # Atlanta
    ("metro", "544"),   # Denver
    ("metro", "658"),   # Seattle
    ("metro", "528"),   # Miami
    ("metro", "539"),   # Tampa
    ("metro", "548"),   # Detroit
    ("metro", "609"),   # St. Louis
    ("metro", "563"),   # Indianapolis
    ("metro", "514"),   # Baltimore
    ("metro", "506"),   # Boston
]

# ---------------------------------------------------------------------------
# Daypart helpers
# ---------------------------------------------------------------------------

def _daypart_business_primetime() -> str:
    """Mon–Fri 08:00–23:00 UTC + Sat–Sun 10:00–23:00 UTC.

    Bit 0 = Mon 00:00, bits 0–167 packed as a 168-character '0'/'1' string
    which Postgres accepts for bit(168).
    """
    bits = ["0"] * 168
    for day in range(7):
        start = 8 if day < 5 else 10
        for hour in range(start, 23):
            bits[day * 24 + hour] = "1"
    return "".join(bits)


def _daypart_allday() -> str:
    return "1" * 168


def _daypart_random(rng: random.Random) -> str:
    bits = []
    for day in range(7):
        # Anchor on a plausible daily window so it is not pure noise.
        start = rng.randint(0, 10)
        end = rng.randint(14, 24)
        for hour in range(24):
            bits.append("1" if start <= hour < end else "0")
    return "".join(bits)


DAYPART_BUSINESS = _daypart_business_primetime()
DAYPART_ALLDAY = _daypart_allday()

# ---------------------------------------------------------------------------
# Creative dimensions
# ---------------------------------------------------------------------------

FORMAT_DIMS = {
    "BANNER": [(728, 90), (300, 250), (320, 50), (160, 600)],
    "VIDEO":  [(1920, 1080), (1280, 720), (640, 480)],
    "NATIVE": [(1200, 628), (600, 315)],
    "AUDIO":  [(1, 1)],
}

CLICK_DOMAIN = "https://click.example.com"
IMAGE_DOMAIN = "https://cdn.example.com/creatives"

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    conn = psycopg2.connect(DATABASE_URL)
    conn.autocommit = False
    cur = conn.cursor()

    rng = random.Random(42)

    # --- Segments ---

    segment_pool = _build_segment_pool(args.segments)

    seg_buf = io.StringIO()
    for name, cat in segment_pool:
        seg_buf.write(f"{name}\t{cat}\n")
    seg_buf.seek(0)

    cur.execute("CREATE TEMP TABLE _seg_import (name text, category text)")
    cur.copy_from(seg_buf, "_seg_import", columns=("name", "category"))
    cur.execute("""
        INSERT INTO segment (name, category)
        SELECT name, category FROM _seg_import
        ON CONFLICT (name) DO NOTHING
    """)
    conn.commit()

    cur.execute("SELECT id, name FROM segment ORDER BY id")
    all_segments = cur.fetchall()   # [(id, name), ...]
    seg_ids = [row[0] for row in all_segments]

    print(f"segments seeded: {len(seg_ids)}")

    # --- Truncate campaign-side tables (leave segment alone) ---

    cur.execute("""
        TRUNCATE TABLE
            campaign_targeting_segment,
            campaign_targeting_geo,
            campaign_targeting_device,
            campaign_targeting_format,
            campaign_daypart,
            creative,
            campaign
        CASCADE
    """)
    conn.commit()

    # --- Zipfian weight vector over segment ids ---
    #
    # We rank all segments by id (rank 1 = lowest id = "most popular").
    # The Zipf probability for rank k with exponent α is proportional to 1/k^α.
    # α=1.1 gives a steep-enough curve so the top-50 segments are heavily
    # over-represented compared to a uniform draw, matching real DSP traffic
    # patterns where a handful of broad audience segments dominate.

    alpha = 1.1
    seg_weights = [1.0 / (i + 1) ** alpha for i in range(len(seg_ids))]
    weight_sum = sum(seg_weights)
    seg_weights = [w / weight_sum for w in seg_weights]

    top50_ids = set(seg_ids[:50])

    def _sample_segments(n: int) -> list[int]:
        """Sample n unique segment ids biased by Zipfian weight.

        70% of the time draw exclusively from the top-50; the remaining
        draws come from the full distribution.  This mirrors the spec's
        intent that popular segments appear in ~70% of campaigns.
        """
        if rng.random() < 0.70:
            pool = [s for s in seg_ids if s in top50_ids]
            pool_weights = [seg_weights[i] for i, s in enumerate(seg_ids) if s in top50_ids]
            w_sum = sum(pool_weights)
            pool_weights = [w / w_sum for w in pool_weights]
        else:
            pool = seg_ids
            pool_weights = seg_weights

        chosen: list[int] = []
        seen: set[int] = set()
        attempts = 0
        while len(chosen) < n and attempts < n * 20:
            attempts += 1
            pick = rng.choices(pool, weights=pool_weights, k=1)[0]
            if pick not in seen:
                seen.add(pick)
                chosen.append(pick)
        return chosen

    # --- Batch buffers ---

    campaign_rows: list[tuple] = []
    seg_rows: list[tuple] = []
    geo_rows: list[tuple] = []
    device_rows: list[tuple] = []
    format_rows: list[tuple] = []
    daypart_rows: list[tuple] = []
    creative_rows: list[tuple] = []

    BATCH = 1000

    def _flush(campaign_id_offset: int) -> int:
        """Insert accumulated rows; returns the next campaign id offset."""
        if not campaign_rows:
            return campaign_id_offset

        # Insert campaigns and retrieve their generated ids in order.
        psycopg2.extras.execute_values(
            cur,
            """
            INSERT INTO campaign
                (name, advertiser_id, status, bid_floor_cents,
                 daily_budget_cents, hourly_budget_cents)
            VALUES %s
            RETURNING id
            """,
            campaign_rows,
            fetch=True,
        )
        new_ids = [row[0] for row in cur.fetchall()]

        # Remap placeholder indices (0-based within batch) to real ids.
        id_map = {i: new_ids[i] for i in range(len(new_ids))}

        def _remap(rows: list[tuple]) -> list[tuple]:
            return [(id_map[r[0]],) + r[1:] for r in rows]

        if seg_rows:
            psycopg2.extras.execute_values(
                cur,
                "INSERT INTO campaign_targeting_segment (campaign_id, segment_id) VALUES %s ON CONFLICT DO NOTHING",
                _remap(seg_rows),
            )
        if geo_rows:
            psycopg2.extras.execute_values(
                cur,
                "INSERT INTO campaign_targeting_geo (campaign_id, geo_kind, geo_code) VALUES %s ON CONFLICT DO NOTHING",
                _remap(geo_rows),
            )
        if device_rows:
            psycopg2.extras.execute_values(
                cur,
                "INSERT INTO campaign_targeting_device (campaign_id, device_type) VALUES %s ON CONFLICT DO NOTHING",
                _remap(device_rows),
            )
        if format_rows:
            psycopg2.extras.execute_values(
                cur,
                "INSERT INTO campaign_targeting_format (campaign_id, ad_format) VALUES %s ON CONFLICT DO NOTHING",
                _remap(format_rows),
            )
        if daypart_rows:
            psycopg2.extras.execute_values(
                cur,
                "INSERT INTO campaign_daypart (campaign_id, hours_active) VALUES %s",
                [(id_map[r[0]], r[1]) for r in daypart_rows],
            )
        if creative_rows:
            psycopg2.extras.execute_values(
                cur,
                """
                INSERT INTO creative
                    (campaign_id, ad_format, click_url, image_url, width, height)
                VALUES %s
                """,
                _remap(creative_rows),
            )

        conn.commit()

        count = len(campaign_rows)
        campaign_rows.clear()
        seg_rows.clear()
        geo_rows.clear()
        device_rows.clear()
        format_rows.clear()
        daypart_rows.clear()
        creative_rows.clear()
        return campaign_id_offset + count

    STATUSES = ["active", "active", "active", "active", "active", "paused", "draft", "archived"]

    total_flushed = 0
    targeting_rows = 0

    for i in range(args.campaigns):
        idx = i % BATCH  # local index within the current batch

        advertiser_id = rng.randint(1, 500)
        status = rng.choice(STATUSES)
        bid_floor = rng.randint(50, 500)
        daily_budget = rng.randint(5_000, 500_000)
        hourly_budget = daily_budget // 24

        campaign_rows.append((
            f"campaign-{i+1:06d}",
            advertiser_id,
            status,
            bid_floor,
            daily_budget,
            hourly_budget,
        ))

        # Segments
        n_segs = rng.randint(3, 8)
        for sid in _sample_segments(n_segs):
            seg_rows.append((idx, sid))

        # Geo
        r_geo = rng.random()
        if r_geo < 0.60:
            # Top-10 US metros + US country.
            seg_rows  # (reuse variable just to avoid empty block)
            for kind, code in TOP_METROS:
                geo_rows.append((idx, kind, code))
            geo_rows.append((idx, "country", "US"))
        else:
            # Random mix: 1-3 countries + 0-2 extra metros.
            countries = rng.sample(COUNTRY_CODES, rng.randint(1, 3))
            for c in countries:
                geo_rows.append((idx, "country", c))
            n_metros = rng.randint(0, 2)
            if n_metros:
                for kind, code in rng.sample(EXTRA_METROS + TOP_METROS, n_metros):
                    geo_rows.append((idx, kind, code))

        # Device
        r_dev = rng.random()
        if r_dev < 0.40:
            device_rows.append((idx, "MOBILE"))
        elif r_dev < 0.60:
            device_rows.append((idx, "DESKTOP"))
        elif r_dev < 0.90:
            device_rows.append((idx, "MOBILE"))
            device_rows.append((idx, "DESKTOP"))
        else:
            for dt in ["DESKTOP", "MOBILE", "TABLET", "CTV", "OTHER"]:
                device_rows.append((idx, dt))

        # Format
        r_fmt = rng.random()
        if r_fmt < 0.50:
            chosen_formats = ["BANNER"]
        elif r_fmt < 0.70:
            chosen_formats = ["VIDEO"]
        elif r_fmt < 0.90:
            chosen_formats = ["NATIVE"]
        else:
            chosen_formats = ["AUDIO"]

        # ~15% of campaigns get a second format.
        if rng.random() < 0.15:
            extra = rng.choice([f for f in ["BANNER", "VIDEO", "NATIVE", "AUDIO"] if f not in chosen_formats])
            chosen_formats.append(extra)

        for fmt in chosen_formats:
            format_rows.append((idx, fmt))

        # Daypart
        r_dp = rng.random()
        if r_dp < 0.70:
            dp_mask = DAYPART_BUSINESS
        elif r_dp < 0.90:
            dp_mask = DAYPART_ALLDAY
        else:
            dp_mask = _daypart_random(rng)
        daypart_rows.append((idx, dp_mask))

        # Creatives (1-3, matching ad formats).
        n_creatives = rng.randint(1, 3)
        for _ in range(n_creatives):
            fmt = rng.choice(chosen_formats)
            dims = rng.choice(FORMAT_DIMS[fmt])
            w, h = dims
            creative_rows.append((
                idx,
                fmt,
                f"{CLICK_DOMAIN}/c/{i+1:06d}/{rng.randint(1000,9999)}",
                f"{IMAGE_DOMAIN}/{fmt.lower()}/{i+1:06d}.jpg" if fmt != "AUDIO" else None,
                w,
                h,
            ))

        if len(campaign_rows) >= BATCH:
            flushed_before = total_flushed
            total_flushed = _flush(total_flushed)
            batch_targeting = (
                len(seg_rows) + len(geo_rows) + len(device_rows) +
                len(format_rows) + len(daypart_rows)
            )
            # targeting_rows is approximate here; exact count tracked after flush
            print(f"  campaigns flushed: {total_flushed}", flush=True)

    # Final partial batch
    if campaign_rows:
        total_flushed = _flush(total_flushed)

    # Count targeting rows for the summary.
    cur.execute("""
        SELECT
            (SELECT COUNT(*) FROM campaign_targeting_segment) +
            (SELECT COUNT(*) FROM campaign_targeting_geo) +
            (SELECT COUNT(*) FROM campaign_targeting_device) +
            (SELECT COUNT(*) FROM campaign_targeting_format) +
            (SELECT COUNT(*) FROM campaign_daypart)
        AS total
    """)
    targeting_total = cur.fetchone()[0]

    cur.execute("SELECT COUNT(*) FROM creative")
    creative_total = cur.fetchone()[0]

    conn.close()

    print(f"campaigns seeded: {total_flushed}")
    print(f"targeting rows inserted: {targeting_total}")
    print(f"creatives inserted: {creative_total}")


if __name__ == "__main__":
    main()

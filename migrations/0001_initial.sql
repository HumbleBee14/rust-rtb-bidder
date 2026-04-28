-- Migration: 0001_initial — campaign catalog and targeting tables

BEGIN;

-- Enums

CREATE TYPE campaign_status AS ENUM ('active', 'paused', 'draft', 'archived');

CREATE TYPE device_type AS ENUM ('DESKTOP', 'MOBILE', 'TABLET', 'CTV', 'OTHER');

CREATE TYPE ad_format AS ENUM ('BANNER', 'VIDEO', 'NATIVE', 'AUDIO');

CREATE TYPE geo_kind AS ENUM ('country', 'metro');

CREATE TYPE segment_status AS ENUM ('active', 'retired');

-- Campaign

CREATE TABLE campaign (
    id                   bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name                 text        NOT NULL,
    advertiser_id        bigint      NOT NULL,
    status               campaign_status NOT NULL DEFAULT 'draft',
    bid_floor_cents      integer     NOT NULL CHECK (bid_floor_cents >= 0),
    daily_budget_cents   bigint      NOT NULL CHECK (daily_budget_cents >= 0),
    hourly_budget_cents  bigint      NOT NULL CHECK (hourly_budget_cents >= 0),
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    version              integer     NOT NULL DEFAULT 1
);

-- Partial index: hot-path catalog filter is always status = 'active'.
CREATE INDEX idx_campaign_active ON campaign (id) WHERE status = 'active';
CREATE INDEX idx_campaign_updated_at ON campaign (updated_at);

-- Maintain updated_at on UPDATE.
CREATE OR REPLACE FUNCTION campaign_set_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_campaign_updated_at
    BEFORE UPDATE ON campaign
    FOR EACH ROW
    EXECUTE FUNCTION campaign_set_updated_at();

-- Segment registry

CREATE TABLE segment (
    id         integer        GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       text           NOT NULL UNIQUE,
    category   text           NOT NULL DEFAULT 'uncategorized',
    status     segment_status NOT NULL DEFAULT 'active',
    created_at timestamptz    NOT NULL DEFAULT now()
);

-- Targeting: segment

CREATE TABLE campaign_targeting_segment (
    campaign_id bigint  NOT NULL REFERENCES campaign(id) ON DELETE CASCADE,
    segment_id  integer NOT NULL REFERENCES segment(id) ON DELETE RESTRICT,
    PRIMARY KEY (campaign_id, segment_id)
);

-- Reverse-order index for the inverted-index build query (GROUP BY segment_id).
CREATE INDEX idx_cts_segment ON campaign_targeting_segment (segment_id, campaign_id);

-- Targeting: geo (country + metro share the table, discriminated by geo_kind).

CREATE TABLE campaign_targeting_geo (
    campaign_id bigint   NOT NULL REFERENCES campaign(id) ON DELETE CASCADE,
    geo_kind    geo_kind NOT NULL,
    geo_code    text     NOT NULL,
    PRIMARY KEY (campaign_id, geo_kind, geo_code)
);

CREATE INDEX idx_ctg_geo ON campaign_targeting_geo (geo_kind, geo_code, campaign_id);

-- Targeting: device

CREATE TABLE campaign_targeting_device (
    campaign_id bigint      NOT NULL REFERENCES campaign(id) ON DELETE CASCADE,
    device_type device_type NOT NULL,
    PRIMARY KEY (campaign_id, device_type)
);

CREATE INDEX idx_ctd_device ON campaign_targeting_device (device_type, campaign_id);

-- Targeting: ad format

CREATE TABLE campaign_targeting_format (
    campaign_id bigint    NOT NULL REFERENCES campaign(id) ON DELETE CASCADE,
    ad_format   ad_format NOT NULL,
    PRIMARY KEY (campaign_id, ad_format)
);

CREATE INDEX idx_ctf_format ON campaign_targeting_format (ad_format, campaign_id);

-- Daypart: 168-bit hour-of-week mask, one row per campaign.
-- Bit 0 = Monday 00:00 UTC, bit 167 = Sunday 23:00 UTC.

CREATE TABLE campaign_daypart (
    campaign_id   bigint   PRIMARY KEY REFERENCES campaign(id) ON DELETE CASCADE,
    hours_active  bit(168) NOT NULL
);

-- Creatives

CREATE TABLE creative (
    id          bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    campaign_id bigint      NOT NULL REFERENCES campaign(id) ON DELETE CASCADE,
    ad_format   ad_format   NOT NULL,
    click_url   text        NOT NULL,
    image_url   text,
    width       integer,
    height      integer,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_creative_campaign ON creative (campaign_id, ad_format);

-- Seed segments. Production seeding (10K-100K) ships via docker/seed-postgres.py in Phase 3.

INSERT INTO segment (name, category) VALUES
    ('auto-intender',         'auto'),
    ('auto-luxury',           'auto'),
    ('in-market-travel',      'travel'),
    ('frequent-flyer',        'travel'),
    ('cruise-shopper',        'travel'),
    ('parents',               'demographic'),
    ('young-adults',          'demographic'),
    ('millennials',           'demographic'),
    ('gen-z',                 'demographic'),
    ('homeowners',            'demographic'),
    ('high-income',           'demographic'),
    ('sports-fans',           'interest'),
    ('gamers',                'interest'),
    ('foodies',               'interest'),
    ('fitness-enthusiasts',   'interest'),
    ('tech-early-adopters',   'interest'),
    ('fashion-shoppers',      'shopping'),
    ('back-to-school',        'shopping'),
    ('holiday-shoppers',      'shopping'),
    ('small-business-owners', 'b2b');

COMMIT;

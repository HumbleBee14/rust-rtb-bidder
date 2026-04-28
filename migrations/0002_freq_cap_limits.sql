-- Migration: 0002_freq_cap_limits — per-campaign frequency cap limits.
--
-- Phase 5 hardcoded day=10, hour=3 in the freq-cap stage. These limits are a
-- per-campaign business decision (a brand-safety campaign caps tighter than a
-- prospecting campaign) and must come from the catalog.
--
-- Both columns are NOT NULL with conservative defaults so existing rows don't
-- need backfill — the defaults match the prior hardcoded values exactly so
-- production behaviour is identical until campaigns set their own limits.

ALTER TABLE campaign
    ADD COLUMN daily_cap_imps  integer NOT NULL DEFAULT 10 CHECK (daily_cap_imps  >= 0),
    ADD COLUMN hourly_cap_imps integer NOT NULL DEFAULT 3  CHECK (hourly_cap_imps >= 0);

-- A cap of 0 disables bidding for the campaign on that user once any impression
-- has been recorded; documented as the explicit "blocked" sentinel rather than
-- a separate boolean flag.
COMMENT ON COLUMN campaign.daily_cap_imps  IS '24h per-user impression cap. 0 = block after first impression.';
COMMENT ON COLUMN campaign.hourly_cap_imps IS '1h per-user impression cap. 0 = block after first impression.';

-- Platform fee: a flat naira amount, or a percentage of gross.
--
-- Flat is the default and the primary mode. Before this migration the fee was
-- always `gross * platform_fee_percent` (10%) capped by `platform_fee_cap_kobo`,
-- computed at transfer time from whatever the settings row said *then*.
--
-- Two deliberate choices here, both of which cost real money if reversed:
--
--  1. `platform_fee_flat_naira` is NAIRA, not kobo, and the column name says so.
--     Every other amount in this schema is `*_kobo BIGINT`, so an unmarked
--     `platform_fee_flat` would be read as kobo by the next person and silently
--     charge a hundredth of the intended fee. The naira -> kobo conversion
--     happens in exactly one place, `PayoutService::compute_platform_fee`.
--
--  2. `shifts.platform_fee_kobo` snapshots the fee at creation time. Payout runs
--     days or weeks after a shift is created, so recomputing from current
--     settings means an admin changing the fee retroactively re-prices work that
--     was already agreed — and the hospital was shown a net figure in
--     `POST /shifts/preview` before it committed the money. NULL means
--     "created before this column existed, fall back to current settings", which
--     keeps every existing row payable.
--
-- `platform_fee_mode` is TEXT + CHECK rather than a Postgres enum: `ALTER TYPE
-- ... ADD VALUE` cannot run inside sqlx's per-file transaction (see 20240057).
--
-- NOTE ON DEPLOY: platform_settings holds one row, so flipping the default mode
-- to 'flat' re-prices production on the next boot. 500 is a placeholder, not a
-- figure from the FRS (which only ever specifies 10%) — confirm the real launch
-- amount before this reaches Render.

ALTER TABLE platform_settings
    ADD COLUMN IF NOT EXISTS platform_fee_mode TEXT NOT NULL DEFAULT 'flat'
        CHECK (platform_fee_mode IN ('flat', 'percent')),
    ADD COLUMN IF NOT EXISTS platform_fee_flat_naira BIGINT NOT NULL DEFAULT 500
        CHECK (platform_fee_flat_naira >= 0);

ALTER TABLE shifts
    ADD COLUMN IF NOT EXISTS platform_fee_kobo BIGINT
        CHECK (platform_fee_kobo IS NULL OR platform_fee_kobo >= 0);

-- Rollback:
--   ALTER TABLE shifts DROP COLUMN IF EXISTS platform_fee_kobo;
--   ALTER TABLE platform_settings
--       DROP COLUMN IF EXISTS platform_fee_flat_naira,
--       DROP COLUMN IF EXISTS platform_fee_mode;

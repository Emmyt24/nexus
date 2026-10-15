-- Platform fee: a flat naira amount, or a percentage of gross.
--
-- Flat is the default and the primary mode. Before this migration the fee was
-- always `gross * platform_fee_percent` (10%) capped by `platform_fee_cap_kobo`,
-- computed at transfer time from whatever the settings row said *then*.
--
-- Two deliberate choices here, both of which cost real money if reversed:
--
--  1. `platform_fee_flat_kobo` is kobo, like every other stored amount here.
--     An admin sets the fee in naira and the API speaks naira throughout, but
--     the conversion happens once at the HTTP boundary (`utils::money::Naira`)
--     rather than giving one column a different unit from its neighbours. A
--     per-column exception is how a 100x error eventually ships.
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
-- DEPLOY SAFETY: platform_settings holds exactly one row, so a column default of
-- 'flat' would re-price production the moment this migration runs at boot — a
-- ₦64,000 shift would go from a ₦6,400 fee to ₦500, a 92% revenue cut, with no
-- admin action and no alert, and the wrong figure then snapshotted onto every
-- shift created afterwards.
--
-- So the column default is 'flat' (flat is the primary mode, and a fresh install
-- gets it), but the existing row is explicitly pinned to 'percent' at the bottom
-- of this file. Live pricing is unchanged until an admin opts in with
-- PUT /admin/settings, which is one call. 500 is a placeholder for the flat
-- amount, not a figure from the FRS, which only ever specifies 10%.

ALTER TABLE platform_settings
    ADD COLUMN IF NOT EXISTS platform_fee_mode TEXT NOT NULL DEFAULT 'flat'
        CHECK (platform_fee_mode IN ('flat', 'percent')),
    -- Kobo, like every other stored amount. 50000 kobo = ₦500.
    ADD COLUMN IF NOT EXISTS platform_fee_flat_kobo BIGINT NOT NULL DEFAULT 50000
        CHECK (platform_fee_flat_kobo >= 0);

ALTER TABLE shifts
    ADD COLUMN IF NOT EXISTS platform_fee_kobo BIGINT
        CHECK (platform_fee_kobo IS NULL OR platform_fee_kobo >= 0);

-- Pin the existing (production) row to the behaviour it already had. New rows
-- get 'flat' from the column default; this one changes only when an admin says
-- so. Guarded on the pre-existing value so re-running cannot undo a deliberate
-- switch to flat.
UPDATE platform_settings
   SET platform_fee_mode = 'percent'
 WHERE singleton = 'global'
   AND platform_fee_mode = 'flat'
   AND platform_fee_flat_kobo = 50000;

-- Rollback:
--   ALTER TABLE shifts DROP COLUMN IF EXISTS platform_fee_kobo;
--   ALTER TABLE platform_settings
--       DROP COLUMN IF EXISTS platform_fee_flat_kobo,
--       DROP COLUMN IF EXISTS platform_fee_mode;

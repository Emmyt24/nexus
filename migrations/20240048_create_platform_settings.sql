-- =============================================================================
-- Admin §8 — Configurable platform settings (Super Admin)
-- =============================================================================
-- Single-row table (enforced by the `singleton` primary key) holding the
-- system-wide configuration surfaced on the admin settings screen. Seeded with
-- the defaults from the spec so GET /settings always returns a row.

CREATE TABLE IF NOT EXISTS platform_settings (
    -- Always 'global' — the CHECK guarantees at most one row.
    singleton                   TEXT        PRIMARY KEY DEFAULT 'global'
                                            CHECK (singleton = 'global'),

    platform_fee_percent        NUMERIC(5,2) NOT NULL DEFAULT 10.00
                                            CHECK (platform_fee_percent >= 0 AND platform_fee_percent <= 100),
    worker_broadcast_radius_km  NUMERIC(6,2) NOT NULL DEFAULT 5.00
                                            CHECK (worker_broadcast_radius_km > 0),
    stat_bonus_percent          NUMERIC(5,2) NOT NULL DEFAULT 20.00
                                            CHECK (stat_bonus_percent >= 0),
    urgent_bonus_percent        NUMERIC(5,2) NOT NULL DEFAULT 10.00
                                            CHECK (urgent_bonus_percent >= 0),

    clock_in_grace_minutes      INTEGER     NOT NULL DEFAULT 15  CHECK (clock_in_grace_minutes >= 0),
    auto_clock_out_hours        INTEGER     NOT NULL DEFAULT 2   CHECK (auto_clock_out_hours > 0),
    handover_edit_window_hours  INTEGER     NOT NULL DEFAULT 1   CHECK (handover_edit_window_hours >= 0),
    dispute_filing_window_hours INTEGER     NOT NULL DEFAULT 24  CHECK (dispute_filing_window_hours >= 0),

    max_active_shifts_per_hospital INTEGER  NOT NULL DEFAULT 10  CHECK (max_active_shifts_per_hospital > 0),
    min_hourly_rate_kobo        BIGINT      NOT NULL DEFAULT 200000 CHECK (min_hourly_rate_kobo >= 0), -- ₦2,000
    max_recording_minutes       INTEGER     NOT NULL DEFAULT 15  CHECK (max_recording_minutes > 0),

    updated_by                  UUID        REFERENCES users (id) ON DELETE SET NULL,
    updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed the single global row with defaults (no-op if it already exists).
INSERT INTO platform_settings (singleton) VALUES ('global')
    ON CONFLICT (singleton) DO NOTHING;

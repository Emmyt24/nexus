-- =============================================================================
-- Admin §4.3 / §5.4 — Shift disputes
-- =============================================================================
-- A dispute is filed by a hospital or worker within the dispute window after a
-- shift, and resolved by an Operations/Finance admin. Resolution decides how
-- the shift is paid out (full / partial / none) or escalates to a super admin.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'dispute_status') THEN
        CREATE TYPE dispute_status AS ENUM ('open', 'resolved', 'closed');
    END IF;

    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'dispute_priority') THEN
        CREATE TYPE dispute_priority AS ENUM ('low', 'medium', 'high');
    END IF;

    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'dispute_filed_by') THEN
        CREATE TYPE dispute_filed_by AS ENUM ('hospital', 'worker');
    END IF;

    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'dispute_resolution') THEN
        CREATE TYPE dispute_resolution AS ENUM (
            'full_payment',    -- worker paid in full, hospital billed
            'partial_refund',  -- worker paid X%, hospital refunded the rest
            'no_payment',      -- worker not paid, hospital not billed
            'escalate'         -- sent to super admin for final decision
        );
    END IF;
END$$;

CREATE TABLE IF NOT EXISTS disputes (
    id                 UUID              PRIMARY KEY DEFAULT gen_random_uuid(),
    shift_id           UUID              NOT NULL REFERENCES shifts (id) ON DELETE CASCADE,
    hospital_id        UUID              NOT NULL REFERENCES hospitals (id) ON DELETE CASCADE,
    worker_id          UUID              REFERENCES users (id) ON DELETE SET NULL,

    filed_by           dispute_filed_by  NOT NULL,
    filed_by_user_id   UUID              REFERENCES users (id) ON DELETE SET NULL,
    reason             TEXT              NOT NULL,
    -- Free-form evidence links / notes captured at filing time.
    evidence           JSONB             NOT NULL DEFAULT '[]'::jsonb,

    status             dispute_status    NOT NULL DEFAULT 'open',
    priority           dispute_priority  NOT NULL DEFAULT 'medium',
    -- Amount in dispute (kobo), used to display "amount involved".
    amount_kobo        BIGINT            CHECK (amount_kobo IS NULL OR amount_kobo >= 0),

    -- Populated on resolution.
    resolution         dispute_resolution,
    resolution_amount_kobo BIGINT        CHECK (resolution_amount_kobo IS NULL OR resolution_amount_kobo >= 0),
    admin_notes        TEXT,
    resolved_by        UUID              REFERENCES users (id) ON DELETE SET NULL,
    resolved_at        TIMESTAMPTZ,

    created_at         TIMESTAMPTZ       NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ       NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_disputes_status
    ON disputes (status, priority DESC, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_disputes_shift
    ON disputes (shift_id);

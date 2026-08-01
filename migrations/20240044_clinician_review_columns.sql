-- Manual license-review attribution + suspend metadata for clinicians (Admin §2).
-- is_verified / is_active already exist; these columns record who reviewed and why.
ALTER TABLE clinicians
    ADD COLUMN IF NOT EXISTS reviewed_by      UUID REFERENCES users (id),
    ADD COLUMN IF NOT EXISTS reviewed_at      TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS review_notes     TEXT,
    ADD COLUMN IF NOT EXISTS suspended_at     TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS suspended_reason TEXT;

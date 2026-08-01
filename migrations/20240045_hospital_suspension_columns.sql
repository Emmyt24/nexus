-- Suspend metadata for hospitals (who suspended, when, and why) (Admin §1.2).
ALTER TABLE hospitals
    ADD COLUMN IF NOT EXISTS suspended_at     TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS suspended_by     UUID REFERENCES users (id),
    ADD COLUMN IF NOT EXISTS suspended_reason TEXT;

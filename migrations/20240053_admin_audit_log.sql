-- Audit trail for privileged admin actions (suspend, verify, cancel, payouts,
-- admin management) so every mutation records the acting admin (Admin §4.3).
CREATE TABLE IF NOT EXISTS admin_actions_log (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    actor_id    UUID        NOT NULL REFERENCES users (id),
    action      TEXT        NOT NULL,
    target_type TEXT,
    target_id   UUID,
    metadata    JSONB,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for listing a given admin's recent actions.
CREATE INDEX IF NOT EXISTS idx_admin_actions_actor
    ON admin_actions_log (actor_id, created_at DESC);

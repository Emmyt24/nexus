-- E4 — Push notification infrastructure.
-- General-purpose notification center backing store. Distinct from the
-- onboarding-scoped `onboarding_notifications` table, which is left untouched.

CREATE TABLE IF NOT EXISTS notifications (
    id          UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID         NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Stable event key, e.g. 'shift_offered', 'shift_accepted', 'interest_expressed'.
    kind        VARCHAR(64)  NOT NULL,
    title       VARCHAR(200) NOT NULL,
    body        TEXT         NOT NULL,
    -- Arbitrary structured payload for deep-linking (e.g. { "shift_id": "..." }).
    data        JSONB        NOT NULL DEFAULT '{}'::jsonb,
    read_at     TIMESTAMPTZ,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- Notification-center listing: newest first, with an unread fast-path.
CREATE INDEX IF NOT EXISTS idx_notifications_user_created
    ON notifications (user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_user_unread
    ON notifications (user_id)
    WHERE read_at IS NULL;

-- Rollback:
--   DROP TABLE IF EXISTS notifications;

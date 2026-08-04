-- E4 — Push notification infrastructure.
-- Device push tokens (FCM/APNs-via-FCM) registered per user. Additive.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'device_platform') THEN
        CREATE TYPE device_platform AS ENUM ('ios', 'android', 'web');
    END IF;
END$$;

CREATE TABLE IF NOT EXISTS device_tokens (
    id           UUID             PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      UUID             NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    platform     device_platform  NOT NULL,
    token        TEXT             NOT NULL,
    created_at   TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    -- Set when FCM reports the token invalid/unregistered, so it is skipped
    -- without being deleted (kept for audit).
    revoked_at   TIMESTAMPTZ,

    CONSTRAINT uq_device_user_token UNIQUE (user_id, token)
);

-- Active-token lookups when dispatching a push to a user.
CREATE INDEX IF NOT EXISTS idx_device_tokens_active
    ON device_tokens (user_id)
    WHERE revoked_at IS NULL;

-- Rollback:
--   DROP TABLE IF EXISTS device_tokens;
--   DROP TYPE  IF EXISTS device_platform;

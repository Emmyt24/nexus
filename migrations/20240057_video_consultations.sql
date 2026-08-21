-- =============================================================================
-- LiveKit video consultations — core consult flow.
--
--   video_sessions              one LiveKit room per consult.
--                               shift_id NULL == ad-hoc consult (future scope).
--   video_session_participants  one row per (session, LiveKit identity).
--   video_session_events        append-only NDPR audit trail.
--
-- Webhook idempotency reuses the existing `webhook_events` table with
-- provider = 'livekit' (20240032_hospital_wallet.sql:124).
--
-- Status / role columns are TEXT + CHECK rather than Postgres enums on
-- purpose: ad-hoc consults, patient guests and egress recording all add values
-- later, and extending a pg enum needs its own standalone migration file
-- (ALTER TYPE ... ADD VALUE cannot run inside sqlx's per-file transaction).
-- =============================================================================

CREATE TABLE IF NOT EXISTS video_sessions (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),

    -- NULL == ad-hoc consult not tied to a shift (future scope). Postgres treats
    -- NULLs as distinct in a UNIQUE constraint, so uq_video_sessions_shift means
    -- "at most one session per shift, unlimited ad-hoc sessions" for free.
    shift_id            UUID        REFERENCES shifts (id) ON DELETE CASCADE,
    hospital_id         UUID        NOT NULL REFERENCES hospitals (id),
    created_by          UUID        REFERENCES users (id),

    -- STORED, not derived, so the naming scheme can change, a room can be
    -- rotated after abuse, and ad-hoc rooms need no code branching.
    room_name           TEXT        NOT NULL,
    livekit_room_sid    TEXT,

    status              TEXT        NOT NULL DEFAULT 'pending'
                                    CHECK (status IN ('pending','active','ended','failed')),

    max_participants    INTEGER     NOT NULL DEFAULT 4 CHECK (max_participants > 0),
    departure_timeout_s INTEGER     NOT NULL DEFAULT 120,
    empty_timeout_s     INTEGER     NOT NULL DEFAULT 900,

    started_at          TIMESTAMPTZ,
    ended_at            TIMESTAMPTZ,
    -- 'room_finished' | 'ended_by_hospital' | 'reconciled_missing'
    ended_reason        TEXT,

    -- --- Egress / recording seam (FUTURE SCOPE; phase 1 writes FALSE/NULL) ---
    -- Session *policy* lives here. The recording artefacts (egress id, storage
    -- location, duration) will go in a separate `video_recordings` table keyed
    -- by session_id, because one session can produce several egresses.
    -- NDPR consent clause 2 means recording_consent MUST be non-null before any
    -- egress is started.
    recording_enabled   BOOLEAN     NOT NULL DEFAULT FALSE,
    recording_consent   JSONB,
    -- -----------------------------------------------------------------------

    metadata            JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT uq_video_sessions_room  UNIQUE (room_name),
    CONSTRAINT uq_video_sessions_shift UNIQUE (shift_id)
);

CREATE INDEX IF NOT EXISTS idx_video_sessions_hospital
    ON video_sessions (hospital_id, created_at DESC);
-- Drives the reconciler sweep.
CREATE INDEX IF NOT EXISTS idx_video_sessions_open
    ON video_sessions (updated_at) WHERE status IN ('pending','active');

DROP TRIGGER IF EXISTS trg_video_sessions_updated_at ON video_sessions;
CREATE TRIGGER trg_video_sessions_updated_at
    BEFORE UPDATE ON video_sessions
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();


CREATE TABLE IF NOT EXISTS video_session_participants (
    id                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id        UUID        NOT NULL REFERENCES video_sessions (id) ON DELETE CASCADE,

    -- The LiveKit identity we minted a token for: "u:<user_uuid>".
    identity          TEXT        NOT NULL,
    -- NULL for future non-user participants (patient guest link, AI agent).
    user_id           UUID        REFERENCES users (id),
    clinician_id      UUID        REFERENCES clinicians (id),
    display_name      TEXT,

    participant_role  TEXT        NOT NULL
                                  CHECK (participant_role IN
                                      ('clinician','hospital_observer','patient','agent')),
    can_publish       BOOLEAN     NOT NULL DEFAULT TRUE,

    -- Lets the reconciler tell "never asked for a token" apart from "asked but
    -- the participant_joined webhook never arrived".
    token_issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    token_expires_at  TIMESTAMPTZ NOT NULL,
    token_issue_count INTEGER     NOT NULL DEFAULT 1,

    participant_sid   TEXT,
    joined_at         TIMESTAMPTZ,
    left_at           TIMESTAMPTZ,
    disconnect_reason TEXT,
    -- Second idempotency layer: claimed by a single
    -- UPDATE ... WHERE clocked_in_at IS NULL RETURNING id, so concurrent
    -- deliveries are resolved by a row lock before the attendance write.
    clocked_in_at     TIMESTAMPTZ,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT uq_video_participant UNIQUE (session_id, identity)
);

CREATE INDEX IF NOT EXISTS idx_video_participants_session
    ON video_session_participants (session_id);
CREATE INDEX IF NOT EXISTS idx_video_participants_pending_join
    ON video_session_participants (token_issued_at) WHERE joined_at IS NULL;

DROP TRIGGER IF EXISTS trg_video_session_participants_updated_at ON video_session_participants;
CREATE TRIGGER trg_video_session_participants_updated_at
    BEFORE UPDATE ON video_session_participants
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();


-- Append-only NDPR audit trail. This CANNOT live in admin_actions_log — that
-- table declares `actor_id UUID NOT NULL REFERENCES users (id)`
-- (20240053_admin_audit_log.sql:5) and a webhook has no user actor.
CREATE TABLE IF NOT EXISTS video_session_events (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Nullable: an event can arrive for a room whose session row is gone, and we
    -- still want the audit line.
    session_id       UUID        REFERENCES video_sessions (id) ON DELETE SET NULL,
    room_name        TEXT        NOT NULL,
    -- A LiveKit event name, or an internal action ('token_issued',
    -- 'ended_by_hospital', 'clockin_recorded', 'clockin_skipped:<reason>').
    event_type       TEXT        NOT NULL,
    identity         TEXT,
    -- Set only for operator-initiated actions; always NULL for webhook rows.
    actor_user_id    UUID        REFERENCES users (id),
    livekit_event_id TEXT,
    payload          JSONB,
    -- LiveKit's WebhookEvent.created_at, NOT our clock — needed to detect
    -- out-of-order delivery.
    occurred_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_video_events_session
    ON video_session_events (session_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_video_events_room
    ON video_session_events (room_name, occurred_at DESC);

-- Rollback:
--   DROP TABLE IF EXISTS video_session_events;
--   DROP TABLE IF EXISTS video_session_participants;
--   DROP TABLE IF EXISTS video_sessions;
--   DELETE FROM webhook_events WHERE provider = 'livekit';

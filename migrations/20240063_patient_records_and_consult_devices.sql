-- =============================================================================
-- Patient records, per-patient handover notes, the consult waiting room, and
-- multi-device consultations ("Continue on phone").
--
--   consult_patient_queue    the waiting room: patients queued into a session.
--   consultation_notes       EXTENDED (not duplicated) so the doctor's existing
--                            SOAP note can belong to a shift and be locked.
--   patient_handover_notes   the on-site hospital party's note. Exactly one per
--                            (shift, patient).
--   consult_device_handoffs  single-use codes exchanged for a companion-device
--                            join token.
--
-- This normalises `shift_handovers.critical_patients`
-- (20240024_shift_marketplace_v2.sql:82), which is an untyped JSONB array with
-- no foreign key to `patients`. That column is left in place: this release
-- complements it and attempts no backfill.
--
-- Status columns are TEXT + CHECK rather than Postgres enums, for the reason
-- recorded in 20240057_video_consultations.sql:12 — `ALTER TYPE ... ADD VALUE`
-- cannot run inside sqlx's per-file transaction, so every future value would
-- otherwise need a migration file of its own.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- consult_patient_queue — the waiting room.
--
-- Patients do NOT join the LiveKit room. These are queue records only: the
-- patient is physically with the on-site worker, so `video_session_participants`
-- is untouched and the dormant 'patient' participant_role stays unused.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS consult_patient_queue (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID        NOT NULL REFERENCES video_sessions (id) ON DELETE CASCADE,
    -- Denormalised from the session so the roll-up read never needs the join.
    -- NULL mirrors video_sessions.shift_id being NULL for an ad-hoc consult.
    shift_id    UUID        REFERENCES shifts (id) ON DELETE CASCADE,
    patient_id  UUID        NOT NULL REFERENCES patients (id),
    -- The tenant key. Carried here so a queue read is scopeable without
    -- reaching through two tables.
    hospital_id UUID        NOT NULL REFERENCES hospitals (id),

    state       TEXT        NOT NULL DEFAULT 'waiting'
                            CHECK (state IN ('waiting','in_consult','seen','removed')),
    -- Display order within the queue. Deliberately NOT unique: removing an
    -- entry would otherwise force a renumber of everything behind it.
    position    INTEGER     NOT NULL,

    added_by    UUID        NOT NULL REFERENCES users (id),
    called_at   TIMESTAMPTZ,
    seen_at     TIMESTAMPTZ,
    removed_at  TIMESTAMPTZ,

    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT uq_consult_queue_patient UNIQUE (session_id, patient_id)
);

CREATE INDEX IF NOT EXISTS idx_consult_queue_session
    ON consult_patient_queue (session_id, state, position);
CREATE INDEX IF NOT EXISTS idx_consult_queue_patient
    ON consult_patient_queue (patient_id);

-- The doctor attends one patient at a time. This is what makes the waiting
-- count meaningful: without it "waiting: 4" could coexist with three patients
-- simultaneously in consult.
CREATE UNIQUE INDEX IF NOT EXISTS uq_consult_queue_one_in_consult
    ON consult_patient_queue (session_id) WHERE state = 'in_consult';

DROP TRIGGER IF EXISTS trg_consult_patient_queue_updated_at ON consult_patient_queue;
CREATE TRIGGER trg_consult_patient_queue_updated_at
    BEFORE UPDATE ON consult_patient_queue
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();


-- ---------------------------------------------------------------------------
-- consultation_notes — EXTENDED, not duplicated.
--
-- 20240062 already holds chief_complaint, history_of_present_illness,
-- assessment and plan, plus the voice transcript and its per-chunk segments. A
-- second table for the same doctor's notes about the same patient would be a
-- data-integrity problem, not a feature.
-- ---------------------------------------------------------------------------
ALTER TABLE consultation_notes
    -- Nullable: rows written before this migration belong to no shift, and an
    -- ad-hoc note outside any shift stays legal.
    ADD COLUMN IF NOT EXISTS shift_id       UUID REFERENCES shifts (id) ON DELETE SET NULL,
    -- SET NULL, not CASCADE: the clinical record must outlive the video session
    -- it happened to be written during.
    ADD COLUMN IF NOT EXISTS session_id     UUID REFERENCES video_sessions (id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS diagnosis      TEXT,
    -- Free-form so a note is never blocked on a vitals schema the ML model owns.
    ADD COLUMN IF NOT EXISTS vitals         JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN IF NOT EXISTS medications    JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN IF NOT EXISTS follow_up_at   TIMESTAMPTZ,
    -- An amendment after the window closes is a new row pointing at the
    -- original, which is never rewritten.
    ADD COLUMN IF NOT EXISTS amends_note_id UUID REFERENCES consultation_notes (id),
    -- Nullable for the same reason as shift_id: existing rows have no window.
    -- NULL means "not governed by a window" and MUST be treated as closed, not
    -- open, by the update guard — note that a bare `editable_until > NOW()`
    -- yields NULL (not TRUE) for those rows, so the guard spells out
    -- `IS NOT NULL` rather than relying on that.
    ADD COLUMN IF NOT EXISTS editable_until TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_consultation_notes_shift
    ON consultation_notes (shift_id, patient_id, created_at DESC)
    WHERE shift_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_consultation_notes_amends
    ON consultation_notes (amends_note_id) WHERE amends_note_id IS NOT NULL;

-- NOTE: consultation_notes.status stays a bare VARCHAR(20) with no CHECK
-- ('recording' | 'completed' | 'failed', validated in Rust). Adding a CHECK is
-- a separate, riskier change against existing rows.


-- ---------------------------------------------------------------------------
-- patient_handover_notes — the on-site hospital party's note, one per patient.
--
-- Written by the hospital admin in the consult room (participant_role
-- 'hospital_observer'), which is the "health worker on site". UNIQUE on
-- (shift_id, patient_id) makes the submit idempotent, exactly like
-- shift_handovers' UNIQUE (shift_id).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS patient_handover_notes (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    shift_id            UUID        NOT NULL REFERENCES shifts (id) ON DELETE CASCADE,
    patient_id          UUID        NOT NULL REFERENCES patients (id) ON DELETE CASCADE,
    session_id          UUID        REFERENCES video_sessions (id) ON DELETE SET NULL,
    hospital_id         UUID        NOT NULL REFERENCES hospitals (id),
    author_user_id      UUID        NOT NULL REFERENCES users (id),

    summary             TEXT        NOT NULL,
    outstanding_tasks   JSONB       NOT NULL DEFAULT '[]'::jsonb,
    medications_given   JSONB       NOT NULL DEFAULT '[]'::jsonb,
    escalation_required BOOLEAN     NOT NULL DEFAULT FALSE,
    escalation_reason   TEXT,

    submitted_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Same window and same mechanism as shift_handovers.editable_until, so the
    -- two handover surfaces cannot drift.
    editable_until      TIMESTAMPTZ NOT NULL,

    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT uq_patient_handover UNIQUE (shift_id, patient_id)
);

CREATE INDEX IF NOT EXISTS idx_patient_handover_shift
    ON patient_handover_notes (shift_id);
CREATE INDEX IF NOT EXISTS idx_patient_handover_patient
    ON patient_handover_notes (patient_id, submitted_at DESC);

DROP TRIGGER IF EXISTS trg_patient_handover_notes_updated_at ON patient_handover_notes;
CREATE TRIGGER trg_patient_handover_notes_updated_at
    BEFORE UPDATE ON patient_handover_notes
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();


-- ---------------------------------------------------------------------------
-- consult_device_handoffs — "Continue on phone".
--
-- The redeeming device holds no app JWT: the code IS the credential. So it is
-- stored only as a SHA-256 hash (the pattern from
-- 20240039_identity_number_hash.sql and the email OTP codes), carries a short
-- expiry, and is single-use — claimed by one
-- `UPDATE ... WHERE redeemed_at IS NULL AND expires_at > NOW() RETURNING *`,
-- so the row lock resolves concurrent redemptions.
--
-- No updated_at/trigger: a handoff is written once and claimed once.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS consult_device_handoffs (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id       UUID        NOT NULL REFERENCES video_sessions (id) ON DELETE CASCADE,
    -- Who the companion device will act as. The redeemed token is minted for
    -- this user and no other; nothing in the request can change it.
    user_id          UUID        NOT NULL REFERENCES users (id),

    code_hash        TEXT        NOT NULL,
    -- Which device slot this grant is for; feeds identity "u:<uuid>#d<n>".
    -- >= 2 because slot 1 is the primary device, which never needs a handoff.
    device_ordinal   INTEGER     NOT NULL CHECK (device_ordinal >= 2),

    participant_role TEXT        NOT NULL
                                 CHECK (participant_role IN
                                     ('clinician','hospital_observer','patient','agent')),
    mode             TEXT        NOT NULL DEFAULT 'participant'
                                 CHECK (mode IN ('participant','observer')),
    device_label     TEXT,

    expires_at       TIMESTAMPTZ NOT NULL,
    redeemed_at      TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT uq_consult_handoff_code UNIQUE (code_hash)
);

CREATE INDEX IF NOT EXISTS idx_consult_handoff_session
    ON consult_device_handoffs (session_id, user_id);
-- Drives expiry housekeeping without scanning spent codes.
CREATE INDEX IF NOT EXISTS idx_consult_handoff_live
    ON consult_device_handoffs (expires_at) WHERE redeemed_at IS NULL;


-- ---------------------------------------------------------------------------
-- video_session_participants: one row per DEVICE, not per person.
--
-- Identity was "u:<user_uuid>" and UNIQUE (session_id, identity) therefore
-- meant one row per user. A companion device joins as "u:<user_uuid>#d2", which
-- is a second row for the same user_id.
-- ---------------------------------------------------------------------------
ALTER TABLE video_session_participants
    ADD COLUMN IF NOT EXISTS device_ordinal INTEGER NOT NULL DEFAULT 1
        CHECK (device_ordinal >= 1),
    ADD COLUMN IF NOT EXISTS device_label   TEXT;

-- The clock-in belongs to the PERSON, not the device. The service claims the
-- slot on the user's primary row so `claim_clockin_slot`'s single-row lock keeps
-- its guarantee; this index is the database-level backstop, so a second
-- clock-in for one user in one session cannot exist even if that is bypassed.
CREATE UNIQUE INDEX IF NOT EXISTS uq_video_participant_user_clockin
    ON video_session_participants (session_id, user_id)
    WHERE clocked_in_at IS NOT NULL AND user_id IS NOT NULL;

-- Headroom for one companion device each. Two users x two devices is exactly
-- the old cap of 4. Sessions created before this migration keep their stored
-- value: LiveKit's create_room on an EXISTING room returns it without
-- re-applying options, so a live room's cap cannot be raised.
ALTER TABLE video_sessions ALTER COLUMN max_participants SET DEFAULT 6;

-- Rollback:
--   ALTER TABLE video_sessions ALTER COLUMN max_participants SET DEFAULT 4;
--   DROP INDEX IF EXISTS uq_video_participant_user_clockin;
--   ALTER TABLE video_session_participants
--       DROP COLUMN IF EXISTS device_label,
--       DROP COLUMN IF EXISTS device_ordinal;
--   DROP TABLE IF EXISTS consult_device_handoffs;
--   DROP TABLE IF EXISTS patient_handover_notes;
--   DROP INDEX IF EXISTS idx_consultation_notes_amends;
--   DROP INDEX IF EXISTS idx_consultation_notes_shift;
--   ALTER TABLE consultation_notes
--       DROP COLUMN IF EXISTS editable_until,
--       DROP COLUMN IF EXISTS amends_note_id,
--       DROP COLUMN IF EXISTS follow_up_at,
--       DROP COLUMN IF EXISTS medications,
--       DROP COLUMN IF EXISTS vitals,
--       DROP COLUMN IF EXISTS diagnosis,
--       DROP COLUMN IF EXISTS session_id,
--       DROP COLUMN IF EXISTS shift_id;
--   DROP TABLE IF EXISTS consult_patient_queue;

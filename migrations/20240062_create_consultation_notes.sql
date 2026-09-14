-- Consultation notes — voice-recorded doctor/patient encounters.
-- One row per consultation: a clinician starts a note against a patient,
-- audio is captured client-side in short chunks and transcribed by
-- ml-service's Whisper endpoint as they arrive (consultation_transcript_segments,
-- one row per chunk), and full_transcript is the running concatenation the
-- clinician references while filling in the structured SOAP fields by hand
-- (see docs discussion — no LLM summarization in this phase).
--
-- Mirrors patient_predictions' status/lifecycle shape (status + timestamps,
-- 20240041_create_patient_predictions.sql) rather than a Postgres ENUM, for
-- the same reason: validated at the app layer, cheap to add a status later.

CREATE TABLE consultation_notes (
    id                          UUID          PRIMARY KEY DEFAULT gen_random_uuid(),
    patient_id                  UUID          NOT NULL REFERENCES patients (id),
    hospital_id                 UUID          NOT NULL REFERENCES hospitals (id),
    recorded_by                 UUID          NOT NULL REFERENCES users (id),

    status                      VARCHAR(20)   NOT NULL DEFAULT 'recording',
    -- 'recording' -> 'completed', or 'failed' if transcription errors out
    -- badly enough that the clinician abandons it (rare — a single failed
    -- chunk doesn't fail the whole note, see consultation_transcript_segments).

    full_transcript              TEXT         NOT NULL DEFAULT '',
    language                     VARCHAR(10)  NOT NULL DEFAULT 'en',
    duration_seconds             INTEGER      NOT NULL DEFAULT 0,

    -- Structured note fields — filled in by the clinician (not AI-generated;
    -- see docs/consultation-notes.md), the transcript above is their reference.
    chief_complaint               TEXT,
    history_of_present_illness    TEXT,
    assessment                    TEXT,
    plan                          TEXT,

    created_at                   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at                   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    completed_at                  TIMESTAMPTZ
);

CREATE INDEX idx_consultation_notes_patient_id  ON consultation_notes (patient_id);
CREATE INDEX idx_consultation_notes_hospital_id ON consultation_notes (hospital_id);
CREATE INDEX idx_consultation_notes_recorded_by ON consultation_notes (recorded_by);

CREATE TRIGGER trg_consultation_notes_updated_at
    BEFORE UPDATE ON consultation_notes
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- One row per audio chunk uploaded during a consultation. Kept separate from
-- consultation_notes (rather than just appending to full_transcript in
-- place) so a single chunk's transcription failure is visible/retryable
-- without losing the rest of the session, and so the raw per-chunk audio
-- path is traceable back to what produced each piece of the transcript.
CREATE TABLE consultation_transcript_segments (
    id                     UUID          PRIMARY KEY DEFAULT gen_random_uuid(),
    consultation_note_id   UUID          NOT NULL REFERENCES consultation_notes (id) ON DELETE CASCADE,
    sequence               INTEGER       NOT NULL,

    -- Path to the stored audio chunk on local disk (uploads/consultations/{note_id}/chunk_{sequence}.webm)
    -- — see docs/consultation-notes.md for why local disk vs. object storage.
    audio_path              TEXT         NOT NULL,
    audio_duration_seconds   REAL,

    transcript_text          TEXT        NOT NULL DEFAULT '',
    status                   VARCHAR(20) NOT NULL DEFAULT 'completed',
    -- 'completed' or 'failed' — a failed chunk keeps transcript_text empty
    -- and last_error set; the clinician still has every other chunk's text.
    last_error                TEXT,

    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT uq_consultation_segment_sequence UNIQUE (consultation_note_id, sequence)
);

CREATE INDEX idx_consultation_segments_note_id ON consultation_transcript_segments (consultation_note_id, sequence);

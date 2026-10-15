//! SQL for per-patient handover notes and the consult waiting room.
//!
//! The doctor's clinical note is **not** here — `consultation_note.rs` owns
//! that table.
//!
//! No business rules: the state-transition guards live inside the statements
//! (`WHERE id = $1 AND state = $2`) rather than in a read-then-write above
//! them, so a concurrent transition loses the race instead of both appearing
//! to succeed.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::patient_record::{
    ConsultQueueEntry, ConsultQueueEntryWithPatient, PatientHandoverNote, QueueEntryState,
};

const QUEUE_COLUMNS: &str = r#"
    id, session_id, shift_id, patient_id, hospital_id, state, position,
    added_by, called_at, seen_at, removed_at, created_at, updated_at
"#;

const HANDOVER_COLUMNS: &str = r#"
    id, shift_id, patient_id, session_id, hospital_id, author_user_id, summary,
    outstanding_tasks, medications_given, escalation_required,
    escalation_reason, submitted_at, editable_until, created_at, updated_at
"#;

/// Per-state counts for one session's waiting room.
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct QueueCounts {
    pub waiting: i64,
    pub in_consult: i64,
    pub seen: i64,
    pub total: i64,
}

/// A patient the shift touched, for the hospital's roll-up read.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ShiftPatientRow {
    pub patient_id: Uuid,
    pub full_name: String,
    pub age: f32,
    pub gender: String,
    pub severity_level: String,
    pub queue_state: Option<QueueEntryState>,
}

pub struct PatientRecordRepository {
    pool: PgPool,
}

impl PatientRecordRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // Waiting room

    /// Queue a patient. Runs in the caller's transaction so intaking a new
    /// patient, queueing its ML prediction and enqueueing it either all land or
    /// none do — the same reasoning as `PatientRepository::create`.
    ///
    /// `position` is assigned inside the statement: computing it outside would
    /// race two concurrent adds to the same number.
    pub async fn enqueue_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        shift_id: Option<Uuid>,
        patient_id: Uuid,
        hospital_id: Uuid,
        added_by: Uuid,
    ) -> Result<ConsultQueueEntry, sqlx::Error> {
        sqlx::query_as::<_, ConsultQueueEntry>(&format!(
            r#"
            INSERT INTO consult_patient_queue
                (session_id, shift_id, patient_id, hospital_id, position, added_by)
            SELECT $1, $2, $3, $4,
                   COALESCE((SELECT MAX(position) FROM consult_patient_queue
                              WHERE session_id = $1), 0) + 1,
                   $5
            RETURNING {QUEUE_COLUMNS}
            "#
        ))
        .bind(session_id)
        .bind(shift_id)
        .bind(patient_id)
        .bind(hospital_id)
        .bind(added_by)
        .fetch_one(&mut **tx)
        .await
    }

    pub async fn find_entry(
        &self,
        entry_id: Uuid,
    ) -> Result<Option<ConsultQueueEntry>, sqlx::Error> {
        sqlx::query_as::<_, ConsultQueueEntry>(&format!(
            r#"SELECT {QUEUE_COLUMNS} FROM consult_patient_queue WHERE id = $1"#
        ))
        .bind(entry_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn find_entry_for_patient(
        &self,
        session_id: Uuid,
        patient_id: Uuid,
    ) -> Result<Option<ConsultQueueEntry>, sqlx::Error> {
        sqlx::query_as::<_, ConsultQueueEntry>(&format!(
            r#"
            SELECT {QUEUE_COLUMNS} FROM consult_patient_queue
             WHERE session_id = $1 AND patient_id = $2
            "#
        ))
        .bind(session_id)
        .bind(patient_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Move an entry between states, but only from the state the caller
    /// believes it is in. Returns `None` for an illegal transition — a silent
    /// success there would let two clients both think they called a patient.
    ///
    /// The timestamps use `COALESCE` so a repeated transition cannot move one
    /// that is already set.
    pub async fn transition_state(
        &self,
        entry_id: Uuid,
        from: QueueEntryState,
        to: QueueEntryState,
    ) -> Result<Option<ConsultQueueEntry>, sqlx::Error> {
        sqlx::query_as::<_, ConsultQueueEntry>(&format!(
            r#"
            UPDATE consult_patient_queue
               SET state      = $3,
                   called_at  = CASE WHEN $3 = 'in_consult'
                                     THEN COALESCE(called_at, NOW()) ELSE called_at END,
                   seen_at    = CASE WHEN $3 = 'seen'
                                     THEN COALESCE(seen_at, NOW())   ELSE seen_at END,
                   removed_at = CASE WHEN $3 = 'removed'
                                     THEN COALESCE(removed_at, NOW()) ELSE removed_at END,
                   updated_at = NOW()
             WHERE id = $1 AND state = $2
            RETURNING {QUEUE_COLUMNS}
            "#
        ))
        .bind(entry_id)
        .bind(from.as_str())
        .bind(to.as_str())
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn counts_for_session(&self, session_id: Uuid) -> Result<QueueCounts, sqlx::Error> {
        sqlx::query_as::<_, QueueCounts>(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE state = 'waiting')    AS waiting,
                COUNT(*) FILTER (WHERE state = 'in_consult') AS in_consult,
                COUNT(*) FILTER (WHERE state = 'seen')       AS seen,
                COUNT(*)                                     AS total
              FROM consult_patient_queue
             WHERE session_id = $1
            "#,
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
    }

    /// The waiting-room list, joined to triage data and to whether each note
    /// has been filed, so the UI needs no follow-up request per patient.
    pub async fn list_for_session(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<ConsultQueueEntryWithPatient>, sqlx::Error> {
        sqlx::query_as::<_, ConsultQueueEntryWithPatient>(
            r#"
            SELECT q.id, q.session_id, q.patient_id, q.state, q.position,
                   q.called_at, q.seen_at, q.created_at,
                   p.full_name, p.age, p.gender, p.severity_level,
                   p.predictive_risk_score,
                   EXISTS (SELECT 1 FROM consultation_notes n
                            WHERE n.patient_id = q.patient_id
                              AND n.shift_id   = q.shift_id) AS has_clinical_note,
                   EXISTS (SELECT 1 FROM patient_handover_notes h
                            WHERE h.patient_id = q.patient_id
                              AND h.shift_id   = q.shift_id) AS has_handover_note
              FROM consult_patient_queue q
              JOIN patients p ON p.id = q.patient_id
             WHERE q.session_id = $1
             ORDER BY q.position ASC
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
    }

    // Per-patient handover notes

    /// Upsert on `(shift_id, patient_id)`, so a resubmit edits the one row.
    /// `DO UPDATE` is guarded on the window: past `editable_until` the
    /// statement updates nothing and returns `None`, which the service turns
    /// into a 409 rather than a silent no-op.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_handover_note(
        &self,
        shift_id: Uuid,
        patient_id: Uuid,
        session_id: Option<Uuid>,
        hospital_id: Uuid,
        author_user_id: Uuid,
        summary: &str,
        outstanding_tasks: &serde_json::Value,
        medications_given: &serde_json::Value,
        escalation_required: bool,
        escalation_reason: Option<&str>,
    ) -> Result<Option<PatientHandoverNote>, sqlx::Error> {
        sqlx::query_as::<_, PatientHandoverNote>(&format!(
            r#"
            INSERT INTO patient_handover_notes (
                shift_id, patient_id, session_id, hospital_id, author_user_id,
                summary, outstanding_tasks, medications_given,
                escalation_required, escalation_reason,
                submitted_at, editable_until
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    NOW(), NOW() + INTERVAL '1 hour')
            ON CONFLICT (shift_id, patient_id) DO UPDATE
               SET summary             = EXCLUDED.summary,
                   outstanding_tasks   = EXCLUDED.outstanding_tasks,
                   medications_given   = EXCLUDED.medications_given,
                   escalation_required = EXCLUDED.escalation_required,
                   escalation_reason   = EXCLUDED.escalation_reason,
                   session_id          = COALESCE(EXCLUDED.session_id,
                                                  patient_handover_notes.session_id),
                   submitted_at        = NOW(),
                   updated_at          = NOW()
             WHERE patient_handover_notes.editable_until > NOW()
            RETURNING {HANDOVER_COLUMNS}
            "#
        ))
        .bind(shift_id)
        .bind(patient_id)
        .bind(session_id)
        .bind(hospital_id)
        .bind(author_user_id)
        .bind(summary)
        .bind(outstanding_tasks)
        .bind(medications_given)
        .bind(escalation_required)
        .bind(escalation_reason)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn find_handover_note(
        &self,
        shift_id: Uuid,
        patient_id: Uuid,
    ) -> Result<Option<PatientHandoverNote>, sqlx::Error> {
        sqlx::query_as::<_, PatientHandoverNote>(&format!(
            r#"
            SELECT {HANDOVER_COLUMNS} FROM patient_handover_notes
             WHERE shift_id = $1 AND patient_id = $2
            "#
        ))
        .bind(shift_id)
        .bind(patient_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn list_handover_notes_for_shift(
        &self,
        shift_id: Uuid,
    ) -> Result<Vec<PatientHandoverNote>, sqlx::Error> {
        sqlx::query_as::<_, PatientHandoverNote>(&format!(
            r#"
            SELECT {HANDOVER_COLUMNS} FROM patient_handover_notes
             WHERE shift_id = $1
             ORDER BY submitted_at ASC
            "#
        ))
        .bind(shift_id)
        .fetch_all(&self.pool)
        .await
    }

    // Roll-up

    /// Every patient the shift touched — queued, noted, or handed over.
    ///
    /// The `UNION` covers a patient whose queue row was cleaned up but whose
    /// clinical record remains: `consultation_notes.shift_id` is
    /// `ON DELETE SET NULL` while the queue row cascades.
    pub async fn patients_touched_by_shift(
        &self,
        shift_id: Uuid,
    ) -> Result<Vec<ShiftPatientRow>, sqlx::Error> {
        sqlx::query_as::<_, ShiftPatientRow>(
            r#"
            WITH touched AS (
                SELECT patient_id FROM consult_patient_queue  WHERE shift_id = $1
                UNION
                SELECT patient_id FROM consultation_notes      WHERE shift_id = $1
                UNION
                SELECT patient_id FROM patient_handover_notes  WHERE shift_id = $1
            )
            SELECT p.id AS patient_id, p.full_name, p.age, p.gender,
                   p.severity_level,
                   (SELECT q.state FROM consult_patient_queue q
                     WHERE q.shift_id = $1 AND q.patient_id = p.id
                     LIMIT 1) AS queue_state
              FROM touched t
              JOIN patients p ON p.id = t.patient_id
             ORDER BY p.full_name ASC
            "#,
        )
        .bind(shift_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Display names for note authors, resolved in one round trip rather than
    /// per note.
    pub async fn author_names(
        &self,
        user_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
        sqlx::query_as::<_, (Uuid, String)>(
            r#"
            SELECT id, TRIM(COALESCE(first_name,'') || ' ' || COALESCE(last_name,''))
              FROM users
             WHERE id = ANY($1)
            "#,
        )
        .bind(user_ids)
        .fetch_all(&self.pool)
        .await
    }
}

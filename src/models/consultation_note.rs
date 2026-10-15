use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// Voice-recorded consultation note — maps to `consultation_notes`. The
/// structured fields (chief_complaint..plan) are filled in by the clinician
/// by hand, referencing `full_transcript` — there is no LLM summarization
/// step in this phase.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, ToSchema)]
pub struct ConsultationNote {
    pub id: Uuid,
    pub patient_id: Uuid,
    pub hospital_id: Uuid,
    pub recorded_by: Uuid,
    pub status: String,
    pub full_transcript: String,
    pub language: String,
    pub duration_seconds: i32,
    pub chief_complaint: Option<String>,
    pub history_of_present_illness: Option<String>,
    pub assessment: Option<String>,
    pub plan: Option<String>,

    /// The shift this note was written during. `None` for an ad-hoc note, and
    /// for every note written before `20240063` — those are simply absent from
    /// the per-shift roll-up.
    pub shift_id: Option<Uuid>,
    /// The consultation the note was written in. `ON DELETE SET NULL`: the
    /// clinical record outlives the video session.
    pub session_id: Option<Uuid>,
    /// The doctor's diagnosis, alongside the existing SOAP fields.
    pub diagnosis: Option<String>,
    /// Free-form, e.g. `{"bp":"120/80","temp_c":37.1}` — deliberately unshaped
    /// so a note is never blocked on a vitals schema the ML model owns.
    pub vitals: serde_json::Value,
    /// JSON array of what was prescribed. Defaults to `[]`.
    pub medications: serde_json::Value,
    /// When the patient should be seen again; `null` if no follow-up is due.
    pub follow_up_at: Option<DateTime<Utc>>,
    /// Set when this note amends an earlier, locked one. The original is never
    /// rewritten.
    pub amends_note_id: Option<Uuid>,
    /// Past this, the note is locked and an amendment needs a new row. `None`
    /// means *not governed by a window*, which is treated as **closed** — see
    /// `ConsultationNoteRepository::update_fields`.
    pub editable_until: Option<DateTime<Utc>>,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// One transcribed audio chunk within a consultation note.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, ToSchema)]
pub struct ConsultationTranscriptSegment {
    pub id: Uuid,
    pub consultation_note_id: Uuid,
    pub sequence: i32,
    pub audio_path: String,
    pub audio_duration_seconds: Option<f32>,
    pub transcript_text: String,
    pub status: String,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConsultationNoteDetail {
    #[serde(flatten)]
    pub note: ConsultationNote,
    pub segments: Vec<ConsultationTranscriptSegment>,
}

/// Body for `PATCH /api/v1/consultation-notes/{id}` — every field optional
/// so the clinician can save partial progress as they type.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateConsultationNoteRequest {
    pub chief_complaint: Option<String>,
    pub history_of_present_illness: Option<String>,
    pub assessment: Option<String>,
    pub plan: Option<String>,
    pub diagnosis: Option<String>,
    /// Replaces the stored object wholesale; it is not merged field by field.
    pub vitals: Option<serde_json::Value>,
    /// Replaces the stored array wholesale.
    pub medications: Option<Vec<serde_json::Value>>,
    pub follow_up_at: Option<DateTime<Utc>>,
}

/// Body for `POST /api/v1/patients/{patient_id}/consultation-notes`.
///
/// `shift_id` is optional and, when present, is authorized server-side: the
/// caller must be that shift's assigned clinician. The session is derived from
/// the shift rather than accepted, so a client cannot attach a note to someone
/// else's consultation.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct StartConsultationNoteRequest {
    #[serde(default)]
    pub shift_id: Option<Uuid>,
    /// Amend a note whose edit window has closed. Must belong to the same
    /// patient, and to the same shift when one is given.
    #[serde(default)]
    pub amends_note_id: Option<Uuid>,
}

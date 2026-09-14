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
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateConsultationNoteRequest {
    pub chief_complaint: Option<String>,
    pub history_of_present_illness: Option<String>,
    pub assessment: Option<String>,
    pub plan: Option<String>,
}

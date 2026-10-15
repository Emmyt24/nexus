//! Orchestrates voice-recorded consultation notes: a clinician starts a note
//! against a patient, uploads audio chunks as they record, each chunk is
//! saved to disk and sent to ml-service's Whisper endpoint for transcription,
//! and the result is appended to the note's running transcript. The
//! structured SOAP fields are filled in by the clinician by hand — there is
//! no LLM summarization step in this phase.

use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use crate::models::consultation_note::{ConsultationNote, ConsultationNoteDetail};
use crate::models::user::Claims;
use crate::repositories::consultation_note::{
    ConsultationNoteRepository, RepositoryError as ConsultationNoteRepoError,
};
use crate::repositories::patient::PatientRepository;
use crate::services::ml_client::{MlClient, MlClientError};
use crate::services::video_service::{ShiftDataRole, VideoService};

#[derive(Debug, thiserror::Error)]
pub enum ConsultationNoteError {
    #[error("Repository error: {0}")]
    Repository(#[from] ConsultationNoteRepoError),

    #[error("Failed to store audio chunk: {0}")]
    Storage(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Consultation note not found: {0}")]
    NotFound(Uuid),

    #[error("Patient not found: {0}")]
    PatientNotFound(Uuid),

    /// The caller's hospital does not own this note. This is the tenant
    /// boundary for clinical content — there is no RLS.
    #[error("This consultation note belongs to a different hospital")]
    WrongHospital,

    #[error("No hospital associated with this account")]
    NoHospital,

    #[error("Only the clinician assigned to this shift can record against it")]
    NotTheAssignedClinician,

    #[error("The edit window for this note has closed")]
    EditWindowClosed,

    #[error("Not authorized")]
    NotAuthorized,

    #[error("Validation failed: {0}")]
    Validation(String),
}

pub struct ConsultationNoteService {
    repo: Arc<ConsultationNoteRepository>,
    patient_repo: Arc<PatientRepository>,
    /// Holds the one tenant boundary for shift-scoped data. Never re-derived
    /// here.
    video_service: Arc<VideoService>,
    ml_client: Arc<MlClient>,
    upload_dir: PathBuf,
}

impl ConsultationNoteService {
    pub fn new(
        repo: Arc<ConsultationNoteRepository>,
        patient_repo: Arc<PatientRepository>,
        video_service: Arc<VideoService>,
        ml_client: Arc<MlClient>,
    ) -> Self {
        let upload_dir = std::env::var("CONSULTATION_UPLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("uploads/consultations"));
        Self {
            repo,
            patient_repo,
            video_service,
            ml_client,
            upload_dir,
        }
    }

    /// The tenant boundary for a note reached by its own id.
    ///
    /// Every read and write path goes through this. A role guard at the router
    /// only proves the caller is *a* clinician or *a* hospital admin; it can
    /// never say *which hospital*, so without this a note id is a read
    /// capability for the whole platform.
    async fn note_for_caller(
        &self,
        id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultationNote, ConsultationNoteError> {
        let note = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or(ConsultationNoteError::NotFound(id))?;

        match claims.role {
            // Support reaches metadata through other surfaces; a note body is
            // clinical content and platform staff have no lawful basis for it.
            crate::models::user::UserRole::SuperAdmin
            | crate::models::user::UserRole::OperationsAdmin => {
                return Err(ConsultationNoteError::NotAuthorized)
            }
            _ => {}
        }

        let caller_hospital = claims
            .hospital_id
            .as_deref()
            .and_then(|h| Uuid::parse_str(h).ok());

        match caller_hospital {
            Some(h) if h == note.hospital_id => Ok(note),
            Some(_) => Err(ConsultationNoteError::WrongHospital),
            // A health worker's JWT carries no hospital, so fall back to the
            // shift that produced the note: being its assigned clinician is the
            // claim. A note with no shift cannot be reached this way.
            None => {
                let shift_id = note
                    .shift_id
                    .ok_or(ConsultationNoteError::NoHospital)?;
                let shift = self
                    .video_service
                    .load_shift_for_authz(shift_id)
                    .await
                    .map_err(|_| ConsultationNoteError::NotAuthorized)?;
                match self
                    .video_service
                    .authorize_shift_data_access(&shift, claims)
                    .await
                {
                    Ok(ShiftDataRole::Clinician(_)) => Ok(note),
                    _ => Err(ConsultationNoteError::NotAuthorized),
                }
            }
        }
    }

    /// Start a note. When `shift_id` is given the caller must be that shift's
    /// assigned clinician, and both the hospital and the session are derived
    /// from the shift rather than accepted from the client.
    ///
    /// `claims_hospital_id` is the caller's own hospital, which a **health
    /// worker's token does not carry**: clinicians are marketplace-wide, so a
    /// worker's hospital association *is* the shift. Without a shift there is
    /// nothing to derive it from, and the caller must supply one.
    pub async fn start(
        &self,
        patient_id: Uuid,
        claims_hospital_id: Option<Uuid>,
        recorded_by: Uuid,
        shift_id: Option<Uuid>,
        amends_note_id: Option<Uuid>,
        claims: &Claims,
    ) -> Result<ConsultationNote, ConsultationNoteError> {
        let patient = self
            .patient_repo
            .find_by_id(patient_id)
            .await
            .map_err(|e| ConsultationNoteError::Validation(e.to_string()))?
            .ok_or(ConsultationNoteError::PatientNotFound(patient_id))?;

        let mut session_id = None;
        let hospital_id = if let Some(shift_id) = shift_id {
            let shift = self
                .video_service
                .load_shift_for_authz(shift_id)
                .await
                .map_err(|_| ConsultationNoteError::NotAuthorized)?;
            // Only the assigned clinician records clinical findings against a
            // shift. A hospital admin files the handover note instead.
            match self
                .video_service
                .authorize_shift_data_access(&shift, claims)
                .await
            {
                Ok(ShiftDataRole::Clinician(_)) => {}
                _ => return Err(ConsultationNoteError::NotTheAssignedClinician),
            }
            if shift.hospital_id != patient.hospital_id {
                return Err(ConsultationNoteError::WrongHospital);
            }
            session_id = self.video_service.session_id_for_shift(shift_id).await?;
            // The shift is the authority, not the token.
            shift.hospital_id
        } else {
            let hospital_id = claims_hospital_id.ok_or(ConsultationNoteError::NoHospital)?;
            if patient.hospital_id != hospital_id {
                return Err(ConsultationNoteError::WrongHospital);
            }
            hospital_id
        };

        // An amendment must belong to the same patient, and to the same shift
        // when one is given, or the record detaches from its own history.
        if let Some(amends) = amends_note_id {
            let original = self
                .repo
                .find_by_id(amends)
                .await?
                .ok_or(ConsultationNoteError::NotFound(amends))?;
            if original.patient_id != patient_id
                || (shift_id.is_some() && original.shift_id != shift_id)
            {
                return Err(ConsultationNoteError::NotFound(amends));
            }
        }

        Ok(self
            .repo
            .create(
                patient_id,
                hospital_id,
                recorded_by,
                shift_id,
                session_id,
                amends_note_id,
            )
            .await?)
    }

    pub async fn get_detail(
        &self,
        id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultationNoteDetail, ConsultationNoteError> {
        let note = self.note_for_caller(id, claims).await?;
        let segments = self.repo.segments_for_note(id).await?;
        Ok(ConsultationNoteDetail { note, segments })
    }

    pub async fn list_for_patient(
        &self,
        patient_id: Uuid,
        claims: &Claims,
    ) -> Result<Vec<ConsultationNote>, ConsultationNoteError> {
        // Scoped by the patient's hospital, not the note's: an empty list is
        // the right answer for a foreign patient, but reading one patient's
        // notes must still prove the caller owns that patient.
        let patient = self
            .patient_repo
            .find_by_id(patient_id)
            .await
            .map_err(|e| ConsultationNoteError::Validation(e.to_string()))?
            .ok_or(ConsultationNoteError::PatientNotFound(patient_id))?;

        let caller_hospital = claims
            .hospital_id
            .as_deref()
            .and_then(|h| Uuid::parse_str(h).ok())
            .ok_or(ConsultationNoteError::NoHospital)?;
        if caller_hospital != patient.hospital_id {
            return Err(ConsultationNoteError::WrongHospital);
        }

        Ok(self.repo.list_by_patient(patient_id).await?)
    }

    /// Every note a shift produced, across all its patients. Backs the
    /// hospital's per-shift roll-up.
    pub async fn list_for_shift(
        &self,
        shift_id: Uuid,
    ) -> Result<Vec<ConsultationNote>, ConsultationNoteError> {
        Ok(self.repo.list_by_shift(shift_id).await?)
    }

    /// Every note a shift produced for one patient.
    pub async fn list_for_shift_patient(
        &self,
        shift_id: Uuid,
        patient_id: Uuid,
    ) -> Result<Vec<ConsultationNote>, ConsultationNoteError> {
        Ok(self
            .repo
            .list_by_shift_patient(shift_id, patient_id)
            .await?)
    }

    /// Saves the chunk to disk, sends it to ml-service for transcription, and
    /// persists the resulting segment (or the failure — a bad chunk doesn't
    /// stop the consultation; the clinician keeps recording and still has
    /// every other chunk's text). Returns the persisted segment either way,
    /// via `Ok`, so the caller doesn't need to special-case transcription
    /// failures as an HTTP error — the note carries the failure per-segment.
    pub async fn add_audio_chunk(
        &self,
        note_id: Uuid,
        sequence: i32,
        audio_bytes: Vec<u8>,
        original_filename: &str,
    ) -> Result<crate::models::consultation_note::ConsultationTranscriptSegment, ConsultationNoteError>
    {
        let ext = std::path::Path::new(original_filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("webm");
        let note_dir = self.upload_dir.join(note_id.to_string());
        tokio::fs::create_dir_all(&note_dir).await?;
        let chunk_path = note_dir.join(format!("chunk_{sequence}.{ext}"));
        tokio::fs::write(&chunk_path, &audio_bytes).await?;

        let stored_path = chunk_path.to_string_lossy().to_string();

        match self.ml_client.transcribe(audio_bytes, original_filename).await {
            Ok(result) => Ok(self
                .repo
                .append_segment(
                    note_id,
                    sequence,
                    &stored_path,
                    Some(result.duration_seconds),
                    &result.text,
                    "completed",
                    None,
                )
                .await?),
            Err(e) => {
                tracing::warn!("Transcription failed for note {note_id} chunk {sequence}: {e}");
                Ok(self
                    .repo
                    .append_segment(
                        note_id,
                        sequence,
                        &stored_path,
                        None,
                        "",
                        "failed",
                        Some(&transcribe_error_message(&e)),
                    )
                    .await?)
            }
        }
    }

    /// Patch a note in place. Allowed only inside its edit window; past that the
    /// caller must start a new note carrying `amends_note_id`, so a locked
    /// clinical record is never rewritten.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_fields(
        &self,
        id: Uuid,
        claims: &Claims,
        request: crate::models::consultation_note::UpdateConsultationNoteRequest,
    ) -> Result<ConsultationNote, ConsultationNoteError> {
        let note = self.note_for_caller(id, claims).await?;
        // A clinician edits their own findings, not a colleague's.
        let caller = Uuid::parse_str(&claims.sub)
            .map_err(|_| ConsultationNoteError::NotAuthorized)?;
        if note.recorded_by != caller {
            return Err(ConsultationNoteError::NotAuthorized);
        }

        let medications = request.medications.map(serde_json::Value::Array);

        self.repo
            .update_fields(
                id,
                request.chief_complaint.as_deref(),
                request.history_of_present_illness.as_deref(),
                request.assessment.as_deref(),
                request.plan.as_deref(),
                request.diagnosis.as_deref(),
                request.vitals.as_ref(),
                medications.as_ref(),
                request.follow_up_at,
            )
            .await?
            .ok_or(ConsultationNoteError::EditWindowClosed)
    }

    pub async fn complete(
        &self,
        id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultationNote, ConsultationNoteError> {
        self.note_for_caller(id, claims).await?;
        Ok(self.repo.mark_completed(id).await?)
    }

    /// Audio chunks are reached by note id, so they need the same check.
    pub async fn authorize_note(
        &self,
        id: Uuid,
        claims: &Claims,
    ) -> Result<(), ConsultationNoteError> {
        self.note_for_caller(id, claims).await.map(|_| ())
    }
}

fn transcribe_error_message(e: &MlClientError) -> String {
    e.to_string()
}

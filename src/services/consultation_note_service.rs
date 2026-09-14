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
use crate::repositories::consultation_note::{
    ConsultationNoteRepository, RepositoryError as ConsultationNoteRepoError,
};
use crate::services::ml_client::{MlClient, MlClientError};

#[derive(Debug, thiserror::Error)]
pub enum ConsultationNoteError {
    #[error("Repository error: {0}")]
    Repository(#[from] ConsultationNoteRepoError),

    #[error("Failed to store audio chunk: {0}")]
    Storage(#[from] std::io::Error),
}

pub struct ConsultationNoteService {
    repo: Arc<ConsultationNoteRepository>,
    ml_client: Arc<MlClient>,
    upload_dir: PathBuf,
}

impl ConsultationNoteService {
    pub fn new(repo: Arc<ConsultationNoteRepository>, ml_client: Arc<MlClient>) -> Self {
        let upload_dir = std::env::var("CONSULTATION_UPLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("uploads/consultations"));
        Self {
            repo,
            ml_client,
            upload_dir,
        }
    }

    pub async fn start(
        &self,
        patient_id: Uuid,
        hospital_id: Uuid,
        recorded_by: Uuid,
    ) -> Result<ConsultationNote, ConsultationNoteError> {
        Ok(self.repo.create(patient_id, hospital_id, recorded_by).await?)
    }

    pub async fn get_detail(
        &self,
        id: Uuid,
    ) -> Result<Option<ConsultationNoteDetail>, ConsultationNoteError> {
        let Some(note) = self.repo.find_by_id(id).await? else {
            return Ok(None);
        };
        let segments = self.repo.segments_for_note(id).await?;
        Ok(Some(ConsultationNoteDetail { note, segments }))
    }

    pub async fn list_for_patient(
        &self,
        patient_id: Uuid,
    ) -> Result<Vec<ConsultationNote>, ConsultationNoteError> {
        Ok(self.repo.list_by_patient(patient_id).await?)
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

    pub async fn update_fields(
        &self,
        id: Uuid,
        chief_complaint: Option<&str>,
        history_of_present_illness: Option<&str>,
        assessment: Option<&str>,
        plan: Option<&str>,
    ) -> Result<ConsultationNote, ConsultationNoteError> {
        Ok(self
            .repo
            .update_fields(id, chief_complaint, history_of_present_illness, assessment, plan)
            .await?)
    }

    pub async fn complete(&self, id: Uuid) -> Result<ConsultationNote, ConsultationNoteError> {
        Ok(self.repo.mark_completed(id).await?)
    }
}

fn transcribe_error_message(e: &MlClientError) -> String {
    e.to_string()
}

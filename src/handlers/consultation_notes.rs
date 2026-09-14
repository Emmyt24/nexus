use axum::{
    extract::{Multipart, Path, State},
    http::HeaderMap,
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::models::consultation_note::{
    ConsultationNote, ConsultationNoteDetail, UpdateConsultationNoteRequest,
};
use crate::routes::AppState;
use crate::services::consultation_note_service::ConsultationNoteError;
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

fn map_service_error(e: ConsultationNoteError) -> AppError {
    match e {
        ConsultationNoteError::Repository(e) => AppError::InternalServerError(e.to_string()),
        ConsultationNoteError::Storage(e) => AppError::InternalServerError(e.to_string()),
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StartConsultationNoteResponse {
    pub id: Uuid,
}

/// POST /api/v1/patients/{patient_id}/consultation-notes
#[utoipa::path(
    post,
    path = "/api/v1/patients/{patient_id}/consultation-notes",
    params(("patient_id" = Uuid, Path, description = "Patient ID")),
    responses(
        (status = 201, description = "Consultation note started", body = StartConsultationNoteResponse)
    ),
    tag = "consultation-notes",
    summary = "Start a new voice-recorded consultation note for a patient"
)]
pub async fn start_note(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(patient_id): Path<Uuid>,
) -> AppResult<Json<StartConsultationNoteResponse>> {
    let claims = extract_claims(&headers)?;
    let recorded_by = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;
    let hospital_id = claims
        .hospital_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| {
            AppError::Forbidden("No hospital associated with this account".to_string())
        })?;

    let note = state
        .consultation_note_service
        .start(patient_id, hospital_id, recorded_by)
        .await
        .map_err(map_service_error)?;

    Ok(Json(StartConsultationNoteResponse { id: note.id }))
}

/// GET /api/v1/patients/{patient_id}/consultation-notes
#[utoipa::path(
    get,
    path = "/api/v1/patients/{patient_id}/consultation-notes",
    params(("patient_id" = Uuid, Path, description = "Patient ID")),
    responses(
        (status = 200, description = "Consultation notes for this patient, newest first", body = [ConsultationNote])
    ),
    tag = "consultation-notes",
    summary = "List a patient's consultation notes"
)]
pub async fn list_for_patient(
    State(state): State<AppState>,
    Path(patient_id): Path<Uuid>,
) -> AppResult<Json<Vec<ConsultationNote>>> {
    let notes = state
        .consultation_note_service
        .list_for_patient(patient_id)
        .await
        .map_err(map_service_error)?;
    Ok(Json(notes))
}

/// GET /api/v1/consultation-notes/{id}
#[utoipa::path(
    get,
    path = "/api/v1/consultation-notes/{id}",
    params(("id" = Uuid, Path, description = "Consultation note ID")),
    responses(
        (status = 200, description = "Note with its transcript segments", body = ConsultationNoteDetail),
        (status = 404, description = "Not found")
    ),
    tag = "consultation-notes",
    summary = "Fetch a consultation note and its transcript segments"
)]
pub async fn get_note(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ConsultationNoteDetail>> {
    let detail = state
        .consultation_note_service
        .get_detail(id)
        .await
        .map_err(map_service_error)?
        .ok_or_else(|| AppError::NotFound(format!("Consultation note {id} not found")))?;
    Ok(Json(detail))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AudioChunkResponse {
    pub sequence: i32,
    pub transcript_text: String,
    pub status: String,
    pub full_transcript: String,
}

/// POST /api/v1/consultation-notes/{id}/audio-chunk
///
/// Multipart form with an `audio` file field and a `sequence` text field
/// (0-indexed chunk order — the browser sends these in order via
/// MediaRecorder's timeslice, but the server doesn't assume that).
#[utoipa::path(
    post,
    path = "/api/v1/consultation-notes/{id}/audio-chunk",
    params(("id" = Uuid, Path, description = "Consultation note ID")),
    responses(
        (status = 200, description = "Chunk transcribed and appended", body = AudioChunkResponse)
    ),
    tag = "consultation-notes",
    summary = "Upload and transcribe one audio chunk of a consultation recording"
)]
pub async fn upload_audio_chunk(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    mut multipart: Multipart,
) -> AppResult<Json<AudioChunkResponse>> {
    let mut sequence: Option<i32> = None;
    let mut audio_bytes: Option<Vec<u8>> = None;
    let mut filename = "chunk.webm".to_string();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Invalid multipart body: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "sequence" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                sequence = text.parse().ok();
            }
            "audio" => {
                filename = field.file_name().unwrap_or("chunk.webm").to_string();
                audio_bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| AppError::BadRequest(e.to_string()))?
                        .to_vec(),
                );
            }
            _ => {}
        }
    }

    let sequence = sequence
        .ok_or_else(|| AppError::BadRequest("Missing 'sequence' field".to_string()))?;
    let audio_bytes =
        audio_bytes.ok_or_else(|| AppError::BadRequest("Missing 'audio' field".to_string()))?;
    if audio_bytes.is_empty() {
        return Err(AppError::BadRequest("Empty audio upload".to_string()));
    }

    let segment = state
        .consultation_note_service
        .add_audio_chunk(id, sequence, audio_bytes, &filename)
        .await
        .map_err(map_service_error)?;

    let detail = state
        .consultation_note_service
        .get_detail(id)
        .await
        .map_err(map_service_error)?
        .ok_or_else(|| AppError::NotFound(format!("Consultation note {id} not found")))?;

    Ok(Json(AudioChunkResponse {
        sequence: segment.sequence,
        transcript_text: segment.transcript_text,
        status: segment.status,
        full_transcript: detail.note.full_transcript,
    }))
}

/// PATCH /api/v1/consultation-notes/{id}
#[utoipa::path(
    patch,
    path = "/api/v1/consultation-notes/{id}",
    params(("id" = Uuid, Path, description = "Consultation note ID")),
    request_body = UpdateConsultationNoteRequest,
    responses(
        (status = 200, description = "Updated note", body = ConsultationNote)
    ),
    tag = "consultation-notes",
    summary = "Save the clinician's structured note fields"
)]
pub async fn update_note(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateConsultationNoteRequest>,
) -> AppResult<Json<ConsultationNote>> {
    let note = state
        .consultation_note_service
        .update_fields(
            id,
            payload.chief_complaint.as_deref(),
            payload.history_of_present_illness.as_deref(),
            payload.assessment.as_deref(),
            payload.plan.as_deref(),
        )
        .await
        .map_err(map_service_error)?;
    Ok(Json(note))
}

/// POST /api/v1/consultation-notes/{id}/complete
#[utoipa::path(
    post,
    path = "/api/v1/consultation-notes/{id}/complete",
    params(("id" = Uuid, Path, description = "Consultation note ID")),
    responses(
        (status = 200, description = "Note marked completed", body = ConsultationNote)
    ),
    tag = "consultation-notes",
    summary = "Mark a consultation note as completed"
)]
pub async fn complete_note(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ConsultationNote>> {
    let note = state
        .consultation_note_service
        .complete(id)
        .await
        .map_err(map_service_error)?;
    Ok(Json(note))
}

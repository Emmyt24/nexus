use axum::{
    extract::{Multipart, Path, State},
    http::HeaderMap,
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::models::consultation_note::{
    ConsultationNote, ConsultationNoteDetail, StartConsultationNoteRequest,
    UpdateConsultationNoteRequest,
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
        ConsultationNoteError::Database(e) => AppError::Database(e),
        ConsultationNoteError::NotFound(id) => {
            AppError::NotFound(format!("Consultation note {id} not found"))
        }
        ConsultationNoteError::PatientNotFound(id) => {
            AppError::NotFound(format!("Patient {id} not found"))
        }
        // Deliberately the same shape as a 403 elsewhere: the caller learns it
        // may not have this note, not whether the id exists.
        ConsultationNoteError::WrongHospital => AppError::Forbidden(
            "This consultation note belongs to a different hospital".to_string(),
        ),
        ConsultationNoteError::NoHospital => {
            AppError::Forbidden("No hospital associated with this account".to_string())
        }
        ConsultationNoteError::NotTheAssignedClinician => AppError::Forbidden(
            "Only the clinician assigned to this shift can record against it".to_string(),
        ),
        ConsultationNoteError::EditWindowClosed => AppError::Conflict(
            "The edit window for this note has closed — post a new note with amends_note_id"
                .to_string(),
        ),
        ConsultationNoteError::NotAuthorized => {
            AppError::Forbidden("Not authorized to access this consultation note".to_string())
        }
        ConsultationNoteError::Validation(m) => AppError::Validation(m),
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
    request_body = Option<StartConsultationNoteRequest>,
    responses(
        (status = 201, description = "Consultation note started", body = StartConsultationNoteResponse),
        (status = 403, description = "Not this patient's hospital, or not the shift's assigned clinician"),
    ),
    tag = "consultation-notes",
    summary = "Start a new voice-recorded consultation note for a patient",
    description = "Send `{}` (or no body) for an ad-hoc note. Send `shift_id` to attach it \
                   to a shift, which requires being that shift's assigned clinician and \
                   makes the note appear in the hospital's per-shift roll-up."
)]
pub async fn start_note(
    State(state): State<AppState>,
    Path(patient_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Option<Json<StartConsultationNoteRequest>>,
) -> AppResult<Json<StartConsultationNoteResponse>> {
    // Optional body: the pre-existing ad-hoc flow posts nothing at all, and
    // must keep working.
    let payload = payload.map(|Json(p)| p).unwrap_or_default();
    let claims = extract_claims(&headers)?;
    let recorded_by = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;
    // Not required: a health worker's token carries no hospital, because
    // clinicians are marketplace-wide. With a `shift_id` the service derives the
    // hospital from the shift; without one it demands this.
    let hospital_id = claims
        .hospital_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok());

    let note = state
        .consultation_note_service
        .start(
            patient_id,
            hospital_id,
            recorded_by,
            payload.shift_id,
            payload.amends_note_id,
            &claims,
        )
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
    headers: HeaderMap,
) -> AppResult<Json<Vec<ConsultationNote>>> {
    let claims = extract_claims(&headers)?;
    let notes = state
        .consultation_note_service
        .list_for_patient(patient_id, &claims)
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
    headers: HeaderMap,
) -> AppResult<Json<ConsultationNoteDetail>> {
    let claims = extract_claims(&headers)?;
    let detail = state
        .consultation_note_service
        .get_detail(id, &claims)
        .await
        .map_err(map_service_error)?;
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
    headers: HeaderMap,
    mut multipart: Multipart,
) -> AppResult<Json<AudioChunkResponse>> {
    // Appending audio to a note is a write to it, so it needs the same tenancy
    // check as reading one. Done before the upload is consumed.
    let claims = extract_claims(&headers)?;
    state
        .consultation_note_service
        .authorize_note(id, &claims)
        .await
        .map_err(map_service_error)?;

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
        .get_detail(id, &claims)
        .await
        .map_err(map_service_error)?;

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
        (status = 200, description = "Updated note", body = ConsultationNote),
        (status = 403, description = "Not your hospital's note, or not its author"),
        (status = 409, description = "The edit window has closed"),
    ),
    tag = "consultation-notes",
    summary = "Save the clinician's structured note fields",
    description = "Patches in place while the note's edit window is open. Once it closes, \
                   start a new note carrying `amends_note_id` — a locked clinical record is \
                   never rewritten."
)]
pub async fn update_note(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<UpdateConsultationNoteRequest>,
) -> AppResult<Json<ConsultationNote>> {
    let claims = extract_claims(&headers)?;
    let note = state
        .consultation_note_service
        .update_fields(id, &claims, payload)
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
    headers: HeaderMap,
) -> AppResult<Json<ConsultationNote>> {
    let claims = extract_claims(&headers)?;
    let note = state
        .consultation_note_service
        .complete(id, &claims)
        .await
        .map_err(map_service_error)?;
    Ok(Json(note))
}

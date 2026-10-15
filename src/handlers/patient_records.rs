// ! HTTP surface for the consult waiting room, per-patient handover notes, and
// ! the hospital's per-shift patient-record roll-up.
// !
// ! Thin by convention: validate, extract the claims, make one service call,
// ! map the error. Fine-grained authorization lives in `PatientRecordService`,
// ! because the route-level `require_role` guard cannot know which hospital owns
// ! a shift.
// !
// ! The doctor's clinical note is written through `consultation_notes.rs`, not
// ! here — this module only *reads* notes, as part of the roll-up.
// !
// ! No local `ErrorResponse`: seven handler modules already declare one and
// ! utoipa keys components by type name, so the shifts one is referenced.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use uuid::Uuid;
use validator::Validate;

use crate::models::consultation_note::ConsultationNote;
use crate::models::patient_record::{
    AddPatientToQueueRequest, ConsultWaitingRoomView, PatientHandoverNoteView,
    ShiftPatientRecordsView, SubmitPatientHandoverNoteRequest,
};
use crate::routes::AppState;
use crate::services::patient_record_service::PatientRecordServiceError;
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

/// POST /api/v1/shifts/{shift_id}/consult/queue
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/consult/queue",
    request_body = AddPatientToQueueRequest,
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 201, description = "Patient queued", body = ConsultWaitingRoomView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not an admin of this shift's hospital", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift or patient not found", body = crate::handlers::shifts::ErrorResponse),
        (status = 409, description = "Already queued, or the consultation has ended", body = crate::handlers::shifts::ErrorResponse),
        (status = 422, description = "Validation error", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "Add a patient to the consultation's waiting room",
    description = "Send `patient_id` to queue someone already on file, or `patient` to \
                   intake a new one and queue them in the same transaction — the ML triage \
                   prediction is queued either way. Returns the whole waiting room so the \
                   caller needs no follow-up read."
)]
pub async fn add_patient_to_queue(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<AddPatientToQueueRequest>,
) -> AppResult<(StatusCode, Json<ConsultWaitingRoomView>)> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .add_patient_to_queue(shift_id, &claims, payload)
        .await
        .map_err(map_patient_record_error)?;

    let view = state
        .patient_record_service
        .waiting_room(shift_id, &claims)
        .await
        .map_err(map_patient_record_error)?;

    Ok((StatusCode::CREATED, Json(view)))
}

/// GET /api/v1/shifts/{shift_id}/consult/queue
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}/consult/queue",
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 200, description = "The waiting room", body = ConsultWaitingRoomView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not a party to this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift not found, or no consultation started", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "Read the consultation's waiting room",
    description = "Counts are always present. `patients` is populated for the assigned \
                   clinician and admins of the owning hospital, and empty for platform \
                   admins, who get no patient identities."
)]
pub async fn get_waiting_room(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<ConsultWaitingRoomView>> {
    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .waiting_room(shift_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// POST /api/v1/shifts/{shift_id}/consult/queue/{entry_id}/call
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/consult/queue/{entry_id}/call",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
        ("entry_id" = Uuid, Path, description = "Queue entry unique identifier"),
    ),
    responses(
        (status = 200, description = "Patient called in", body = ConsultWaitingRoomView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not the clinician assigned to this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Queue entry not found on this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 409, description = "Another patient is already in consultation, or the entry is not waiting", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "Call the next patient in",
    description = "Moves the entry `waiting` -> `in_consult`. At most one patient per \
                   session can be in consultation, enforced in the database — a second \
                   call returns 409 rather than queue-jumping."
)]
pub async fn call_patient(
    State(state): State<AppState>,
    Path((shift_id, entry_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> AppResult<Json<ConsultWaitingRoomView>> {
    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .call_patient(shift_id, entry_id, &claims)
        .await
        .map_err(map_patient_record_error)?;

    state
        .patient_record_service
        .waiting_room(shift_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// POST /api/v1/shifts/{shift_id}/consult/queue/{entry_id}/seen
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/consult/queue/{entry_id}/seen",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
        ("entry_id" = Uuid, Path, description = "Queue entry unique identifier"),
    ),
    responses(
        (status = 200, description = "Patient released", body = ConsultWaitingRoomView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not the clinician assigned to this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Queue entry not found on this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 409, description = "The entry is not in consultation", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "Mark the current patient seen",
    description = "Moves the entry `in_consult` -> `seen` and frees the in-consultation slot."
)]
pub async fn mark_patient_seen(
    State(state): State<AppState>,
    Path((shift_id, entry_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> AppResult<Json<ConsultWaitingRoomView>> {
    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .mark_patient_seen(shift_id, entry_id, &claims)
        .await
        .map_err(map_patient_record_error)?;

    state
        .patient_record_service
        .waiting_room(shift_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// DELETE /api/v1/shifts/{shift_id}/consult/queue/{entry_id}
#[utoipa::path(
    delete,
    path = "/api/v1/shifts/{shift_id}/consult/queue/{entry_id}",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
        ("entry_id" = Uuid, Path, description = "Queue entry unique identifier"),
    ),
    responses(
        (status = 200, description = "Patient withdrawn from the queue", body = ConsultWaitingRoomView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not an admin of this shift's hospital", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Queue entry not found on this shift", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "Withdraw a patient from the waiting room",
    description = "Marks the entry `removed` rather than deleting it, so the queue stays \
                   auditable."
)]
pub async fn remove_from_queue(
    State(state): State<AppState>,
    Path((shift_id, entry_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> AppResult<Json<ConsultWaitingRoomView>> {
    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .remove_from_queue(shift_id, entry_id, &claims)
        .await
        .map_err(map_patient_record_error)?;

    state
        .patient_record_service
        .waiting_room(shift_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// GET /api/v1/shifts/{shift_id}/patients/{patient_id}/consultation-notes
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}/patients/{patient_id}/consultation-notes",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
        ("patient_id" = Uuid, Path, description = "Patient unique identifier"),
    ),
    responses(
        (status = 200, description = "This patient's notes for this shift", body = [ConsultationNote]),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not a party to this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift or patient not found", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "List a patient's consultation notes for one shift",
    description = "Readable by the shift's assigned clinician and by admins of the owning \
                   hospital, during the shift and permanently after it completes. Writes go \
                   through POST /api/v1/patients/{patient_id}/consultation-notes."
)]
pub async fn list_shift_patient_notes(
    State(state): State<AppState>,
    Path((shift_id, patient_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<ConsultationNote>>> {
    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .list_clinical_notes(shift_id, patient_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// PUT /api/v1/shifts/{shift_id}/patients/{patient_id}/handover-note
#[utoipa::path(
    put,
    path = "/api/v1/shifts/{shift_id}/patients/{patient_id}/handover-note",
    request_body = SubmitPatientHandoverNoteRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
        ("patient_id" = Uuid, Path, description = "Patient unique identifier"),
    ),
    responses(
        (status = 200, description = "Handover note filed", body = PatientHandoverNoteView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not a party on site for this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift or patient not found", body = crate::handlers::shifts::ErrorResponse),
        (status = 409, description = "The edit window has closed", body = crate::handlers::shifts::ErrorResponse),
        (status = 422, description = "Validation error", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "File the on-site party's handover note for one patient",
    description = "Written by whoever is on site for this patient: an admin of the shift's \
                   hospital, or the shift's assigned clinician — who is the on-site party on \
                   an in-person shift. Any other health worker is refused. Authorship is \
                   taken from the token, never the body. Idempotent — one note per \
                   (shift, patient); resubmitting inside the one-hour edit window updates it \
                   rather than inserting a second. `escalation_reason` is required when \
                   `escalation_required` is true."
)]
pub async fn submit_handover_note(
    State(state): State<AppState>,
    Path((shift_id, patient_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(payload): Json<SubmitPatientHandoverNoteRequest>,
) -> AppResult<Json<PatientHandoverNoteView>> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .submit_handover_note(shift_id, patient_id, &claims, payload)
        .await
        .map_err(map_patient_record_error)?;

    state
        .patient_record_service
        .get_handover_note(shift_id, patient_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// GET /api/v1/shifts/{shift_id}/patients/{patient_id}/handover-note
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}/patients/{patient_id}/handover-note",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
        ("patient_id" = Uuid, Path, description = "Patient unique identifier"),
    ),
    responses(
        (status = 200, description = "The handover note", body = PatientHandoverNoteView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not a party to this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "No handover note filed for this patient", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "Read one patient's handover note",
    description = "Readable by the shift's assigned clinician and by admins of the owning hospital, during the shift and permanently after it completes. 404 means no note has been filed for this patient yet, which is an ordinary state rather than an error — the on-site party may simply not have written it."
)]
pub async fn get_handover_note(
    State(state): State<AppState>,
    Path((shift_id, patient_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> AppResult<Json<PatientHandoverNoteView>> {
    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .get_handover_note(shift_id, patient_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// GET /api/v1/shifts/{shift_id}/patient-records
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}/patient-records",
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 200, description = "Every patient the shift produced a record for", body = ShiftPatientRecordsView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not a party to this shift", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift not found", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "patient-records",
    summary = "The hospital's read of everything a shift produced",
    description = "Per patient: the clinician's consultation notes and the on-site party's \
                   handover note. Available during the shift and permanently after it \
                   completes — `shift_status` tells a live shift from a final record. \
                   Platform admins receive counts only, with `metadata_only: true` and an \
                   empty `patients`."
)]
pub async fn shift_patient_records(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<ShiftPatientRecordsView>> {
    let claims = extract_claims(&headers)?;

    state
        .patient_record_service
        .shift_patient_records(shift_id, &claims)
        .await
        .map(Json)
        .map_err(map_patient_record_error)
}

/// Exhaustive on purpose: a future variant must not silently fall through to a
/// 500.
fn map_patient_record_error(e: PatientRecordServiceError) -> AppError {
    use PatientRecordServiceError as E;
    match e {
        E::Database(e) => AppError::Database(e),
        E::Validation(m) => AppError::Validation(m),
        E::ShiftNotFound(id) => AppError::NotFound(format!("Shift {id} not found")),
        E::PatientNotFound(id) => AppError::NotFound(format!("Patient {id} not found")),
        E::SessionNotFound => {
            AppError::NotFound("No consultation has been started for this shift".to_string())
        }
        E::SessionEnded => AppError::Conflict("This consultation has already ended".to_string()),
        E::QueueEntryNotFound(id) => {
            AppError::NotFound(format!("Queue entry {id} not found on this shift"))
        }
        E::PatientAlreadyQueued => AppError::Conflict(
            "This patient is already in the queue for this consultation".to_string(),
        ),
        E::AnotherPatientInConsult => {
            AppError::Conflict("Another patient is already in consultation".to_string())
        }
        E::IllegalQueueTransition { from, to } => {
            AppError::Conflict(format!("Cannot move this patient from {from} to {to}"))
        }
        E::HandoverNoteNotFound => {
            AppError::NotFound("No handover note has been filed for this patient".to_string())
        }
        E::HandoverEditWindowClosed => {
            AppError::Conflict("The edit window for this handover note has closed".to_string())
        }
        E::NotTheAssignedClinician => AppError::Forbidden(
            "Only the clinician assigned to this shift can do this".to_string(),
        ),
        E::NotTheOwningHospital => {
            AppError::Forbidden("Only an admin of this shift's hospital can do this".to_string())
        }
        E::NotAuthorized => {
            AppError::Forbidden("Not authorized to access this shift's patient records".to_string())
        }
        E::NoClinicianProfile => {
            AppError::Forbidden("Authenticated user has no clinician profile".to_string())
        }
        E::PatientHospitalMismatch => {
            AppError::Forbidden("Patient belongs to a different hospital".to_string())
        }
    }
}

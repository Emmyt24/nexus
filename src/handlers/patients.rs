use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

use crate::models::patient::{NewPatientRequest, PatientResponse};
use crate::models::patient_prediction::PredictionResponse;
use crate::routes::AppState;
use crate::services::patient_prediction_service::PatientPredictionError;
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

/// Error response for API documentation.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorDetail {
    pub message: String,
    pub status: u16,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct IngestPatientResponse {
    pub patient_id: Uuid,
    pub prediction_id: Uuid,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PatientDetailResponse {
    #[serde(flatten)]
    pub patient: PatientResponse,
    pub prediction: Option<PredictionResponse>,
}

fn map_prediction_error(e: PatientPredictionError) -> AppError {
    match e {
        PatientPredictionError::Database(e) => AppError::Database(e),
        PatientPredictionError::PatientRepo(e) => {
            AppError::InternalServerError(e.to_string())
        }
    }
}

/// POST /api/v1/ingest/patient
#[utoipa::path(
    post,
    path = "/api/v1/ingest/patient",
    request_body = NewPatientRequest,
    responses(
        (status = 202, description = "Patient queued for ML prediction", body = IngestPatientResponse),
        (status = 422, description = "Validation error", body = ErrorResponse),
        (status = 403, description = "No hospital associated with this account", body = ErrorResponse)
    ),
    tag = "patients",
    summary = "Submit a patient for ML triage",
    description = "Stores the patient intake and queues a prediction. No ML call happens inline — PatientPredictionWorker picks the row up on its next poll tick and the result is delivered via GET /api/v1/pipeline/events (SSE) or by polling GET /api/v1/patients/{id}."
)]
pub async fn ingest_patient(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<NewPatientRequest>,
) -> AppResult<(StatusCode, Json<IngestPatientResponse>)> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let claims = extract_claims(&headers)?;
    let created_by = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;
    let hospital_id = claims
        .hospital_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| {
            AppError::Forbidden("No hospital associated with this account".to_string())
        })?;

    let (patient, prediction) = state
        .patient_prediction_service
        .ingest_patient(hospital_id, created_by, payload)
        .await
        .map_err(map_prediction_error)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(IngestPatientResponse {
            patient_id: patient.id,
            prediction_id: prediction.id,
            status: prediction.status,
        }),
    ))
}

/// GET /api/v1/patients/{id}
#[utoipa::path(
    get,
    path = "/api/v1/patients/{id}",
    params(("id" = Uuid, Path, description = "Patient ID")),
    responses(
        (status = 200, description = "Patient and current prediction state", body = PatientDetailResponse),
        (status = 404, description = "Patient not found", body = ErrorResponse),
        (status = 403, description = "Patient belongs to a different hospital", body = ErrorResponse)
    ),
    tag = "patients",
    summary = "Fetch a patient and their latest prediction",
    description = "Poll fallback for clients not using the SSE stream at GET /api/v1/pipeline/events."
)]
pub async fn get_patient(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PatientDetailResponse>> {
    let claims = extract_claims(&headers)?;
    let hospital_id = claims
        .hospital_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| {
            AppError::Forbidden("No hospital associated with this account".to_string())
        })?;

    let patient = state
        .patient_repo
        .find_by_id(id)
        .await
        .map_err(|e| AppError::InternalServerError(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Patient {id} not found")))?;

    if patient.hospital_id != hospital_id {
        return Err(AppError::Forbidden(
            "Patient belongs to a different hospital".to_string(),
        ));
    }

    let prediction = state
        .patient_prediction_service
        .latest_for_patient(id)
        .await
        .map_err(map_prediction_error)?;

    Ok(Json(PatientDetailResponse {
        patient: PatientResponse::from(patient),
        prediction,
    }))
}

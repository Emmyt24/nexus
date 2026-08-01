use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::models::admin_registration::HospitalRegistrationRequest;
use crate::routes::AppState;
use crate::utils::extract_claims;
use crate::services::registration_service::{
    HospitalListResponse, RegistrationError, RegistrationStatusResponse,
};

/// Response for hospital registration
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HospitalRegistrationResponse {
    pub hospital_id: Uuid,
    pub status: String,
    pub message: String,
    pub next_steps: Vec<String>,
}

/// Response for approval/rejection
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StatusChangeResponse {
    pub message: String,
    pub hospital_id: Uuid,
    pub new_status: String,
}

/// Request for approval
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ApprovalRequest {
    #[schema(example = "All documents verified. Hospital meets requirements.")]
    pub notes: Option<String>,
}

/// Request for rejection
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RejectionRequest {
    #[schema(example = "Incomplete documentation. Missing operational license.")]
    pub reason: String,
}

/// Error response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

/// Register a new hospital
#[utoipa::path(
    post,
    path = "/api/v1/hospitals/register",
    tag = "hospitals",
    request_body = HospitalRegistrationRequest,
    responses(
        (status = 201, description = "Hospital registered successfully", body = HospitalRegistrationResponse),
        (status = 400, description = "Validation error", body = ErrorResponse),
        (status = 409, description = "Duplicate email registration", body = ErrorResponse),
        (status = 503, description = "External service error", body = ErrorResponse)
    )
)]
pub async fn register_hospital(
    State(state): State<AppState>,
    Json(request): Json<HospitalRegistrationRequest>,
) -> Result<(StatusCode, Json<HospitalRegistrationResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user_id = Uuid::new_v4();
    match state
        .registration_service
        .register_hospital(user_id, request)
        .await
    {
        Ok(result) => Ok((
            StatusCode::CREATED,
            Json(HospitalRegistrationResponse {
                hospital_id: result.hospital_id,
                status: format!("{:?}", result.status),
                message: result.message,
                next_steps: result.next_steps,
            }),
        )),
        Err(e) => {
            let (status, code) = match e {
                RegistrationError::ValidationError(_) => {
                    (StatusCode::BAD_REQUEST, "VALIDATION_ERROR")
                }
                RegistrationError::DuplicateRegistration(_) => {
                    (StatusCode::CONFLICT, "DUPLICATE_REGISTRATION")
                }
                RegistrationError::NotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
                RegistrationError::InvalidStatusTransition(_, _) => {
                    (StatusCode::CONFLICT, "INVALID_STATUS_TRANSITION")
                }
                RegistrationError::LocationError(_) => {
                    (StatusCode::SERVICE_UNAVAILABLE, "EXTERNAL_SERVICE_ERROR")
                }
                RegistrationError::IdentityNotVerified => {
                    (StatusCode::FORBIDDEN, "IDENTITY_NOT_VERIFIED")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
            };

            Err((
                status,
                Json(ErrorResponse {
                    code: code.to_string(),
                    message: e.to_string(),
                }),
            ))
        }
    }
}

/// Get registration status for a hospital
#[utoipa::path(
    get,
    path = "/api/v1/hospitals/{hospital_id}/status",
    tag = "hospitals",
    params(
        ("hospital_id" = Uuid, Path, description = "Hospital ID")
    ),
    responses(
        (status = 200, description = "Registration status retrieved", body = RegistrationStatusResponse),
        (status = 404, description = "Hospital not found", body = ErrorResponse)
    )
)]
pub async fn get_registration_status(
    State(state): State<AppState>,
    Path(hospital_id): Path<Uuid>,
) -> Result<Json<RegistrationStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    match state
        .registration_service
        .get_registration_status(hospital_id)
        .await
    {
        Ok(status) => Ok(Json(status)),
        Err(e) => {
            let status_code = match e {
                RegistrationError::NotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };

            Err((
                status_code,
                Json(ErrorResponse {
                    code: "ERROR".to_string(),
                    message: e.to_string(),
                }),
            ))
        }
    }
}

/// Approve hospital registration
#[utoipa::path(
    post,
    path = "/api/v1/admin/hospitals/{hospital_id}/approve",
    tag = "admin",
    params(
        ("hospital_id" = Uuid, Path, description = "Hospital ID")
    ),
    request_body = ApprovalRequest,
    responses(
        (status = 200, description = "Hospital approved successfully", body = StatusChangeResponse),
        (status = 404, description = "Hospital not found", body = ErrorResponse),
        (status = 409, description = "Invalid status transition", body = ErrorResponse)
    ),
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn approve_hospital(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(hospital_id): Path<Uuid>,
    Json(request): Json<ApprovalRequest>,
) -> Result<Json<StatusChangeResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Attribute the approval to the acting admin from the bearer token.
    let admin_id = extract_claims(&headers)
        .ok()
        .and_then(|c| Uuid::parse_str(&c.sub).ok());

    match state
        .registration_service
        .approve_hospital(hospital_id, admin_id, request.notes)
        .await
    {
        Ok(_) => Ok(Json(StatusChangeResponse {
            message: "Hospital approved successfully".to_string(),
            hospital_id,
            new_status: "Approved".to_string(),
        })),
        Err(e) => {
            let (status_code, code) = match e {
                RegistrationError::NotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
                RegistrationError::InvalidStatusTransition(_, _) => {
                    (StatusCode::CONFLICT, "INVALID_STATUS_TRANSITION")
                }
                RegistrationError::IdentityNotVerified => {
                    (StatusCode::FORBIDDEN, "IDENTITY_NOT_VERIFIED")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "ERROR"),
            };

            Err((
                status_code,
                Json(ErrorResponse {
                    code: code.to_string(),
                    message: e.to_string(),
                }),
            ))
        }
    }
}

/// Reject hospital registration
#[utoipa::path(
    post,
    path = "/api/v1/admin/hospitals/{hospital_id}/reject",
    tag = "admin",
    params(
        ("hospital_id" = Uuid, Path, description = "Hospital ID")
    ),
    request_body = RejectionRequest,
    responses(
        (status = 200, description = "Hospital rejected successfully", body = StatusChangeResponse),
        (status = 400, description = "Validation error", body = ErrorResponse),
        (status = 404, description = "Hospital not found", body = ErrorResponse),
        (status = 409, description = "Invalid status transition", body = ErrorResponse)
    ),
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn reject_hospital(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(hospital_id): Path<Uuid>,
    Json(request): Json<RejectionRequest>,
) -> Result<Json<StatusChangeResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Attribute the rejection to the acting admin from the bearer token.
    let admin_id = extract_claims(&headers)
        .ok()
        .and_then(|c| Uuid::parse_str(&c.sub).ok());

    match state
        .registration_service
        .reject_hospital(hospital_id, admin_id, request.reason)
        .await
    {
        Ok(_) => Ok(Json(StatusChangeResponse {
            message: "Hospital rejected successfully".to_string(),
            hospital_id,
            new_status: "Rejected".to_string(),
        })),
        Err(e) => {
            let status_code = match e {
                RegistrationError::ValidationError(_) => StatusCode::BAD_REQUEST,
                RegistrationError::NotFound(_) => StatusCode::NOT_FOUND,
                RegistrationError::InvalidStatusTransition(_, _) => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };

            Err((
                status_code,
                Json(ErrorResponse {
                    code: "ERROR".to_string(),
                    message: e.to_string(),
                }),
            ))
        }
    }
}

/// List all hospitals with optional filtering and pagination
#[utoipa::path(
    get,
    path = "/api/v1/hospitals",
    tag = "hospitals",
    params(
        ("status" = Option<String>, Query, description = "Filter by status: pending, approved, rejected"),
        ("page" = Option<i64>, Query, description = "Page number (default: 1)"),
        ("page_size" = Option<i64>, Query, description = "Items per page (default: 20, max: 100)")
    ),
    responses(
        (status = 200, description = "Hospitals retrieved successfully", body = HospitalListResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn list_hospitals(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ListHospitalsQuery>,
) -> Result<Json<HospitalListResponse>, (StatusCode, Json<ErrorResponse>)> {
    use crate::models::registration::RegistrationStatus;

    let status_filter = if let Some(status_str) = params.status {
        match status_str.to_lowercase().as_str() {
            "pending" => Some(RegistrationStatus::Pending),
            "approved" => Some(RegistrationStatus::Approved),
            "rejected" => Some(RegistrationStatus::Rejected),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        code: "INVALID_STATUS".to_string(),
                        message: "Status must be one of: pending, approved, rejected".to_string(),
                    }),
                ));
            }
        }
    } else {
        None
    };

    let page = params.page.unwrap_or(1);
    let page_size = params.page_size.unwrap_or(20);

    match state
        .registration_service
        .list_hospitals(status_filter, page, page_size)
        .await
    {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                code: "ERROR".to_string(),
                message: e.to_string(),
            }),
        )),
    }
}

/// Query parameters for listing hospitals
#[derive(Debug, Deserialize, ToSchema)]
pub struct ListHospitalsQuery {
    pub status: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

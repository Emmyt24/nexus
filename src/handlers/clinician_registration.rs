use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;
use validator::Validate;

use crate::models::clinician::WorkerPublicDetail;
use crate::models::clinician_registration::{
    AddBankAccountRequest, BankAccountResponse, CompleteProfileRequest, ProfileResponse,
    SendOtpRequest, SendOtpResponse, VerifyOtpRequest, VerifyOtpResponse,
};
use crate::routes::AppState;
use crate::services::clinician_registration_service::ClinicianRegistrationError;
use crate::utils::errors::{AppError, AppResult};

/// GET /api/v1/workers/{id}
/// Public (ungated) worker profile with rating, completed shifts, verification
/// and location. Excludes contact, bank-account and earnings (admin-only).
#[utoipa::path(
    get,
    path = "/api/v1/workers/{id}",
    tag = "clinicians",
    params(("id" = Uuid, Path, description = "Clinician id")),
    responses(
        (status = 200, description = "Worker public profile", body = WorkerPublicDetail),
        (status = 404, description = "Worker not found")
    )
)]
pub async fn get_worker_public(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<WorkerPublicDetail>> {
    let worker: Option<WorkerPublicDetail> = sqlx::query_as(
        r#"
        SELECT
            c.id,
            c.first_name,
            c.last_name,
            c.specialty::TEXT      AS specialty,
            c.role_title,
            c.license_number,
            c.rating::REAL         AS rating,
            c.rating_count,
            c.acceptance_rate_pct,
            c.availability::TEXT   AS availability,
            c.is_verified,
            c.is_active,
            EXISTS (SELECT 1 FROM identity_verifications iv
                WHERE iv.owner_type = 'clinician' AND iv.owner_id = c.id
                  AND iv.status = 'verified')                                  AS identity_verified,
            (SELECT COUNT(*) FROM shifts s WHERE s.assigned_clinician_id = c.id
                AND s.status = 'completed')::BIGINT                            AS completed_shifts,
            cl.latitude   AS latitude,
            cl.longitude  AS longitude,
            c.created_at
        FROM clinicians c
        LEFT JOIN clinician_locations cl ON cl.clinician_id = c.id
        WHERE c.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;

    let worker = worker.ok_or_else(|| AppError::NotFound(format!("Worker {} not found", id)))?;
    Ok(Json(worker))
}

/// POST /api/v1/clinicians/otp/send
#[utoipa::path(
    post,
    path = "/api/v1/clinicians/otp/send",
    request_body = SendOtpRequest,
    responses(
        (status = 200, description = "OTP sent successfully", body = SendOtpResponse),
        (status = 409, description = "Email already registered"),
        (status = 422, description = "Validation error")
    ),
    tag = "clinicians",
    summary = "Send OTP for clinician registration",
    description = "Send a 6-digit OTP code to the clinician's email to start registration"
)]
pub async fn send_otp(
    State(state): State<AppState>,
    Json(req): Json<SendOtpRequest>,
) -> AppResult<(StatusCode, Json<SendOtpResponse>)> {
    req.validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .clinician_registration_service
        .send_otp(&req.email)
        .await
        .map(|r| (StatusCode::OK, Json(r)))
        .map_err(map_err)
}

/// POST /api/v1/clinicians/otp/verify
#[utoipa::path(
    post,
    path = "/api/v1/clinicians/otp/verify",
    request_body = VerifyOtpRequest,
    responses(
        (status = 201, description = "Account created successfully", body = VerifyOtpResponse),
        (status = 422, description = "Invalid or expired OTP")
    ),
    tag = "clinicians",
    summary = "Verify OTP and create clinician account",
    description = "Verify the OTP code and create a new clinician account with JWT token"
)]
pub async fn verify_otp(
    State(state): State<AppState>,
    Json(req): Json<VerifyOtpRequest>,
) -> AppResult<(StatusCode, Json<VerifyOtpResponse>)> {
    req.validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .clinician_registration_service
        .verify_otp(&req.email, &req.code)
        .await
        .map(|r| (StatusCode::CREATED, Json(r)))
        .map_err(map_err)
}

/// PUT /api/v1/clinicians/{clinician_id}/profile
#[utoipa::path(
    put,
    path = "/api/v1/clinicians/{clinician_id}/profile",
    request_body = CompleteProfileRequest,
    params(
        ("clinician_id" = Uuid, Path, description = "Clinician unique identifier")
    ),
    responses(
        (status = 200, description = "Profile completed successfully", body = ProfileResponse),
        (status = 404, description = "Clinician not found"),
        (status = 422, description = "Validation error")
    ),
    tag = "clinicians",
    summary = "Complete clinician profile",
    description = "Complete the clinician profile with personal and professional information"
)]
pub async fn complete_profile(
    State(state): State<AppState>,
    axum::extract::Path(clinician_id): axum::extract::Path<Uuid>,
    Json(req): Json<CompleteProfileRequest>,
) -> AppResult<Json<ProfileResponse>> {
    req.validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .clinician_registration_service
        .complete_profile(clinician_id, req)
        .await
        .map(Json)
        .map_err(map_err)
}

/// POST /api/v1/clinicians/{clinician_id}/bank-account
#[utoipa::path(
    post,
    path = "/api/v1/clinicians/{clinician_id}/bank-account",
    request_body = AddBankAccountRequest,
    params(
        ("clinician_id" = Uuid, Path, description = "Clinician unique identifier")
    ),
    responses(
        (status = 200, description = "Bank account added successfully", body = BankAccountResponse),
        (status = 404, description = "Clinician not found"),
        (status = 422, description = "Bank account validation failed")
    ),
    tag = "clinicians",
    summary = "Add and validate bank account",
    description = "Add a bank account for the clinician and validate it with Paystack"
)]
pub async fn add_bank_account(
    State(state): State<AppState>,
    axum::extract::Path(clinician_id): axum::extract::Path<Uuid>,
    Json(req): Json<AddBankAccountRequest>,
) -> AppResult<Json<BankAccountResponse>> {
    req.validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .clinician_registration_service
        .add_bank_account(clinician_id, req)
        .await
        .map(Json)
        .map_err(map_err)
}

fn map_err(e: ClinicianRegistrationError) -> AppError {
    match e {
        ClinicianRegistrationError::DuplicateEmail => {
            AppError::Conflict("Email already registered".to_string())
        }
        ClinicianRegistrationError::InvalidOtp => {
            AppError::Validation("Invalid or expired OTP".to_string())
        }
        ClinicianRegistrationError::Validation(msg) => AppError::Validation(msg),
        ClinicianRegistrationError::NotFound => {
            AppError::NotFound("Clinician not found".to_string())
        }
        ClinicianRegistrationError::Payment(e) => {
            AppError::Validation(format!("Bank account validation failed: {}", e))
        }
        ClinicianRegistrationError::IdentityNotVerified => AppError::Forbidden(
            "BVN or NIN must be verified before adding a bank account".to_string(),
        ),
        e => AppError::Internal(anyhow::anyhow!("{}", e)),
    }
}

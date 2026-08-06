use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::routes::AppState;
use crate::services::identity_verification_service::{IdentityError, IdentityKind, IdentityOwner};
use crate::utils::errors::{AppError, AppResult};

#[derive(Debug, Deserialize, ToSchema)]
pub struct InitiateIdentityRequest {
    /// "BVN" or "NIN"
    #[serde(rename = "type")]
    pub id_type: String,
    /// 11-digit BVN or NIN
    pub number: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ValidateIdentityRequest {
    /// "BVN" or "NIN"
    #[serde(rename = "type")]
    pub id_type: String,
    /// OTP sent to the phone registered against the BVN/NIN
    pub otp: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IdentityStatusResponse {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ResolveAccountRequest {
    /// 10-digit NUBAN account number
    pub account_number: String,
    /// SafeHaven bank code (from GET /api/v1/banks)
    pub bank_code: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResolveAccountResponse {
    pub account_name: String,
    pub account_number: String,
    pub bank_code: String,
}

/// POST /api/v1/hospitals/{hospital_id}/identity/initiate
#[utoipa::path(
    post,
    path = "/api/v1/hospitals/{hospital_id}/identity/initiate",
    request_body = InitiateIdentityRequest,
    params(("hospital_id" = Uuid, Path, description = "Hospital unique identifier")),
    responses(
        (status = 200, description = "Verification initiated; OTP sent", body = IdentityStatusResponse),
        (status = 422, description = "Validation error")
    ),
    tag = "identity",
    summary = "Initiate hospital admin BVN/NIN verification"
)]
pub async fn hospital_initiate(
    State(state): State<AppState>,
    axum::extract::Path(hospital_id): axum::extract::Path<Uuid>,
    Json(req): Json<InitiateIdentityRequest>,
) -> AppResult<Json<IdentityStatusResponse>> {
    initiate(&state, IdentityOwner::Hospital, hospital_id, req).await
}

/// POST /api/v1/hospitals/{hospital_id}/identity/validate
#[utoipa::path(
    post,
    path = "/api/v1/hospitals/{hospital_id}/identity/validate",
    request_body = ValidateIdentityRequest,
    params(("hospital_id" = Uuid, Path, description = "Hospital unique identifier")),
    responses(
        (status = 200, description = "Identity verified", body = IdentityStatusResponse),
        (status = 422, description = "Invalid OTP or not initiated")
    ),
    tag = "identity",
    summary = "Validate hospital admin BVN/NIN OTP"
)]
pub async fn hospital_validate(
    State(state): State<AppState>,
    axum::extract::Path(hospital_id): axum::extract::Path<Uuid>,
    Json(req): Json<ValidateIdentityRequest>,
) -> AppResult<Json<IdentityStatusResponse>> {
    validate(&state, IdentityOwner::Hospital, hospital_id, req).await
}

/// POST /api/v1/clinicians/{clinician_id}/identity/initiate
#[utoipa::path(
    post,
    path = "/api/v1/clinicians/{clinician_id}/identity/initiate",
    request_body = InitiateIdentityRequest,
    params(("clinician_id" = Uuid, Path, description = "Clinician unique identifier")),
    responses(
        (status = 200, description = "Verification initiated; OTP sent", body = IdentityStatusResponse),
        (status = 422, description = "Validation error")
    ),
    tag = "identity",
    summary = "Initiate clinician BVN/NIN verification"
)]
pub async fn clinician_initiate(
    State(state): State<AppState>,
    axum::extract::Path(clinician_id): axum::extract::Path<Uuid>,
    Json(req): Json<InitiateIdentityRequest>,
) -> AppResult<Json<IdentityStatusResponse>> {
    initiate(&state, IdentityOwner::Clinician, clinician_id, req).await
}

/// POST /api/v1/clinicians/{clinician_id}/identity/validate
#[utoipa::path(
    post,
    path = "/api/v1/clinicians/{clinician_id}/identity/validate",
    request_body = ValidateIdentityRequest,
    params(("clinician_id" = Uuid, Path, description = "Clinician unique identifier")),
    responses(
        (status = 200, description = "Identity verified", body = IdentityStatusResponse),
        (status = 422, description = "Invalid OTP or not initiated")
    ),
    tag = "identity",
    summary = "Validate clinician BVN/NIN OTP"
)]
pub async fn clinician_validate(
    State(state): State<AppState>,
    axum::extract::Path(clinician_id): axum::extract::Path<Uuid>,
    Json(req): Json<ValidateIdentityRequest>,
) -> AppResult<Json<IdentityStatusResponse>> {
    validate(&state, IdentityOwner::Clinician, clinician_id, req).await
}

/// GET /api/v1/banks
#[utoipa::path(
    get,
    path = "/api/v1/banks",
    responses((status = 200, description = "List of supported banks")),
    tag = "identity",
    summary = "List banks supported by SafeHaven"
)]
pub async fn list_banks(State(state): State<AppState>) -> AppResult<Json<Value>> {
    state
        .safehaven
        .get_bank_list()
        .await
        .map(Json)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to fetch bank list: {e}")))
}

/// POST /api/v1/banks/resolve
#[utoipa::path(
    post,
    path = "/api/v1/banks/resolve",
    request_body = ResolveAccountRequest,
    responses(
        (status = 200, description = "Account resolved", body = ResolveAccountResponse),
        (status = 422, description = "Account could not be resolved")
    ),
    tag = "identity",
    summary = "Resolve a bank account number to its holder name (SafeHaven name enquiry)"
)]
pub async fn resolve_account(
    State(state): State<AppState>,
    Json(req): Json<ResolveAccountRequest>,
) -> AppResult<Json<ResolveAccountResponse>> {
    let account_number = req.account_number.trim();
    if account_number.len() != 10 || !account_number.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::Validation(
            "account_number must be 10 digits".to_string(),
        ));
    }

    let resolved = state
        .safehaven
        .name_enquiry(&req.bank_code, account_number)
        .await
        .map_err(|e| AppError::Validation(format!("Account could not be resolved: {e}")))?;

    Ok(Json(ResolveAccountResponse {
        account_name: resolved.account_name,
        account_number: resolved.account_number,
        bank_code: req.bank_code,
    }))
}

async fn initiate(
    state: &AppState,
    owner: IdentityOwner,
    owner_id: Uuid,
    req: InitiateIdentityRequest,
) -> AppResult<Json<IdentityStatusResponse>> {
    let id_type = IdentityKind::parse(&req.id_type)
        .ok_or_else(|| AppError::Validation("type must be BVN or NIN".to_string()))?;

    state
        .identity_service
        .initiate(owner, owner_id, id_type, &req.number)
        .await
        .map_err(map_err)?;

    Ok(Json(IdentityStatusResponse {
        message: "Verification initiated. An OTP has been sent to the registered phone number."
            .to_string(),
        first_name: None,
        last_name: None,
        full_name: None,
    }))
}

async fn validate(
    state: &AppState,
    owner: IdentityOwner,
    owner_id: Uuid,
    req: ValidateIdentityRequest,
) -> AppResult<Json<IdentityStatusResponse>> {
    let id_type = IdentityKind::parse(&req.id_type)
        .ok_or_else(|| AppError::Validation("type must be BVN or NIN".to_string()))?;

    let payload = state
        .identity_service
        .validate(owner, owner_id, id_type, &req.otp)
        .await
        .map_err(map_err)?;

    let first_name = payload
        .get("firstName")
        .or_else(|| payload.get("first_name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let last_name = payload
        .get("lastName")
        .or_else(|| payload.get("last_name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let full_name = payload
        .get("fullName")
        .or_else(|| payload.get("full_name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Ok(Json(IdentityStatusResponse {
        message: "Identity verified successfully.".to_string(),
        first_name,
        last_name,
        full_name,
    }))
}

fn map_err(e: IdentityError) -> AppError {
    match e {
        IdentityError::Validation(msg) => AppError::Validation(msg),
        IdentityError::NotInitiated => {
            AppError::Validation("Verification has not been initiated".to_string())
        }
        IdentityError::NumberAlreadyInUse => AppError::Conflict(
            "This BVN/NIN is already verified for another account".to_string(),
        ),
        IdentityError::Provider(e) => {
            AppError::Validation(format!("Identity verification failed: {e}"))
        }
        e => AppError::Internal(anyhow::anyhow!("{e}")),
    }
}

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

#[derive(Debug, Serialize, ToSchema)]
pub struct GetIdentityResponse {
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
}

fn title_case(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let is_all_upper = trimmed.chars().any(|c| c.is_alphabetic())
        && trimmed.chars().all(|c| !c.is_alphabetic() || c.is_uppercase());

    if is_all_upper {
        trimmed
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(f) => {
                        f.to_uppercase().collect::<String>()
                            + chars.as_str().to_lowercase().as_str()
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        trimmed.to_string()
    }
}

fn extract_string_by_keys(val: &Value, keys: &[&str]) -> Option<String> {
    match val {
        Value::Object(map) => {
            for &key in keys {
                if let Some(v) = map.get(key) {
                    if let Some(s) = v.as_str() {
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            return Some(title_case(trimmed));
                        }
                    }
                }
            }
            for (_k, v) in map {
                if v.is_object() || v.is_array() {
                    if let Some(found) = extract_string_by_keys(v, keys) {
                        return Some(found);
                    }
                }
            }
            None
        }
        Value::Array(arr) => {
            for item in arr {
                if let Some(found) = extract_string_by_keys(item, keys) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

pub fn extract_identity_names(
    payload: &Value,
) -> (Option<String>, Option<String>, Option<String>) {
    let fn_keys = &[
        "firstName",
        "first_name",
        "first_Name",
        "firstnames",
        "givenName",
        "given_name",
    ];
    let ln_keys = &[
        "lastName",
        "last_name",
        "last_Name",
        "surname",
        "surName",
        "familyName",
        "family_name",
    ];
    let mn_keys = &[
        "middleName",
        "middle_name",
        "otherNames",
        "other_names",
    ];
    let full_keys = &[
        "fullName",
        "full_name",
        "formattedName",
        "formatted_name",
        "name",
    ];

    let mut first_name = extract_string_by_keys(payload, fn_keys);
    let mut last_name = extract_string_by_keys(payload, ln_keys);
    let middle_name = extract_string_by_keys(payload, mn_keys);
    let full_name = extract_string_by_keys(payload, full_keys);

    if (first_name.is_none() || last_name.is_none()) && full_name.is_some() {
        if let Some(ref full) = full_name {
            let parts: Vec<&str> = full.split_whitespace().collect();
            if parts.len() >= 2 {
                if first_name.is_none() {
                    first_name = Some(title_case(parts[0]));
                }
                if last_name.is_none() {
                    last_name = Some(title_case(&parts[1..].join(" ")));
                }
            } else if parts.len() == 1 {
                if first_name.is_none() {
                    first_name = Some(title_case(parts[0]));
                }
            }
        }
    }

    if last_name.is_none() && middle_name.is_some() {
        last_name = middle_name;
    }

    let computed_full_name = full_name.or_else(|| match (&first_name, &last_name) {
        (Some(f), Some(l)) => Some(format!("{f} {l}")),
        (Some(f), None) => Some(f.clone()),
        (None, Some(l)) => Some(l.clone()),
        _ => None,
    });

    (first_name, last_name, computed_full_name)
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

/// GET /api/v1/hospitals/{hospital_id}/identity
#[utoipa::path(
    get,
    path = "/api/v1/hospitals/{hospital_id}/identity",
    params(("hospital_id" = Uuid, Path, description = "Hospital unique identifier")),
    responses(
        (status = 200, description = "Hospital identity status and names", body = GetIdentityResponse)
    ),
    tag = "identity",
    summary = "Get hospital admin verified identity details"
)]
pub async fn hospital_get_identity(
    State(state): State<AppState>,
    axum::extract::Path(hospital_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<GetIdentityResponse>> {
    get_identity(&state, IdentityOwner::Hospital, hospital_id).await
}

/// GET /api/v1/clinicians/{clinician_id}/identity
#[utoipa::path(
    get,
    path = "/api/v1/clinicians/{clinician_id}/identity",
    params(("clinician_id" = Uuid, Path, description = "Clinician unique identifier")),
    responses(
        (status = 200, description = "Clinician identity status and names", body = GetIdentityResponse)
    ),
    tag = "identity",
    summary = "Get clinician verified identity details"
)]
pub async fn clinician_get_identity(
    State(state): State<AppState>,
    axum::extract::Path(clinician_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<GetIdentityResponse>> {
    get_identity(&state, IdentityOwner::Clinician, clinician_id).await
}

async fn get_identity(
    state: &AppState,
    owner: IdentityOwner,
    owner_id: Uuid,
) -> AppResult<Json<GetIdentityResponse>> {
    let payload_row = state
        .identity_service
        .get_verified_payload(owner, owner_id)
        .await
        .map_err(map_err)?;

    if let Some((id_type, payload)) = payload_row {
        let (first_name, last_name, full_name) = extract_identity_names(&payload);
        Ok(Json(GetIdentityResponse {
            verified: true,
            identity_type: Some(id_type.to_uppercase()),
            first_name,
            last_name,
            full_name,
        }))
    } else {
        Ok(Json(GetIdentityResponse {
            verified: false,
            identity_type: None,
            first_name: None,
            last_name: None,
            full_name: None,
        }))
    }
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

    let (first_name, last_name, full_name) = extract_identity_names(&payload);

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_identity_names_bvn_surname() {
        let payload = json!({
            "identity": {
                "firstName": "CHINEDU",
                "surname": "ADEBAYO",
                "middleName": "EMMANUEL"
            }
        });
        let (fn_opt, ln_opt, full_opt) = extract_identity_names(&payload);
        assert_eq!(fn_opt.as_deref(), Some("Chinedu"));
        assert_eq!(ln_opt.as_deref(), Some("Adebayo"));
        assert_eq!(full_opt.as_deref(), Some("Chinedu Adebayo"));
    }

    #[test]
    fn test_extract_identity_names_bvn_details_nested() {
        let payload = json!({
            "bvnDetails": {
                "first_name": "ADAOBI",
                "last_name": "OKAFOR"
            }
        });
        let (fn_opt, ln_opt, full_opt) = extract_identity_names(&payload);
        assert_eq!(fn_opt.as_deref(), Some("Adaobi"));
        assert_eq!(ln_opt.as_deref(), Some("Okafor"));
        assert_eq!(full_opt.as_deref(), Some("Adaobi Okafor"));
    }

    #[test]
    fn test_extract_identity_names_full_name_fallback() {
        let payload = json!({
            "fullName": "JOHN OBI DOE"
        });
        let (fn_opt, ln_opt, full_opt) = extract_identity_names(&payload);
        assert_eq!(fn_opt.as_deref(), Some("John"));
        assert_eq!(ln_opt.as_deref(), Some("Obi Doe"));
        assert_eq!(full_opt.as_deref(), Some("John Obi Doe"));
    }
}



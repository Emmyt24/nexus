use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

use crate::{
    models::user::{
        EmailLoginRequest, EmailOtpVerifyRequest, LoginRequest, LoginResponse, LogoutRequest,
        RefreshTokenRequest, UserResponse, UserRole,
    },
    routes::AppState,
    services::auth_service::AuthError,
    utils::{errors::{AppError, AppResult}, extract_claims},
};

// Email OTP login — the only authentication path.

/// POST /api/v1/auth/otp/send
#[utoipa::path(
    post,
    path = "/api/v1/auth/otp/send",
    request_body = EmailLoginRequest,
    responses(
        (status = 204, description = "OTP sent successfully"),
        (status = 404, description = "Email not found"),
        (status = 422, description = "Validation error")
    ),
    tag = "auth",
    summary = "Send OTP to email",
    description = "Send a 6-digit OTP code to the user's email address for authentication"
)]
pub async fn email_otp_send(
    State(state): State<AppState>,
    Json(payload): Json<EmailLoginRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .auth_service
        .send_login_otp(&payload.email)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(auth_err_to_app)
}

/// POST /api/v1/auth/otp/verify
#[utoipa::path(
    post,
    path = "/api/v1/auth/otp/verify",
    request_body = EmailOtpVerifyRequest,
    responses(
        (status = 200, description = "OTP verified, login successful", body = LoginResponse),
        (status = 401, description = "Invalid or expired OTP"),
        (status = 422, description = "Validation error")
    ),
    tag = "auth",
    summary = "Verify OTP and login",
    description = "Verify the OTP code and complete email-based authentication"
)]
pub async fn email_otp_verify(
    State(state): State<AppState>,
    Json(payload): Json<EmailOtpVerifyRequest>,
) -> AppResult<Json<LoginResponse>> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .auth_service
        .verify_login_otp(&payload.email, &payload.code)
        .await
        .map(Json)
        .map_err(auth_err_to_app)
}

// Admin password login — separate from the OTP path used by regular users.

/// POST /api/v1/auth/admin/login
#[utoipa::path(
    post,
    path = "/api/v1/auth/admin/login",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Login successful", body = LoginResponse),
        (status = 401, description = "Invalid email or password"),
        (status = 403, description = "Not an admin account"),
        (status = 422, description = "Validation error")
    ),
    tag = "auth",
    summary = "Admin password login",
    description = "Password login for admin accounts (super/operations/verification/finance)."
)]
pub async fn admin_login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> AppResult<Json<LoginResponse>> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let resp = state
        .auth_service
        .login_with_password(&payload.email, &payload.password)
        .await
        .map_err(auth_err_to_app)?;

    // This route is admin-only: reject any non-admin role that has a password.
    let is_admin = matches!(
        resp.user.role,
        UserRole::SuperAdmin
            | UserRole::OperationsAdmin
            | UserRole::VerificationAdmin
            | UserRole::FinanceAdmin
    );
    if !is_admin {
        return Err(AppError::Forbidden("Not an admin account".to_string()));
    }

    Ok(Json(resp))
}

// Token refresh

/// POST /api/v1/auth/refresh
#[utoipa::path(
    post,
    path = "/api/v1/auth/refresh",
    request_body = RefreshTokenRequest,
    responses(
        (status = 200, description = "Token refreshed successfully", body = LoginResponse),
        (status = 401, description = "Invalid or expired refresh token"),
        (status = 422, description = "Validation error")
    ),
    tag = "auth",
    summary = "Refresh access token",
    description = "Get a new access token using a valid refresh token"
)]
pub async fn refresh_token(
    State(state): State<AppState>,
    Json(payload): Json<RefreshTokenRequest>,
) -> AppResult<Json<LoginResponse>> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .auth_service
        .refresh_token(&payload.refresh_token)
        .await
        .map(Json)
        .map_err(auth_err_to_app)
}

// Logout

/// POST /api/v1/auth/logout
#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    request_body = LogoutRequest,
    responses(
        (status = 204, description = "Logout successful"),
        (status = 401, description = "Invalid refresh token"),
        (status = 422, description = "Validation error")
    ),
    tag = "auth",
    summary = "Logout user",
    description = "Revoke the refresh token and logout the user"
)]
pub async fn logout(
    State(state): State<AppState>,
    Json(payload): Json<LogoutRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .auth_service
        .logout(&payload.refresh_token)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(auth_err_to_app)
}

// GET /api/v1/auth/me — return the logged-in user's profile.

#[derive(Debug, Serialize, ToSchema)]
pub struct MeResponse {
    pub user: UserResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clinician: Option<ClinicianProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hospital: Option<HospitalProfile>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClinicianProfile {
    pub id: Uuid,
    pub specialty: String,
    pub role_title: String,
    pub rating: f32,
    pub rating_count: i32,
    pub is_verified: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HospitalProfile {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub registration_number: String,
    pub verification_status: String,
    pub admin_registration_status: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
}

/// GET /api/v1/auth/me
#[utoipa::path(
    get,
    path = "/api/v1/auth/me",
    responses(
        (status = 200, description = "Current user profile", body = MeResponse),
        (status = 401, description = "Missing or invalid token"),
        (status = 404, description = "User not found")
    ),
    tag = "auth",
    summary = "Get the logged-in user's profile",
    description = "Returns the base user record plus role-specific detail (clinician or hospital)."
)]
pub async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<MeResponse>> {
    let claims = extract_claims(&headers)?;
    let user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let user = state
        .auth_service
        .fetch_user_by_id(user_id)
        .await
        .map_err(auth_err_to_app)?;

    // Role-specific detail. Fetched with lightweight, targeted queries so we
    // don't pull unrelated data.
    let clinician = sqlx::query_as::<_, (Uuid, String, String, f32, i32, bool)>(
        r#"
        SELECT id,
               specialty::text AS specialty,
               role_title,
               rating,
               rating_count,
               is_verified
        FROM clinicians
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?
    .map(|(id, specialty, role_title, rating, rating_count, is_verified)| ClinicianProfile {
        id,
        specialty,
        role_title,
        rating,
        rating_count,
        is_verified,
    });

    let hospital = if let Some(hospital_id) = user.hospital_id {
        sqlx::query_as::<_, (Uuid, String, String, String, String, Option<String>, Option<DateTime<Utc>>)>(
            r#"
            SELECT id,
                   name,
                   email,
                   registration_number,
                   verification_status::text        AS verification_status,
                   admin_registration_status::text  AS admin_registration_status,
                   approved_at
            FROM hospitals
            WHERE id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Database)?
        .map(
            |(id, name, email, registration_number, verification_status, admin_registration_status, approved_at)| {
                HospitalProfile {
                    id,
                    name,
                    email,
                    registration_number,
                    verification_status,
                    admin_registration_status,
                    approved_at,
                }
            },
        )
    } else {
        None
    };

    Ok(Json(MeResponse {
        user: UserResponse::from(user),
        clinician,
        hospital,
    }))
}

// Error mapping

fn auth_err_to_app(e: AuthError) -> AppError {
    match e {
        AuthError::NotFound => AppError::NotFound("User not found".to_string()),
        AuthError::InvalidOtp => AppError::Unauthorized("Invalid or expired OTP".to_string()),
        AuthError::InvalidToken => AppError::Unauthorized("Invalid or expired token".to_string()),
        AuthError::InvalidCredentials => {
            AppError::Unauthorized("Invalid email or password".to_string())
        }
        AuthError::Deactivated => AppError::Forbidden("Account is deactivated".to_string()),
        AuthError::EmailQueue(e) => AppError::Internal(anyhow::anyhow!("Email queue error: {}", e)),
        AuthError::Database(e) => AppError::Database(e),
        AuthError::Internal(msg) => AppError::Internal(anyhow::anyhow!(msg)),
    }
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

/// Role of a user within the platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "user_role", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    /// Hospital administrator
    HospitalAdmin,
    /// Clinical staff member — health worker (clinician)
    HealthWorker,
    /// NexusCare platform super-admin (holds every permission)
    SuperAdmin,
    /// Platform monitoring, content, user support, shift cancellation
    OperationsAdmin,
    /// Manual verification of hospital and worker licenses/documents
    VerificationAdmin,
    /// Payouts, refunds, dispute resolution, financial reports
    FinanceAdmin,
}

/// A platform user — can be a hospital admin or staff member.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: Uuid,
    pub hospital_id: Option<Uuid>,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub phone: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: UserRole,
    /// Display label shown in the header, e.g. "LUTH Admin"
    pub role_label: Option<String>,
    /// URL to the user's avatar image (shown in the top-right header)
    pub avatar_url: Option<String>,
    pub is_active: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload for creating a new user account.
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct CreateUserRequest {
    pub hospital_id: Option<Uuid>,

    #[validate(length(min = 1, max = 100, message = "First name is required"))]
    pub first_name: String,

    #[validate(length(min = 1, max = 100, message = "Last name is required"))]
    pub last_name: String,

    #[validate(email(message = "A valid email address is required"))]
    pub email: String,

    /// Optional E.164 phone number (e.g. +2348012345678) — not used for login
    pub phone: Option<String>,

    #[validate(length(min = 8, message = "Password must be at least 8 characters"))]
    pub password: String,

    pub role: UserRole,
}

/// Login request payload.
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct LoginRequest {
    #[validate(email)]
    pub email: String,

    #[validate(length(min = 1))]
    pub password: String,
}

/// Safe user response (no password hash).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserResponse {
    pub id: Uuid,
    pub hospital_id: Option<Uuid>,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub phone: Option<String>,
    pub role: UserRole,
    pub role_label: Option<String>,
    pub avatar_url: Option<String>,
    pub is_active: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<User> for UserResponse {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            hospital_id: u.hospital_id,
            first_name: u.first_name,
            last_name: u.last_name,
            email: u.email,
            phone: u.phone,
            role: u.role,
            role_label: u.role_label,
            avatar_url: u.avatar_url,
            is_active: u.is_active,
            last_login_at: u.last_login_at,
            created_at: u.created_at,
        }
    }
}

/// JWT claims payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (user id)
    pub sub: String,
    pub email: String,
    pub role: UserRole,
    pub hospital_id: Option<String>,
    /// Expiry (Unix timestamp)
    pub exp: usize,
    /// Issued at (Unix timestamp)
    pub iat: usize,
}

/// Request to send a login OTP to an email address (AC-01).
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct EmailLoginRequest {
    #[validate(email(message = "A valid email address is required"))]
    pub email: String,
}

/// Request to verify an email OTP and complete login (AC-01).
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct EmailOtpVerifyRequest {
    #[validate(email(message = "A valid email address is required"))]
    pub email: String,
    #[validate(length(equal = 6, message = "OTP must be 6 digits"))]
    pub code: String,
}

/// Request to initiate a password reset (AC-03).
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct ForgotPasswordRequest {
    #[validate(email(message = "A valid email address is required"))]
    pub email: String,
}

/// Request to complete a password reset (AC-03).
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct ResetPasswordRequest {
    #[validate(length(min = 1, message = "Token is required"))]
    pub token: String,
    #[validate(length(min = 8, message = "Password must be at least 8 characters"))]
    pub new_password: String,
}

/// Request to refresh an access token (AC-04).
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct RefreshTokenRequest {
    #[validate(length(min = 1, message = "Refresh token is required"))]
    pub refresh_token: String,
}

/// Request to logout (AC-05).
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct LogoutRequest {
    #[validate(length(min = 1, message = "Refresh token is required"))]
    pub refresh_token: String,
}

/// Successful login response with tokens and role-based redirect (AC-06).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u64,
    /// Role-specific dashboard path for client-side redirect (AC-06)
    pub redirect_to: String,
    pub user: UserResponse,
}

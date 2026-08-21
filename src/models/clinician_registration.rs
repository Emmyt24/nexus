use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

// Enums

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "clinician_role", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ClinicianRole {
    Doctor,
    Nurse,
    LabTechnician,
    Pharmacist,
    Radiographer,
    Physiotherapist,
    Other,
}

// DB row types

#[derive(Debug, Clone, FromRow)]
pub struct OtpCode {
    pub id: Uuid,
    pub phone: String,
    pub code: String,
    pub expires_at: DateTime<Utc>,
    pub used: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClinicianBankAccount {
    pub id: Uuid,
    pub clinician_id: Uuid,
    pub account_number: String, // encrypted at rest
    pub bank_code: String,
    pub account_name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Request / response types

/// Send OTP to email
#[derive(Debug, Deserialize, Validate, ToSchema)]
pub struct SendOtpRequest {
    #[validate(email(message = "A valid email address is required"))]
    #[schema(example = "clinician@example.com")]
    pub email: String,
}

/// Verify OTP and create account
#[derive(Debug, Deserialize, Validate, ToSchema)]
pub struct VerifyOtpRequest {
    #[validate(email(message = "A valid email address is required"))]
    pub email: String,
    /// 6-digit email OTP (named `code` to match the login/registration OTP flows).
    #[validate(length(equal = 6, message = "OTP must be 6 digits"))]
    pub code: String,
}

/// Complete clinician profile
#[derive(Debug, Deserialize, Validate, ToSchema)]
pub struct CompleteProfileRequest {
    #[validate(length(min = 1, max = 100))]
    pub first_name: String,
    #[validate(length(min = 1, max = 100))]
    pub last_name: String,
    pub role: ClinicianRole,
    #[validate(length(min = 2, max = 100))]
    pub license_number: String,
    pub specialty: crate::models::clinician::ClinicalSpecialty,
}

/// Set the clinician's profile image (Cloudinary secure URL, uploaded by the
/// frontend after obtaining a signature from `GET /api/v1/uploads/signature`).
#[derive(Debug, Deserialize, Validate, ToSchema)]
pub struct SetAvatarRequest {
    #[validate(url(message = "avatar_url must be a valid URL"))]
    pub avatar_url: String,
}

/// Add bank account
#[derive(Debug, Deserialize, Validate, ToSchema)]
pub struct AddBankAccountRequest {
    #[validate(length(equal = 10, message = "Account number must be 10 digits"))]
    pub account_number: String,
    #[validate(length(min = 3, max = 10))]
    pub bank_code: String,
}

// Responses

#[derive(Debug, Serialize, ToSchema)]
pub struct SendOtpResponse {
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VerifyOtpResponse {
    pub clinician_id: Uuid,
    pub access_token: String,
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProfileResponse {
    pub clinician_id: Uuid,
    pub first_name: String,
    pub last_name: String,
    pub role: ClinicianRole,
    pub license_number: String,
    pub email: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BankAccountResponse {
    pub account_name: String,
    pub account_number_masked: String,
    pub bank_code: String,
}

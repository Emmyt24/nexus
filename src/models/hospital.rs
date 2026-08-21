use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

/// Public hospital profile with ratings, shift counts, documents status and
/// payment-method presence — the general (ungated) drill-down. Excludes the
/// sensitive wallet balance, admin contact and spend that stay admin-only.
#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct HospitalPublicDetail {
    pub id: Uuid,
    pub name: String,
    pub registration_number: String,
    pub email: String,
    pub address: String,
    pub phone_number: String,
    pub verification_status: String,
    pub registration_step: String,
    pub setup_progress_percent: i16,
    pub logo_url: Option<String>,
    pub created_at: DateTime<Utc>,
    // Admin / contact person (from the hospital row; always populated)
    pub admin_first_name: Option<String>,
    pub admin_last_name: Option<String>,
    pub contact_email: String,
    pub contact_phone: String,
    // Shift aggregates
    pub total_shifts: i64,
    pub active_shifts: i64,
    pub completed_shifts: i64,
    // Identity + compliance + payment presence
    pub identity_verified: bool,
    pub documents_status: String,
    pub payment_method_on_file: bool,
    // Ratings (workers rating this hospital) + 4-dimension breakdown
    pub average_rating: Option<f64>,
    pub rating_count: i64,
    pub rating_staff_support: Option<f64>,
    pub rating_equipment_availability: Option<f64>,
    pub rating_communication: Option<f64>,
    pub rating_payment_timeliness: Option<f64>,
}

/// Verification status of a hospital against the CAC register.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "verification_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Pending,
    Verified,
    Rejected,
    UnderReview,
}

/// Registration step in the 4-step onboarding flow matching the UI labels:
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "registration_step", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStep {
    /// Step 1 — Basic institutional identity (name, reg number, email, address, phone)
    ProfileSetup,
    /// Step 2 — Legal document uploads (operational license, MCS, TCC)
    Credentials,
    /// Step 3 — Under NexusCare compliance review (24-48 business hours)
    Verification,
    /// Step 4 — Onboarding complete, hospital accredited on the platform
    AccessGranted,
}

/// Core hospital record — maps to the `hospitals` table.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Hospital {
    pub id: Uuid,
    pub name: String,
    /// CAC registration number (e.g. "RC-1234567")
    pub registration_number: String,
    pub email: String,
    pub address: String,
    pub phone_number: String,
    pub verification_status: VerificationStatus,
    pub registration_step: RegistrationStep,
    /// Admin registration status for approval workflow
    pub admin_registration_status: Option<crate::models::registration::RegistrationStatus>,
    /// System admin who approved the hospital registration
    pub approved_by: Option<Uuid>,
    /// Timestamp when hospital was approved by system admin
    pub approved_at: Option<DateTime<Utc>>,
    /// Reason for rejection if status is rejected
    pub rejection_reason: Option<String>,
    /// User ID of the hospital admin who initiated registration
    pub admin_user_id: Option<Uuid>,
    /// Timestamp when documents were submitted for compliance review (Step 3).
    pub legal_submitted_at: Option<DateTime<Utc>>,
    /// Overall setup progress percentage (0–100) shown in the progress bar.
    pub setup_progress_percent: i16,
    /// Optional logo/profile image URL
    pub logo_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload for Step 1 (Setup) of hospital registration.
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct CreateHospitalRequest {
    #[validate(length(
        min = 2,
        max = 255,
        message = "Hospital name must be between 2 and 255 characters"
    ))]
    pub name: String,

    #[validate(length(
        min = 3,
        max = 50,
        message = "Registration number must be between 3 and 50 characters"
    ))]
    pub registration_number: String,

    #[validate(email(message = "A valid email address is required"))]
    pub email: String,

    #[validate(length(
        min = 5,
        max = 500,
        message = "Address must be between 5 and 500 characters"
    ))]
    pub address: String,

    #[validate(length(
        min = 7,
        max = 20,
        message = "Phone number must be between 7 and 20 characters"
    ))]
    pub phone_number: String,
}

/// Payload for updating hospital details.
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct UpdateHospitalRequest {
    #[validate(length(min = 2, max = 255))]
    pub name: Option<String>,

    #[validate(email)]
    pub email: Option<String>,

    #[validate(length(min = 5, max = 500))]
    pub address: Option<String>,

    #[validate(length(min = 7, max = 20))]
    pub phone_number: Option<String>,

    pub logo_url: Option<String>,

    /// Optional exact coordinates (map/GPS picker). When both are provided the
    /// hospital's stored location is updated.
    #[validate(range(min = -90.0, max = 90.0))]
    pub latitude: Option<f64>,

    #[validate(range(min = -180.0, max = 180.0))]
    pub longitude: Option<f64>,
}

/// Response shape returned to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HospitalResponse {
    pub id: Uuid,
    pub name: String,
    pub registration_number: String,
    pub email: String,
    pub address: String,
    pub phone_number: String,
    pub verification_status: VerificationStatus,
    pub registration_step: RegistrationStep,
    pub admin_registration_status: Option<crate::models::registration::RegistrationStatus>,
    pub approved_by: Option<Uuid>,
    pub approved_at: Option<DateTime<Utc>>,
    pub rejection_reason: Option<String>,
    pub legal_submitted_at: Option<DateTime<Utc>>,
    pub setup_progress_percent: i16,
    pub logo_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Hospital> for HospitalResponse {
    fn from(h: Hospital) -> Self {
        Self {
            id: h.id,
            name: h.name,
            registration_number: h.registration_number,
            email: h.email,
            address: h.address,
            phone_number: h.phone_number,
            verification_status: h.verification_status,
            registration_step: h.registration_step,
            admin_registration_status: h.admin_registration_status,
            approved_by: h.approved_by,
            approved_at: h.approved_at,
            rejection_reason: h.rejection_reason,
            legal_submitted_at: h.legal_submitted_at,
            setup_progress_percent: h.setup_progress_percent,
            logo_url: h.logo_url,
            created_at: h.created_at,
            updated_at: h.updated_at,
        }
    }
}

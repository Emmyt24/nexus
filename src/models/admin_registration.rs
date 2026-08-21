use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

use super::registration::{ActorType, AuditEventType, RegistrationStatus};
use crate::utils::validation::{validate_email_rfc5322, validate_phone_e164};

// Core Domain Models for Hospital Admin Registration (AC-01 to AC-05)

/// Address structure for hospital registration
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct Address {
    #[validate(length(min = 5, max = 255))]
    pub line1: String,

    #[validate(length(max = 255))]
    pub line2: Option<String>,

    #[validate(length(min = 2, max = 100))]
    pub city: String,

    #[validate(length(min = 2, max = 100))]
    pub state: String,

    #[validate(length(min = 3, max = 20))]
    pub postal_code: String,

    #[validate(length(min = 2, max = 100))]
    pub country: String,

    /// Optional exact coordinates from a map/GPS picker. When both are provided
    /// they are stored as-is (overriding address geocoding); otherwise the
    /// backend geocodes `line1/city/state/country`.
    #[validate(range(min = -90.0, max = 90.0, message = "latitude must be between -90 and 90"))]
    pub latitude: Option<f64>,

    #[validate(range(min = -180.0, max = 180.0, message = "longitude must be between -180 and 180"))]
    pub longitude: Option<f64>,
}

/// Geographic coordinates
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Coordinates {
    pub latitude: f64,
    pub longitude: f64,
}

/// Payment method type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "payment_method_type", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum PaymentMethodType {
    Card,
    BankAccount,
}

/// Payment details for registration (will be tokenized, never stored raw)
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct PaymentDetails {
    pub method_type: PaymentMethodType,

    // For card payments
    #[validate(length(min = 13, max = 19))]
    pub card_number: Option<String>,

    #[validate(range(min = 1, max = 12))]
    pub expiry_month: Option<u8>,

    #[validate(range(min = 2024, max = 2100))]
    pub expiry_year: Option<u16>,

    #[validate(length(min = 3, max = 4))]
    pub cvv: Option<String>,

    // For bank account payments
    #[validate(length(min = 10, max = 10))]
    pub account_number: Option<String>,

    #[validate(length(min = 3, max = 10))]
    pub bank_code: Option<String>,
}

// Request Models

/// Hospital registration request (AC-01)
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct HospitalRegistrationRequest {
    #[validate(length(min = 2, max = 200))]
    pub hospital_name: String,

    /// First name of the hospital admin (the human contact) — used to
    #[validate(length(min = 1, max = 100))]
    pub admin_first_name: String,

    /// Last name of the hospital admin.
    #[validate(length(min = 1, max = 100))]
    pub admin_last_name: String,

    /// Hospital + admin contact email. Login OTPs go here after approval.
    #[validate(custom(function = "validate_email_rfc5322"))]
    pub email: String,

    #[validate(custom(function = "validate_phone_e164"))]
    pub phone: String,

    #[validate(length(min = 5, max = 50))]
    pub registration_number: String,

    #[validate(nested)]
    pub address: Address,
}

/// Admin approval request (AC-05)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub notes: Option<String>,
}

/// Admin rejection request (AC-05)
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct RejectionRequest {
    #[validate(length(min = 10, max = 500))]
    pub reason: String,
}

// Response Models

/// Hospital registration response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HospitalRegistrationResponse {
    pub hospital_id: Uuid,
    pub status: RegistrationStatus,
    pub message: String,
    pub next_steps: Vec<String>,
}

/// Registration status response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationStatusResponse {
    pub hospital_id: Uuid,
    pub status: RegistrationStatus,
    pub created_at: DateTime<Utc>,
    pub approved_at: Option<DateTime<Utc>>,
    pub approved_by: Option<Uuid>,
}

/// Approval response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub hospital_id: Uuid,
    pub status: RegistrationStatus,
    pub message: String,
}

/// Rejection response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectionResponse {
    pub hospital_id: Uuid,
    pub status: RegistrationStatus,
    pub reason: String,
    pub message: String,
}

// Database Models

/// New hospital record for creation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewHospital {
    pub name: String,
    pub email: String,
    pub phone: String,
    pub registration_number: String,
    pub admin_user_id: Option<Uuid>,
    /// Stashed on the row at registration time so we can create the admin
    /// users row at approval time (not before).
    pub admin_first_name: String,
    pub admin_last_name: String,
}

/// New location record for creation (AC-02)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewLocation {
    pub hospital_id: Uuid,
    pub address_line1: String,
    pub address_line2: Option<String>,
    pub city: String,
    pub state: String,
    pub postal_code: String,
    pub country: String,
    pub latitude: f64,
    pub longitude: f64,
    pub service_radius_km: f64,
}

/// Audit entry for registration events
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AuditEntry {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub event_type: AuditEventType,
    pub actor_id: Option<Uuid>,
    pub actor_type: ActorType,
    pub old_value: Option<serde_json::Value>,
    pub new_value: Option<serde_json::Value>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// New audit entry for creation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAuditEntry {
    pub hospital_id: Uuid,
    pub event_type: AuditEventType,
    pub actor_id: Option<Uuid>,
    pub actor_type: ActorType,
    pub old_value: Option<serde_json::Value>,
    pub new_value: Option<serde_json::Value>,
    pub metadata: Option<serde_json::Value>,
}

/// Registration result returned by service layer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HospitalRegistrationResult {
    pub hospital_id: Uuid,
    pub status: RegistrationStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::validation::{validate_email_rfc5322, validate_phone_e164};
    use proptest::prelude::*;
    use validator::Validate;

    // Property 2: Registration data validation
    proptest! {
        #[test]
        fn property_2_valid_data_passes_validation(
            hospital_name in "[A-Za-z ]{2,200}",
            registration_number in "[A-Z0-9-]{5,50}",
        ) {
            let request = HospitalRegistrationRequest {
                hospital_name,
                admin_first_name: "Admin".to_string(), admin_last_name: "User".to_string(), email: "admin@hospital.com".to_string(), phone: "+2348012345678".to_string(), registration_number,
                address: Address {
                    line1: "123 Test Street".to_string(), line2: None,
                    city: "Lagos".to_string(), state: "Lagos".to_string(), postal_code: "100001".to_string(), country: "Nigeria".to_string(), latitude: None, longitude: None, },
            };

            prop_assert!(request.validate(). is_ok());
        }
    }

    // Property 21: Email format validation
    proptest! {
        #[test]
        fn property_21_valid_emails_pass(
            local_part in "[a-z0-9]{1,20}",
            domain in "[a-z0-9]{1,10}",
            tld in "[a-z]{2,5}",
        ) {
            let email = format!("{}@{}.{}", local_part, domain, tld);
            prop_assert!(validate_email_rfc5322(&email).is_ok());
        }
    }

    // Property 22: Phone format validation
    proptest! {
        #[test]
        fn property_22_valid_e164_phones_pass(
            country_code in 1u16..999,
            subscriber in 1000000u64..9999999999999u64,
        ) {
            let phone = format!("+{}{}", country_code, subscriber);
            if phone.len() <= 16 {
                prop_assert!(validate_phone_e164(&phone).is_ok());
            }
        }
    }

    #[test]
    fn test_email_validation() {
        assert!(validate_email_rfc5322("test@example.com").is_ok());
        assert!(validate_email_rfc5322("invalid").is_err());
    }

    #[test]
    fn test_phone_validation() {
        assert!(validate_phone_e164("+2348012345678").is_ok());
        assert!(validate_phone_e164("08012345678").is_err());
    }
}

use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::models::clinician_registration::{
    AddBankAccountRequest, BankAccountResponse, CompleteProfileRequest, ProfileResponse,
    SendOtpResponse, VerifyOtpResponse,
};
use crate::repositories::clinician::{ClinicianRepoError, ClinicianRepository};
use crate::services::email_outbox_service::{EmailOutboxError, EmailOutboxService};
use crate::services::email_templates;
use crate::services::encryption::EncryptionService;
use crate::services::identity_verification_service::{IdentityOwner, IdentityVerificationService};
use crate::services::safehaven::{SafeHavenClient, SafeHavenError};
use crate::utils::validation::validate_email_rfc5322;

#[derive(Debug, thiserror::Error)]
pub enum ClinicianRegistrationError {
    #[error("Email already registered")]
    DuplicateEmail,
    #[error("Invalid or expired OTP")]
    InvalidOtp,
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Email queue error: {0}")]
    EmailQueue(#[from] EmailOutboxError),
    #[error("Payment provider error: {0}")]
    Payment(#[from] SafeHavenError),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Repository error: {0}")]
    Repository(#[from] ClinicianRepoError),
    #[error("Encryption error: {0}")]
    Encryption(String),
    #[error("Clinician not found")]
    NotFound,
    #[error("BVN or NIN must be verified before adding a bank account")]
    IdentityNotVerified,
}

pub struct ClinicianRegistrationService {
    repo: Arc<ClinicianRepository>,
    email_outbox: Arc<EmailOutboxService>,
    safehaven: Arc<SafeHavenClient>,
    encryption: Arc<EncryptionService>,
    pool: PgPool,
    identity_service: Arc<IdentityVerificationService>,
}

impl ClinicianRegistrationService {
    pub fn new(
        repo: Arc<ClinicianRepository>,
        email_outbox: Arc<EmailOutboxService>,
        safehaven: Arc<SafeHavenClient>,
        encryption: Arc<EncryptionService>,
        pool: PgPool,
        identity_service: Arc<IdentityVerificationService>,
    ) -> Self {
        Self {
            repo,
            email_outbox,
            safehaven,
            encryption,
            pool,
            identity_service,
        }
    }

    // Send OTP
    pub async fn send_otp(
        &self,
        email: &str,
    ) -> Result<SendOtpResponse, ClinicianRegistrationError> {
        // Normalise the email (trim + lowercase) so capitalisation differences
        let email = email.trim().to_lowercase();
        let email = email.as_str();
        validate_email_rfc5322(email)
            .map_err(|e| ClinicianRegistrationError::Validation(e.to_string()))?;

        // Duplicate prevention — an email already on any users row
        if self.repo.email_exists(email).await? {
            return Err(ClinicianRegistrationError::DuplicateEmail);
        }

        let code = generate_otp();
        let expires_at = Utc::now() + Duration::minutes(10);

        // Persist OTP
        sqlx::query(
            "INSERT INTO clinician_email_otp_codes (email, code, expires_at) VALUES ($1, $2, $3)",
        )
        .bind(email)
        .bind(&code)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        let content = email_templates::email_otp(&code, 10);
        self.email_outbox.enqueue_email(email, &content).await?;

        Ok(SendOtpResponse {
            message: "OTP sent successfully".to_string(),
        })
    }

    // Verify OTP → create account → return JWT
    pub async fn verify_otp(
        &self,
        email: &str,
        otp: &str,
    ) -> Result<VerifyOtpResponse, ClinicianRegistrationError> {
        // Fetch the latest unused, non-expired OTP for this email
        let row: Option<(Uuid,)> = sqlx::query_as(
            r#"
            SELECT id FROM clinician_email_otp_codes
            WHERE email = $1 AND code = $2 AND used = FALSE AND expires_at > NOW()
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(email)
        .bind(otp)
        .fetch_optional(&self.pool)
        .await?;

        let otp_id = row
            .map(|(id,)| id)
            .ok_or(ClinicianRegistrationError::InvalidOtp)?;

        // Mark OTP as used
        sqlx::query("UPDATE clinician_email_otp_codes SET used = TRUE WHERE id = $1")
            .bind(otp_id)
            .execute(&self.pool)
            .await?;

        // Create user + clinician in a transaction
        let mut tx = self.pool.begin().await?;
        let (clinician_id, user_id) = self.repo.create_clinician(&mut tx, email).await?;
        tx.commit().await?;

        let token = issue_jwt(user_id);

        let content = email_templates::clinician_welcome(None);
        self.email_outbox.enqueue_email(email, &content).await?;

        Ok(VerifyOtpResponse {
            clinician_id,
            access_token: token,
            message: "Account created successfully".to_string(),
        })
    }

    // Complete profile
    pub async fn complete_profile(
        &self,
        clinician_id: Uuid,
        req: CompleteProfileRequest,
    ) -> Result<ProfileResponse, ClinicianRegistrationError> {
        if req.first_name.trim().is_empty() || req.last_name.trim().is_empty() {
            return Err(ClinicianRegistrationError::Validation(
                "Full name is required".to_string(),
            ));
        }
        if req.license_number.trim().is_empty() {
            return Err(ClinicianRegistrationError::Validation(
                "License number is required".to_string(),
            ));
        }

        self.repo
            .update_profile(
                clinician_id,
                &req.first_name,
                &req.last_name,
                &req.role,
                &req.license_number,
                &req.specialty,
            )
            .await
            .map_err(|e| match e {
                ClinicianRepoError::NotFound => ClinicianRegistrationError::NotFound,
                other => ClinicianRegistrationError::Repository(other),
            })?;

        let email = self
            .repo
            .find_email_by_clinician_id(clinician_id)
            .await?
            .unwrap_or_default();

        Ok(ProfileResponse {
            clinician_id,
            first_name: req.first_name,
            last_name: req.last_name,
            role: req.role,
            license_number: req.license_number,
            email,
        })
    }

    // Add bank account (SafeHaven name-enquiry validation)
    pub async fn add_bank_account(
        &self,
        clinician_id: Uuid,
        req: AddBankAccountRequest,
    ) -> Result<BankAccountResponse, ClinicianRegistrationError> {
        // Gate: BVN or NIN must be verified before linking a payout account (either suffices)
        let verified = self
            .identity_service
            .any_verified(IdentityOwner::Clinician, clinician_id)
            .await
            .map_err(|e| ClinicianRegistrationError::Validation(e.to_string()))?;
        if !verified {
            return Err(ClinicianRegistrationError::IdentityNotVerified);
        }

        // Validate with SafeHaven (returns account holder name + sessionId).
        let resolved = self
            .safehaven
            .name_enquiry(&req.bank_code, &req.account_number)
            .await?;

        // Encrypt account number before storage
        let encrypted = self
            .encryption
            .encrypt_token(&req.account_number)
            .map_err(|e| ClinicianRegistrationError::Encryption(e.to_string()))?;

        self.repo
            .upsert_bank_account(
                clinician_id,
                &encrypted,
                &req.bank_code,
                &resolved.account_name,
            )
            .await?;

        let masked = mask_account(&req.account_number);

        Ok(BankAccountResponse {
            account_name: resolved.account_name,
            account_number_masked: masked,
            bank_code: req.bank_code,
        })
    }
}

// Helpers

fn generate_otp() -> String {
    // Use 4 bytes from a random UUID to produce a 6-digit code
    let id = Uuid::new_v4();
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) % 1_000_000;
    format!("{:06}", n)
}

fn mask_account(account: &str) -> String {
    if account.len() <= 4 {
        return "*".repeat(account.len());
    }
    format!("{}****{}", &account[..3], &account[account.len() - 3..])
}

/// Minimal JWT issuance — reuses the same secret as the rest of the app.
/// `sub` is the users.id (not clinicians.id) to match extract_claims across the app.
fn issue_jwt(user_id: Uuid) -> String {
    use chrono::Utc;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Claims {
        sub: String,
        role: String,
        exp: usize,
        iat: usize,
    }

    let secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| "dev_secret".to_string());
    let now = Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: user_id.to_string(),
        role: "health_worker".to_string(),
        exp: now + 86400,
        iat: now,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap_or_else(|_| "token_error".to_string())
}

// Extend ClinicianRepository with a reverse lookup helper is in repositories/clinician.rs

#[cfg(test)]
mod tests {
    use super::*;

    // --- OTP generation ---

    #[test]
    fn otp_is_six_digits() {
        for _ in 0..20 {
            let code = generate_otp();
            assert_eq!(code.len(), 6, "OTP must be 6 chars: {}", code);
            assert!(
                code.chars().all(|c| c.is_ascii_digit()),
                "OTP must be numeric: {}",
                code
            );
        }
    }

    #[test]
    fn otp_values_in_range() {
        for _ in 0..50 {
            let code = generate_otp();
            let n: u32 = code.parse().expect("OTP must parse as u32");
            assert!(n < 1_000_000, "OTP out of range: {}", n);
        }
    }

    // --- Account masking ---

    #[test]
    fn mask_account_hides_middle_digits() {
        let masked = mask_account("0123456789");
        assert!(masked.starts_with("012"), "Should keep first 3: {}", masked);
        assert!(masked.ends_with("789"), "Should keep last 3: {}", masked);
        assert!(masked.contains("****"), "Should have mask: {}", masked);
    }

    #[test]
    fn mask_account_short_input() {
        let masked = mask_account("123");
        assert_eq!(masked, "***");
    }

    // --- Phone validation (via validate_phone_e164) ---

    #[test]
    fn valid_e164_phones_accepted() {
        use crate::utils::validation::validate_phone_e164;
        assert!(validate_phone_e164("+2348012345678").is_ok());
        assert!(validate_phone_e164("+14155552671").is_ok());
    }

    #[test]
    fn invalid_phones_rejected() {
        use crate::utils::validation::validate_phone_e164;
        assert!(validate_phone_e164("08012345678").is_err()); // no +
        assert!(validate_phone_e164("+0123456789").is_err()); // starts with 0
        assert!(validate_phone_e164("not-a-phone").is_err());
    }

    // --- Bank account validation (SafeHaven mock) ---

    fn mock_safehaven() -> crate::services::safehaven::SafeHavenClient {
        crate::services::safehaven::SafeHavenClient::new(
            String::new(),
            "test-client".to_string(),
            "test-ibs".to_string(),
            "0000000000".to_string(),
            "090286".to_string(),
        )
    }

    #[tokio::test]
    async fn safehaven_mock_resolves_bank_account() {
        let client = mock_safehaven();
        let result = client.name_enquiry("058", "0123456789").await;
        assert!(result.is_ok(), "Mock should succeed: {:?}", result);
        let resolved = result.unwrap();
        assert!(!resolved.account_name.is_empty());
        assert!(resolved.session_id.is_some(), "must surface sessionId");
    }

    #[tokio::test]
    async fn safehaven_mock_returns_account_number() {
        let client = mock_safehaven();
        let result = client.name_enquiry("058", "0123456789").await.unwrap();
        assert_eq!(result.account_number, "0123456789");
    }
}

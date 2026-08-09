use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::models::admin_registration::{HospitalRegistrationRequest, NewHospital};
use crate::models::hospital::Hospital;
use crate::models::registration::RegistrationStatus;
use crate::repositories::hospital::{HospitalRepository, RepositoryError};
use crate::services::audit_service::{AuditService, AuditServiceError, RegistrationDetails};
use crate::services::email_outbox_service::EmailOutboxService;
use crate::services::email_templates;
use crate::services::identity_verification_service::{
    IdentityOwner, IdentityVerificationService,
};
use crate::services::location_service::{LocationService, LocationServiceError};
use crate::services::wallet_service::WalletService;
use crate::utils::validation::{validate_email_rfc5322, validate_phone_e164};

#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    #[error("Validation failed: {0}")]
    ValidationError(String),

    #[error("Duplicate registration for email: {0}")]
    DuplicateRegistration(String),

    #[error("Hospital not found: {0}")]
    NotFound(Uuid),

    #[error("Invalid status transition from {0:?} to {1:?}")]
    InvalidStatusTransition(RegistrationStatus, RegistrationStatus),

    #[error("Location service error: {0}")]
    LocationError(#[from] LocationServiceError),

    #[error("Repository error: {0}")]
    RepositoryError(#[from] RepositoryError),

    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),

    #[error("Audit service error: {0}")]
    AuditError(#[from] AuditServiceError),

    #[error("External service error: {0}")]
    ExternalServiceError(String),

    #[error("Hospital admin BVN or NIN must be verified before approval")]
    IdentityNotVerified,
}

/// Result type for hospital registration
#[derive(Debug, Clone)]
pub struct HospitalRegistrationResult {
    pub hospital_id: Uuid,
    pub status: RegistrationStatus,
    pub message: String,
    pub next_steps: Vec<String>,
}

/// Core service orchestrating the complete hospital registration workflow
pub struct RegistrationService {
    hospital_repo: Arc<HospitalRepository>,
    location_service: Arc<LocationService>,
    audit_service: Arc<AuditService>,
    email_outbox: Arc<EmailOutboxService>,
    wallet_service: Arc<WalletService>,
    db_pool: PgPool,
    identity_service: Arc<IdentityVerificationService>,
}

impl RegistrationService {
    pub fn new(
        hospital_repo: Arc<HospitalRepository>,
        location_service: Arc<LocationService>,
        audit_service: Arc<AuditService>,
        email_outbox: Arc<EmailOutboxService>,
        wallet_service: Arc<WalletService>,
        db_pool: PgPool,
        identity_service: Arc<IdentityVerificationService>,
    ) -> Self {
        Self {
            hospital_repo,
            location_service,
            audit_service,
            email_outbox,
            wallet_service,
            db_pool,
            identity_service,
        }
    }

    /// Register a new hospital with complete workflow
    pub async fn register_hospital(
        &self,
        _user_id: Uuid,
        request: HospitalRegistrationRequest,
    ) -> Result<HospitalRegistrationResult, RegistrationError> {
        // Normalise the email so capitalisation differences can't smuggle
        let mut request = request;
        request.email = request.email.trim().to_lowercase();
        self.validate_registration_data(&request)?;

        // Reject if this email already belongs to ANY user (hospital admin,
        let user_exists: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT id FROM users WHERE email = $1")
                .bind(&request.email)
                .fetch_optional(&self.db_pool)
                .await?;
        if user_exists.is_some() || self.check_duplicate_registration(&request.email).await? {
            return Err(RegistrationError::DuplicateRegistration(
                request.email.clone(),
            ));
        }

        let mut tx = self.db_pool.begin().await?;

        let new_hospital = NewHospital {
            name: request.hospital_name.clone(),
            email: request.email.clone(),
            phone: request.phone.clone(),
            registration_number: request.registration_number.clone(),
            admin_user_id: None,
            admin_first_name: request.admin_first_name.clone(),
            admin_last_name: request.admin_last_name.clone(),
        };

        let hospital = self.hospital_repo.create(&mut tx, new_hospital).await?;
        let hospital_id = hospital.id;

        let _location = self
            .location_service
            .geocode_and_store(&mut tx, hospital_id, request.address.clone())
            .await?;

        // The admin's `users` row is intentionally NOT created here — it's
        // created at approval time by `approve_hospital` so no orphan account
        // exists while the hospital is pending or rejected.
        tx.commit().await?;

        let registration_details = RegistrationDetails {
            hospital_name: request.hospital_name.clone(),
            email: request.email.clone(),
            registration_number: request.registration_number.clone(),
        };

        if let Err(e) = self
            .audit_service
            .log_registration(hospital_id, None, registration_details)
            .await
        {
            eprintln!("Warning: Failed to log registration audit: {}", e);
        }

        if let Err(e) = self
            .email_outbox
            .enqueue_email(
                &request.email,
                &email_templates::hospital_registration_submitted(&request.hospital_name),
            )
            .await
        {
            eprintln!("Warning: Failed to queue registration email: {}", e);
        }

        Ok(HospitalRegistrationResult {
            hospital_id,
            status: RegistrationStatus::Pending,
            message: "Hospital registration submitted successfully. Awaiting admin approval."
                .to_string(),
            next_steps: vec![
                "Upload required documents (license, accreditation)".to_string(),
                "Wait for system administrator review".to_string(),
                "You will receive an email notification upon approval".to_string(),
            ],
        })
    }

    /// Validate registration data
    fn validate_registration_data(
        &self,
        request: &HospitalRegistrationRequest,
    ) -> Result<(), RegistrationError> {
        // Validate hospital name
        if request.hospital_name.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "Hospital name cannot be empty".to_string(),
            ));
        }

        if request.hospital_name.len() < 2 || request.hospital_name.len() > 200 {
            return Err(RegistrationError::ValidationError(
                "Hospital name must be between 2 and 200 characters".to_string(),
            ));
        }

        // Validate email (RFC 5322)
        if validate_email_rfc5322(&request.email).is_err() {
            return Err(RegistrationError::ValidationError(
                "Invalid email format (must conform to RFC 5322)".to_string(),
            ));
        }

        // Validate phone (E.164)
        if validate_phone_e164(&request.phone).is_err() {
            return Err(RegistrationError::ValidationError(
                "Invalid phone format (must conform to E.164)".to_string(),
            ));
        }

        // Validate registration number
        if request.registration_number.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "Registration number cannot be empty".to_string(),
            ));
        }

        if request.registration_number.len() < 5 || request.registration_number.len() > 50 {
            return Err(RegistrationError::ValidationError(
                "Registration number must be between 5 and 50 characters".to_string(),
            ));
        }

        // Validate address
        if request.address.line1.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "Address line 1 cannot be empty".to_string(),
            ));
        }

        if request.address.city.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "City cannot be empty".to_string(),
            ));
        }

        if request.address.state.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "State cannot be empty".to_string(),
            ));
        }

        if request.address.postal_code.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "Postal code cannot be empty".to_string(),
            ));
        }

        if request.address.country.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "Country cannot be empty".to_string(),
            ));
        }

        Ok(())
    }

    /// Check if a hospital with the given email already exists
    async fn check_duplicate_registration(&self, email: &str) -> Result<bool, RegistrationError> {
        match self.hospital_repo.find_by_email(email).await? {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }

    /// Approve a pending hospital registration
    pub async fn approve_hospital(
        &self,
        hospital_id: Uuid,
        admin_id: Option<Uuid>,
        notes: Option<String>,
    ) -> Result<(), RegistrationError> {
        let mut tx = self.db_pool.begin().await?;

        let hospital = self
            .hospital_repo
            .find_by_id(hospital_id)
            .await?
            .ok_or(RegistrationError::NotFound(hospital_id))?;

        if hospital.admin_registration_status != Some(RegistrationStatus::Pending) {
            return Err(RegistrationError::InvalidStatusTransition(
                hospital
                    .admin_registration_status
                    .unwrap_or(RegistrationStatus::Pending),
                RegistrationStatus::Approved,
            ));
        }

        // Gate: the hospital admin's BVN or NIN must be verified (either suffices)
        let identity_verified = self
            .identity_service
            .any_verified(IdentityOwner::Hospital, hospital_id)
            .await
            .map_err(|e| RegistrationError::ExternalServiceError(e.to_string()))?;
        if !identity_verified {
            return Err(RegistrationError::IdentityNotVerified);
        }

        self.hospital_repo
            .update_status(
                &mut tx,
                hospital_id,
                RegistrationStatus::Approved,
                admin_id,
                None,
            )
            .await?;

        // Create the hospital-admin `users` row now that the hospital is
        // approved. Registration stashed the admin's name on the hospitals
        // row; here we materialise the user account so OTP login works.
        // Idempotent: if a users row already exists for this hospital (e.g.
        // approval was retried), just activate it.
        let (admin_first_name, admin_last_name): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT admin_first_name, admin_last_name FROM hospitals WHERE id = $1",
        )
        .bind(hospital_id)
        .fetch_one(&mut *tx)
        .await?;

        let admin_user_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO users (hospital_id, first_name, last_name, email, phone, role, is_active)
            VALUES ($1, $2, $3, $4, $5, 'hospital_admin', TRUE)
            ON CONFLICT (email) DO UPDATE
              SET is_active  = TRUE,
                  updated_at = NOW()
            RETURNING id
            "#,
        )
        .bind(hospital_id)
        .bind(admin_first_name.unwrap_or_default())
        .bind(admin_last_name.unwrap_or_default())
        .bind(&hospital.email)
        .bind(&hospital.phone_number)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query("UPDATE hospitals SET admin_user_id = $1 WHERE id = $2")
            .bind(admin_user_id)
            .bind(hospital_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        if let Err(e) = self
            .audit_service
            .log_status_change(
                hospital_id,
                admin_id,
                RegistrationStatus::Pending,
                RegistrationStatus::Approved,
                notes,
            )
            .await
        {
            eprintln!("Warning: Failed to log approval audit: {}", e);
        }

        if let Err(e) = self
            .email_outbox
            .enqueue_email(
                &hospital.email,
                &email_templates::hospital_registration_approved(&hospital.name, Utc::now()),
            )
            .await
        {
            eprintln!("Warning: Failed to queue approval email: {}", e);
        }

        // Ensure a wallet row exists. SafeHaven sub-account provisioning is a
        // separate, admin-driven step (it needs a fresh OTP) — see the
        // /wallet/sub-account/{initiate,provision} endpoints.
        if let Err(e) = self.wallet_service.ensure_wallet(hospital_id).await {
            eprintln!(
                "Warning: Failed to ensure wallet row for hospital {}: {}",
                hospital_id, e
            );
        }

        Ok(())
    }
    /// Reject a pending hospital registration
    pub async fn reject_hospital(
        &self,
        hospital_id: Uuid,
        admin_id: Option<Uuid>,
        reason: String,
    ) -> Result<(), RegistrationError> {
        if reason.trim().is_empty() {
            return Err(RegistrationError::ValidationError(
                "Rejection reason cannot be empty".to_string(),
            ));
        }

        if reason.len() < 10 || reason.len() > 500 {
            return Err(RegistrationError::ValidationError(
                "Rejection reason must be between 10 and 500 characters".to_string(),
            ));
        }

        let mut tx = self.db_pool.begin().await?;

        let hospital = self
            .hospital_repo
            .find_by_id(hospital_id)
            .await?
            .ok_or(RegistrationError::NotFound(hospital_id))?;

        if hospital.admin_registration_status != Some(RegistrationStatus::Pending) {
            return Err(RegistrationError::InvalidStatusTransition(
                hospital
                    .admin_registration_status
                    .unwrap_or(RegistrationStatus::Pending),
                RegistrationStatus::Rejected,
            ));
        }

        self.hospital_repo
            .update_status(
                &mut tx,
                hospital_id,
                RegistrationStatus::Rejected,
                admin_id,
                Some(reason.clone()),
            )
            .await?;

        tx.commit().await?;

        if let Err(e) = self
            .audit_service
            .log_status_change(
                hospital_id,
                admin_id,
                RegistrationStatus::Pending,
                RegistrationStatus::Rejected,
                Some(reason.clone()),
            )
            .await
        {
            eprintln!("Warning: Failed to log rejection audit: {}", e);
        }

        if let Err(e) = self
            .email_outbox
            .enqueue_email(
                &hospital.email,
                &email_templates::hospital_registration_rejected(&hospital.name, &reason),
            )
            .await
        {
            eprintln!("Warning: Failed to queue rejection email: {}", e);
        }

        Ok(())
    }

    /// Get registration status for a hospital
    pub async fn get_registration_status(
        &self,
        hospital_id: Uuid,
    ) -> Result<RegistrationStatusResponse, RegistrationError> {
        let hospital = self
            .hospital_repo
            .find_by_id(hospital_id)
            .await?
            .ok_or(RegistrationError::NotFound(hospital_id))?;

        Ok(RegistrationStatusResponse {
            hospital_id: hospital.id,
            hospital_name: hospital.name,
            status: hospital
                .admin_registration_status
                .unwrap_or(RegistrationStatus::Pending),
            created_at: hospital.created_at,
            updated_at: hospital.updated_at,
            approved_at: None, // Will be populated from hospital.approved_at when available
            rejection_reason: None, // Will be populated from hospital.rejection_reason when available
        })
    }

    /// List all hospitals with optional status filter and pagination
    pub async fn list_hospitals(
        &self,
        status_filter: Option<RegistrationStatus>,
        page: i64,
        page_size: i64,
    ) -> Result<HospitalListResponse, RegistrationError> {
        // Validate pagination parameters
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100); // Max 100 items per page
        let offset = (page - 1) * page_size;

        // Get hospitals
        let hospitals = self
            .hospital_repo
            .list_all(status_filter, page_size, offset)
            .await?;

        // Get total count
        let total = self.hospital_repo.count_all(status_filter).await?;

        // Calculate pagination metadata
        let total_pages = (total as f64 / page_size as f64).ceil() as i64;

        // Build the summaries, then enrich the page with completed-shift counts
        // and average ratings in a single batch query (no N+1).
        let mut summaries: Vec<HospitalSummary> =
            hospitals.into_iter().map(HospitalSummary::from).collect();
        let ids: Vec<Uuid> = summaries.iter().map(|h| h.id).collect();
        if !ids.is_empty() {
            let aggregates: Vec<(Uuid, i64, Option<f64>)> = sqlx::query_as(
                r#"
                SELECT
                    h.id,
                    (SELECT COUNT(*) FROM shifts s
                        WHERE s.hospital_id = h.id AND s.status = 'completed')::BIGINT AS completed_shifts,
                    (SELECT AVG(sr.score)::FLOAT8 FROM shift_ratings sr
                        WHERE sr.ratee_kind = 'hospital' AND sr.ratee_id = h.id)       AS average_rating
                FROM hospitals h
                WHERE h.id = ANY($1)
                "#,
            )
            .bind(&ids)
            .fetch_all(&self.db_pool)
            .await?;

            // Merge aggregates back onto their matching summary by id.
            for (id, completed, avg) in aggregates {
                if let Some(h) = summaries.iter_mut().find(|h| h.id == id) {
                    h.completed_shifts = completed;
                    h.average_rating = avg;
                }
            }
        }

        Ok(HospitalListResponse {
            hospitals: summaries,
            pagination: PaginationMetadata {
                current_page: page,
                page_size,
                total_items: total,
                total_pages,
                has_next: page < total_pages,
                has_previous: page > 1,
            },
        })
    }
}

/// Response for listing hospitals
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HospitalListResponse {
    pub hospitals: Vec<HospitalSummary>,
    pub pagination: PaginationMetadata,
}

/// Summary information for a hospital in list view
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HospitalSummary {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub phone_number: String,
    pub registration_number: String,
    pub status: Option<RegistrationStatus>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub approved_at: Option<chrono::DateTime<chrono::Utc>>,
    // Enriched card fields (from the same hospital row — no extra query).
    pub address: String,
    pub verification_status: String,
    pub registration_step: String,
    pub setup_progress_percent: i16,
    pub logo_url: Option<String>,
    // Aggregates for the admin list (image 1): filled by list_hospitals.
    pub completed_shifts: i64,
    pub average_rating: Option<f64>,
}

impl From<Hospital> for HospitalSummary {
    fn from(hospital: Hospital) -> Self {
        // Serialize the enums to their snake_case wire form for the card.
        let verification_status = serde_json::to_value(&hospital.verification_status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let registration_step = serde_json::to_value(&hospital.registration_step)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        Self {
            id: hospital.id,
            name: hospital.name,
            email: hospital.email,
            phone_number: hospital.phone_number,
            registration_number: hospital.registration_number,
            status: hospital.admin_registration_status,
            created_at: hospital.created_at,
            approved_at: hospital.approved_at,
            address: hospital.address,
            verification_status,
            registration_step,
            setup_progress_percent: hospital.setup_progress_percent,
            logo_url: hospital.logo_url,
            // Defaults; list_hospitals fills these from a batch aggregate query.
            completed_shifts: 0,
            average_rating: None,
        }
    }
}

/// Pagination metadata
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PaginationMetadata {
    pub current_page: i64,
    pub page_size: i64,
    pub total_items: i64,
    pub total_pages: i64,
    pub has_next: bool,
    pub has_previous: bool,
}

/// Response type for registration status queries
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RegistrationStatusResponse {
    pub hospital_id: Uuid,
    pub hospital_name: String,
    pub status: RegistrationStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub approved_at: Option<chrono::DateTime<chrono::Utc>>,
    pub rejection_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests for validation logic
    #[test]
    fn test_validate_registration_data() {
        // Tests will be added here
    }

    // Property tests will be implemented in Task 10.5 (integration tests)
}

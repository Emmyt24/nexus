use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::admin_registration::NewHospital;
use crate::models::hospital::Hospital;
use crate::models::registration::RegistrationStatus;

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),

    #[error("Hospital not found: {0}")]
    NotFound(Uuid),

    #[error("Duplicate email: {0}")]
    DuplicateEmail(String),
}

/// Repository for hospital data persistence operations
pub struct HospitalRepository {
    pool: PgPool,
}

impl HospitalRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a new hospital record within a transaction
    pub async fn create(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital: NewHospital,
    ) -> Result<Hospital, RepositoryError> {
        let result = sqlx::query_as::<_, Hospital>(
            r#"
            INSERT INTO hospitals (
                name,
                registration_number,
                email,
                address,
                phone_number,
                admin_user_id,
                admin_first_name,
                admin_last_name,
                admin_registration_status,
                verification_status,
                registration_step
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', 'pending', 'profile_setup')
            RETURNING
                id, name, registration_number, email, address, phone_number,
                verification_status, registration_step,
                admin_registration_status, approved_by, approved_at, rejection_reason, admin_user_id,
                legal_submitted_at, setup_progress_percent, logo_url, created_at, updated_at
            "#,
        )
        .bind(&hospital.name)
        .bind(&hospital.registration_number)
        .bind(&hospital.email)
        .bind(&hospital.address)
        .bind(&hospital.phone)
        .bind(hospital.admin_user_id)
        .bind(&hospital.admin_first_name)
        .bind(&hospital.admin_last_name)
        .fetch_one(&mut **tx)
        .await;

        match result {
            Ok(hospital) => Ok(hospital),
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                Err(RepositoryError::DuplicateEmail(hospital.email))
            }
            Err(e) => Err(RepositoryError::DatabaseError(e)),
        }
    }

    /// Find hospital by ID
    pub async fn find_by_id(&self, hospital_id: Uuid) -> Result<Option<Hospital>, RepositoryError> {
        let hospital = sqlx::query_as::<_, Hospital>(
            r#"
            SELECT 
                id, name, registration_number, email, address, phone_number,
                verification_status, registration_step,
                admin_registration_status, approved_by, approved_at, rejection_reason, admin_user_id,
                legal_submitted_at, setup_progress_percent, logo_url, created_at, updated_at
            FROM hospitals
            WHERE id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(hospital)
    }

    /// Find hospital by email
    pub async fn find_by_email(&self, email: &str) -> Result<Option<Hospital>, RepositoryError> {
        let hospital = sqlx::query_as::<_, Hospital>(
            r#"
            SELECT 
                id, name, registration_number, email, address, phone_number,
                verification_status, registration_step,
                admin_registration_status, approved_by, approved_at, rejection_reason, admin_user_id,
                legal_submitted_at, setup_progress_percent, logo_url, created_at, updated_at
            FROM hospitals
            WHERE email = $1
            "#,
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        Ok(hospital)
    }

    /// Update hospital registration status
    pub async fn update_status(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        status: RegistrationStatus,
        admin_id: Option<Uuid>,
        rejection_reason: Option<String>,
    ) -> Result<(), RepositoryError> {
        let result = sqlx::query(
            r#"
            UPDATE hospitals
            SET 
                admin_registration_status = $2,
                approved_at = CASE WHEN $2 = 'approved' THEN NOW() ELSE approved_at END,
                approved_by = $3,
                rejection_reason = $4,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(hospital_id)
        .bind(status)
        .bind(admin_id)
        .bind(rejection_reason)
        .execute(&mut **tx)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(hospital_id));
        }

        Ok(())
    }

    /// List all pending hospital registrations
    pub async fn list_pending(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Hospital>, RepositoryError> {
        let hospitals = sqlx::query_as::<_, Hospital>(
            r#"
            SELECT 
                id, name, registration_number, email, address, phone_number,
                verification_status, registration_step,
                admin_registration_status, approved_by, approved_at, rejection_reason, admin_user_id,
                legal_submitted_at, setup_progress_percent, logo_url, created_at, updated_at
            FROM hospitals
            WHERE admin_registration_status = 'pending'
            ORDER BY created_at DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(hospitals)
    }

    /// List all hospitals with optional status filter
    pub async fn list_all(
        &self,
        status_filter: Option<RegistrationStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Hospital>, RepositoryError> {
        let hospitals = if let Some(status) = status_filter {
            sqlx::query_as::<_, Hospital>(
                r#"
                SELECT 
                    id, name, registration_number, email, address, phone_number,
                    verification_status, registration_step,
                    admin_registration_status, approved_by, approved_at, rejection_reason, admin_user_id,
                    legal_submitted_at, setup_progress_percent, logo_url, created_at, updated_at
                FROM hospitals
                WHERE admin_registration_status = $1
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(status)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, Hospital>(
                r#"
                SELECT 
                    id, name, registration_number, email, address, phone_number,
                    verification_status, registration_step,
                    admin_registration_status, approved_by, approved_at, rejection_reason, admin_user_id,
                    legal_submitted_at, setup_progress_percent, logo_url, created_at, updated_at
                FROM hospitals
                ORDER BY created_at DESC
                LIMIT $1 OFFSET $2
                "#,
            )
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(hospitals)
    }

    /// Count total hospitals with optional status filter
    pub async fn count_all(
        &self,
        status_filter: Option<RegistrationStatus>,
    ) -> Result<i64, RepositoryError> {
        let count = if let Some(status) = status_filter {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT COUNT(*)
                FROM hospitals
                WHERE admin_registration_status = $1
                "#,
            )
            .bind(status)
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT COUNT(*)
                FROM hospitals
                "#,
            )
            .fetch_one(&self.pool)
            .await?
        };

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests will be added here
}

// Note: Full integration tests with database will be in Task 10.5
#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    // Property 3: Unique hospital identifiers

    #[test]
    fn test_duplicate_email_detection() {
        // This will be tested in integration tests with actual database
    }

    // Property 4: Registration data persistence round-trip
}

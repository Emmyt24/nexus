use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::clinician::{ClinicalSpecialty, ClinicianAdminSummary};
use crate::models::clinician_registration::{ClinicianBankAccount, ClinicianRole};

#[derive(Debug, thiserror::Error)]
pub enum ClinicianRepoError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Email already registered")]
    DuplicateEmail,
    #[error("Clinician not found")]
    NotFound,
}

pub struct ClinicianRepository {
    pool: PgPool,
}

impl ClinicianRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Check if email is already registered
    pub async fn email_exists(&self, email: &str) -> Result<bool, ClinicianRepoError> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT COUNT(*) FROM users WHERE email = $1")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(c,)| c > 0).unwrap_or(false))
    }

    /// Create user + clinician row atomically. OTP-only auth: the

    /// Create the `users` + `clinicians` pair atomically. Returns
    /// `(clinician_id, user_id)` so callers can put the user_id in the JWT
    /// `sub` claim, matching the convention used by the rest of the app.
    pub async fn create_clinician(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        email: &str,
    ) -> Result<(Uuid, Uuid), ClinicianRepoError> {
        let user_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO users (first_name, last_name, email, role)
            VALUES ('', '', $1, 'health_worker')
            RETURNING id
            "#,
        )
        .bind(email)
        .fetch_one(&mut **tx)
        .await?;

        let clinician_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO clinicians (id, user_id, first_name, last_name, specialty, role_title)
            VALUES (gen_random_uuid(), $1, '', '', 'other', '')
            RETURNING id
            "#,
        )
        .bind(user_id)
        .fetch_one(&mut **tx)
        .await?;

        Ok((clinician_id, user_id))
    }

    /// Save completed profile fields
    pub async fn update_profile(
        &self,
        clinician_id: Uuid,
        first_name: &str,
        last_name: &str,
        role: &ClinicianRole,
        license_number: &str,
        specialty: &ClinicalSpecialty,
    ) -> Result<(), ClinicianRepoError> {
        // Also update the linked user row
        sqlx::query(
            r#"
            UPDATE users u
            SET first_name = $2, last_name = $3
            FROM clinicians c
            WHERE c.id = $1 AND c.user_id = u.id
            "#,
        )
        .bind(clinician_id)
        .bind(first_name)
        .bind(last_name)
        .execute(&self.pool)
        .await?;

        let result = sqlx::query(
            r#"
            UPDATE clinicians
            SET first_name = $2, last_name = $3, clinician_role = $4,
                license_number = $5, specialty = $6, role_title = $7
            WHERE id = $1
            "#,
        )
        .bind(clinician_id)
        .bind(first_name)
        .bind(last_name)
        .bind(role)
        .bind(license_number)
        .bind(specialty)
        .bind(format!("{:?}", role))
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(ClinicianRepoError::NotFound);
        }

        Ok(())
    }

    /// Set the clinician's profile image URL (and mirror it onto the linked
    /// user row). `url` is a Cloudinary secure URL the frontend already uploaded.
    pub async fn set_avatar_url(
        &self,
        clinician_id: Uuid,
        url: &str,
    ) -> Result<(), ClinicianRepoError> {
        sqlx::query(
            r#"
            UPDATE users u
               SET avatar_url = $2, updated_at = NOW()
              FROM clinicians c
             WHERE c.id = $1 AND c.user_id = u.id
            "#,
        )
        .bind(clinician_id)
        .bind(url)
        .execute(&self.pool)
        .await?;

        let result = sqlx::query(
            r#"UPDATE clinicians SET avatar_url = $2, updated_at = NOW() WHERE id = $1"#,
        )
        .bind(clinician_id)
        .bind(url)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(ClinicianRepoError::NotFound);
        }
        Ok(())
    }

    /// Upsert bank account (encrypted account number)
    pub async fn upsert_bank_account(
        &self,
        clinician_id: Uuid,
        account_number_encrypted: &str,
        bank_code: &str,
        account_name: &str,
    ) -> Result<(), ClinicianRepoError> {
        sqlx::query(
            r#"
            INSERT INTO clinician_bank_accounts
                (clinician_id, account_number, bank_code, account_name)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (clinician_id)
            DO UPDATE SET
                account_number = EXCLUDED.account_number,
                bank_code      = EXCLUDED.bank_code,
                account_name   = EXCLUDED.account_name,
                updated_at     = NOW()
            "#,
        )
        .bind(clinician_id)
        .bind(account_number_encrypted)
        .bind(bank_code)
        .bind(account_name)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Fetch bank account for a clinician
    pub async fn get_bank_account(
        &self,
        clinician_id: Uuid,
    ) -> Result<Option<ClinicianBankAccount>, ClinicianRepoError> {
        let row = sqlx::query_as::<_, ClinicianBankAccount>(
            "SELECT * FROM clinician_bank_accounts WHERE clinician_id = $1",
        )
        .bind(clinician_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Get email for a given clinician_id
    pub async fn find_email_by_clinician_id(
        &self,
        clinician_id: Uuid,
    ) -> Result<Option<String>, ClinicianRepoError> {
        let row: Option<(String,)> = sqlx::query_as(
            r#"
            SELECT u.email
            FROM clinicians c
            JOIN users u ON c.user_id = u.id
            WHERE c.id = $1
            "#,
        )
        .bind(clinician_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(email,)| email))
    }

    pub async fn list_completed_clinicians(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ClinicianAdminSummary>, ClinicianRepoError> {
        let rows = sqlx::query_as::<_, ClinicianAdminSummary>(
            r#"
            SELECT c.id, c.user_id, c.first_name, c.last_name, u.email,
                   c.license_number, c.clinician_role as role, c.specialty,
                   c.is_verified, c.is_active, c.created_at,
                   c.rating::REAL AS rating, c.rating_count, c.availability,
                   (SELECT COUNT(*) FROM shifts s WHERE s.assigned_clinician_id = c.id
                        AND s.status = 'completed')::BIGINT AS completed_shifts,
                   cl.latitude  AS latitude,
                   cl.longitude AS longitude
            FROM clinicians c
            JOIN users u ON c.user_id = u.id
            LEFT JOIN clinician_locations cl ON cl.clinician_id = c.id
            WHERE c.first_name <> ''
              AND c.last_name <> ''
              AND c.license_number IS NOT NULL
              AND c.license_number <> ''
              AND c.clinician_role IS NOT NULL
            ORDER BY c.created_at DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    pub async fn count_completed_clinicians(&self) -> Result<i64, ClinicianRepoError> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM clinicians c
            WHERE c.first_name <> ''
              AND c.last_name <> ''
              AND c.license_number IS NOT NULL
              AND c.license_number <> ''
              AND c.clinician_role IS NOT NULL
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(count)
    }
}

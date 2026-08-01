use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum IdentityRepoError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Minimal projection of an identity_verifications row.
#[derive(Debug, Clone)]
pub struct IdentityVerificationRow {
    pub provider_identity_id: Option<String>,
    pub identity_number: String,
    pub status: String,
}

pub struct IdentityVerificationRepository {
    pool: PgPool,
}

impl IdentityVerificationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert (or overwrite a stale row) as `pending` with the SafeHaven id.
    pub async fn upsert_pending(
        &self,
        owner_type: &str,
        owner_id: Uuid,
        id_type: &str,
        encrypted_number: &str,
        number_hash: &str,
        provider_identity_id: &str,
    ) -> Result<Uuid, IdentityRepoError> {
        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO identity_verifications
                (owner_type, owner_id, identity_type, identity_number,
                 number_hash, provider_identity_id, status)
            VALUES ($1::identity_owner, $2, $3::identity_kind, $4, $5, $6, 'pending')
            ON CONFLICT (owner_type, owner_id, identity_type)
            DO UPDATE SET identity_number      = EXCLUDED.identity_number,
                          number_hash          = EXCLUDED.number_hash,
                          provider_identity_id = EXCLUDED.provider_identity_id,
                          status               = 'pending',
                          provider_payload     = NULL,
                          verified_at          = NULL,
                          updated_at           = NOW()
            RETURNING id
            "#,
        )
        .bind(owner_type)
        .bind(owner_id)
        .bind(id_type)
        .bind(encrypted_number)
        .bind(number_hash)
        .bind(provider_identity_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Whether some OTHER owner already has a verified row with the same
    /// (identity_type, number_hash). Used to reject BVN/NIN reuse.
    pub async fn is_number_taken(
        &self,
        id_type: &str,
        number_hash: &str,
        exclude_owner_type: &str,
        exclude_owner_id: Uuid,
    ) -> Result<bool, IdentityRepoError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM identity_verifications
            WHERE identity_type = $1::identity_kind
              AND number_hash   = $2
              AND status        = 'verified'
              AND NOT (owner_type = $3::identity_owner AND owner_id = $4)
            "#,
        )
        .bind(id_type)
        .bind(number_hash)
        .bind(exclude_owner_type)
        .bind(exclude_owner_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn mark_verified(
        &self,
        owner_type: &str,
        owner_id: Uuid,
        id_type: &str,
        payload: &Value,
    ) -> Result<(), IdentityRepoError> {
        sqlx::query(
            r#"
            UPDATE identity_verifications
               SET status           = 'verified',
                   provider_payload = $4,
                   verified_at      = NOW(),
                   updated_at       = NOW()
             WHERE owner_type = $1::identity_owner
               AND owner_id   = $2
               AND identity_type = $3::identity_kind
            "#,
        )
        .bind(owner_type)
        .bind(owner_id)
        .bind(id_type)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(
        &self,
        owner_type: &str,
        owner_id: Uuid,
        id_type: &str,
    ) -> Result<Option<IdentityVerificationRow>, IdentityRepoError> {
        let row: Option<(Option<String>, String, String)> = sqlx::query_as(
            r#"
            SELECT provider_identity_id, identity_number, status::text
            FROM identity_verifications
            WHERE owner_type = $1::identity_owner
              AND owner_id   = $2
              AND identity_type = $3::identity_kind
            "#,
        )
        .bind(owner_type)
        .bind(owner_id)
        .bind(id_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(provider_identity_id, identity_number, status)| IdentityVerificationRow {
                provider_identity_id,
                identity_number,
                status,
            },
        ))
    }

    /// True if EITHER BVN or NIN has a `verified` row for this owner (either
    /// one satisfies identity verification; both are no longer required).
    pub async fn any_verified(
        &self,
        owner_type: &str,
        owner_id: Uuid,
    ) -> Result<bool, IdentityRepoError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(DISTINCT identity_type)
            FROM identity_verifications
            WHERE owner_type = $1::identity_owner
              AND owner_id   = $2
              AND status     = 'verified'
              AND identity_type IN ('bvn', 'nin')
            "#,
        )
        .bind(owner_type)
        .bind(owner_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count >= 1)
    }
}

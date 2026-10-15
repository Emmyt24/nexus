use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::admin_registration::{AuditEntry, NewAuditEntry};
use crate::models::registration::AuditEventType;

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),
}

/// Repository for immutable audit log persistence
pub struct AuditRepository {
    pool: PgPool,
}

impl AuditRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a new audit entry (immutable)
    pub async fn create(&self, entry: NewAuditEntry) -> Result<AuditEntry, AuditError> {
        let result = sqlx::query_as::<_, AuditEntry>(
            r#"
            INSERT INTO hospital_registration_audit (
                hospital_id,
                event_type,
                actor_id,
                actor_type,
                old_value,
                new_value,
                metadata
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING 
                id, hospital_id, event_type, actor_id, actor_type,
                old_value, new_value, metadata, created_at
            "#,
        )
        .bind(entry.hospital_id)
        .bind(entry.event_type)
        .bind(entry.actor_id)
        .bind(entry.actor_type)
        .bind(entry.old_value)
        .bind(entry.new_value)
        .bind(entry.metadata)
        .fetch_one(&self.pool)
        .await?;

        Ok(result)
    }

    /// Find audit entries by hospital ID
    pub async fn find_by_hospital_id(
        &self,
        hospital_id: Uuid,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, AuditError> {
        let entries = sqlx::query_as::<_, AuditEntry>(
            r#"
            SELECT 
                id, hospital_id, event_type, actor_id, actor_type,
                old_value, new_value, metadata, created_at
            FROM hospital_registration_audit
            WHERE hospital_id = $1
            ORDER BY created_at DESC
            LIMIT $2
            "#,
        )
        .bind(hospital_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(entries)
    }

    /// Find audit entries by event type within a date range
    pub async fn find_by_event_type(
        &self,
        event_type: AuditEventType,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
    ) -> Result<Vec<AuditEntry>, AuditError> {
        let entries = sqlx::query_as::<_, AuditEntry>(
            r#"
            SELECT 
                id, hospital_id, event_type, actor_id, actor_type,
                old_value, new_value, metadata, created_at
            FROM hospital_registration_audit
            WHERE event_type = $1
              AND created_at BETWEEN $2 AND $3
            ORDER BY created_at DESC
            "#,
        )
        .bind(event_type)
        .bind(start_date)
        .bind(end_date)
        .fetch_all(&self.pool)
        .await?;

        Ok(entries)
    }
}

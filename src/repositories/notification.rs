use sqlx::PgPool;
use uuid::Uuid;

use crate::models::notification::{DevicePlatform, Notification};

/// A user's active push token, returned when dispatching a notification.
#[derive(Debug, Clone)]
pub struct ActiveToken {
    pub id: Uuid,
    pub token: String,
}

/// Persistence for device push tokens and the notification center.
pub struct NotificationRepository {
    pool: PgPool,
}

impl NotificationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Register (or re-activate) a device token for a user. Idempotent on
    /// `(user_id, token)`; a previously revoked token is restored.
    pub async fn register_device(
        &self,
        user_id: Uuid,
        platform: DevicePlatform,
        token: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO device_tokens (user_id, platform, token)
            VALUES ($1, $2, $3)
            ON CONFLICT (user_id, token) DO UPDATE
                SET platform     = EXCLUDED.platform,
                    last_seen_at = NOW(),
                    revoked_at   = NULL
            "#,
        )
        .bind(user_id)
        .bind(platform)
        .bind(token)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Revoke a device token (e.g. on logout). No-op if it does not exist.
    pub async fn revoke_device(&self, user_id: Uuid, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE device_tokens
               SET revoked_at = NOW()
             WHERE user_id = $1 AND token = $2 AND revoked_at IS NULL
            "#,
        )
        .bind(user_id)
        .bind(token)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Active (non-revoked) tokens for a user.
    pub async fn active_tokens(&self, user_id: Uuid) -> Result<Vec<ActiveToken>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (Uuid, String)>(
            r#"
            SELECT id, token
            FROM device_tokens
            WHERE user_id = $1 AND revoked_at IS NULL
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, token)| ActiveToken { id, token })
            .collect())
    }

    /// Mark a token revoked by id — used to prune tokens FCM reports invalid.
    pub async fn revoke_token_by_id(&self, token_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE device_tokens SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL"#,
        )
        .bind(token_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Persist a notification-center entry. Returns the new id.
    pub async fn insert_notification(
        &self,
        user_id: Uuid,
        kind: &str,
        title: &str,
        body: &str,
        data: &serde_json::Value,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO notifications (user_id, kind, title, body, data)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id
            "#,
        )
        .bind(user_id)
        .bind(kind)
        .bind(title)
        .bind(body)
        .bind(data)
        .fetch_one(&self.pool)
        .await
    }

    /// List a user's notifications, newest first, optionally unread-only.
    pub async fn list_notifications(
        &self,
        user_id: Uuid,
        unread_only: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Notification>, sqlx::Error> {
        sqlx::query_as::<_, Notification>(
            r#"
            SELECT id, user_id, kind, title, body, data, read_at, created_at
            FROM notifications
            WHERE user_id = $1
              AND ($2 = FALSE OR read_at IS NULL)
            ORDER BY created_at DESC
            LIMIT $3 OFFSET $4
            "#,
        )
        .bind(user_id)
        .bind(unread_only)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    /// Count unread notifications for the badge.
    pub async fn unread_count(&self, user_id: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND read_at IS NULL"#,
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Mark one notification read. Returns the number of rows updated (0 when it
    /// does not exist or is not owned by the user).
    pub async fn mark_read(&self, user_id: Uuid, notification_id: Uuid) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE notifications
               SET read_at = NOW()
             WHERE id = $1 AND user_id = $2 AND read_at IS NULL
            "#,
        )
        .bind(notification_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

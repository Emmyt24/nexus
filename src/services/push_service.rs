//! Push + notification-center service (E4).
//!
//! Owns device-token registration, the notification center, and dispatch of a
//! notification to a user's devices via FCM. Dispatch is best-effort: a
//! notification row is always persisted so it appears in the in-app center even
//! when push delivery fails or no device is registered.

use std::sync::Arc;
use uuid::Uuid;

use crate::models::notification::{DevicePlatform, NotificationPage};
use crate::repositories::notification::NotificationRepository;
use crate::services::fcm::{FcmClient, PushOutcome};

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

pub struct PushService {
    repo: Arc<NotificationRepository>,
    fcm: Arc<FcmClient>,
}

impl PushService {
    pub fn new(repo: Arc<NotificationRepository>, fcm: Arc<FcmClient>) -> Self {
        Self { repo, fcm }
    }

    pub async fn register_device(
        &self,
        user_id: Uuid,
        platform: DevicePlatform,
        token: &str,
    ) -> Result<(), PushError> {
        self.repo.register_device(user_id, platform, token).await?;
        Ok(())
    }

    pub async fn revoke_device(&self, user_id: Uuid, token: &str) -> Result<(), PushError> {
        self.repo.revoke_device(user_id, token).await?;
        Ok(())
    }

    pub async fn list_notifications(
        &self,
        user_id: Uuid,
        unread_only: bool,
        limit: i64,
        offset: i64,
    ) -> Result<NotificationPage, PushError> {
        let notifications = self
            .repo
            .list_notifications(user_id, unread_only, limit, offset)
            .await?;
        let unread_count = self.repo.unread_count(user_id).await?;
        Ok(NotificationPage {
            notifications,
            unread_count,
        })
    }

    /// Mark a notification read. Returns `false` when it does not exist or is
    /// not owned by the user.
    pub async fn mark_read(
        &self,
        user_id: Uuid,
        notification_id: Uuid,
    ) -> Result<bool, PushError> {
        Ok(self.repo.mark_read(user_id, notification_id).await? > 0)
    }

    /// Persist a notification and push it to the user's devices. Best-effort:
    /// the notification row is committed first; push failures are logged and
    /// invalid tokens pruned, but never surfaced as an error to the caller.
    pub async fn notify(
        &self,
        user_id: Uuid,
        kind: &str,
        title: &str,
        body: &str,
        data: serde_json::Value,
    ) -> Result<Uuid, PushError> {
        let id = self
            .repo
            .insert_notification(user_id, kind, title, body, &data)
            .await?;

        match self.repo.active_tokens(user_id).await {
            Ok(tokens) => {
                for t in tokens {
                    match self.fcm.send(&t.token, title, body, &data).await {
                        Ok(PushOutcome::Delivered) => {}
                        Ok(PushOutcome::InvalidToken) => {
                            if let Err(e) = self.repo.revoke_token_by_id(t.id).await {
                                tracing::warn!("failed to prune invalid device token: {e}");
                            }
                        }
                        Err(e) => tracing::warn!("push dispatch failed: {e}"),
                    }
                }
            }
            Err(e) => tracing::warn!("could not load device tokens for {user_id}: {e}"),
        }

        Ok(id)
    }

    /// Fire-and-forget variant for call sites that must not block on delivery.
    /// Errors are logged, never returned.
    pub async fn notify_best_effort(
        &self,
        user_id: Uuid,
        kind: &str,
        title: &str,
        body: &str,
        data: serde_json::Value,
    ) {
        if let Err(e) = self.notify(user_id, kind, title, body, data).await {
            tracing::warn!("notify_best_effort failed for {user_id} ({kind}): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::notification::Notification;

    // Pure shape check: an unread page reports its notifications and count.
    #[test]
    fn notification_page_shape() {
        let page = NotificationPage {
            notifications: Vec::<Notification>::new(),
            unread_count: 0,
        };
        assert_eq!(page.unread_count, 0);
        assert!(page.notifications.is_empty());
    }
}

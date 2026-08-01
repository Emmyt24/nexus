use chrono::{Duration, Utc};
use std::sync::Arc;
use uuid::Uuid;

use crate::repositories::email_outbox::EmailOutboxRepository;
use crate::services::email_templates::EmailContent;
use crate::services::notification_service::{NotificationError, NotificationService};

#[derive(Debug, thiserror::Error)]
pub enum EmailOutboxError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Notification error: {0}")]
    Notification(#[from] NotificationError),
}

pub struct EmailOutboxService {
    repo: Arc<EmailOutboxRepository>,
    notification: Arc<NotificationService>,
}

impl EmailOutboxService {
    pub fn new(repo: Arc<EmailOutboxRepository>, notification: Arc<NotificationService>) -> Self {
        Self { repo, notification }
    }

    pub async fn enqueue_email(
        &self,
        to_email: &str,
        content: &EmailContent,
    ) -> Result<Uuid, EmailOutboxError> {
        self.repo
            .enqueue(
                to_email,
                &content.subject,
                &content.text_body,
                Some(&content.html_body),
            )
            .await
            .map_err(EmailOutboxError::from)
    }

    pub async fn process_pending_batch(
        &self,
        batch_size: i64,
        max_attempts: i32,
        retry_base_minutes: i64,
    ) -> Result<usize, EmailOutboxError> {
        let pending = self.repo.fetch_pending(batch_size).await?;
        let mut processed = 0usize;

        for item in pending {
            let attempts = self.repo.mark_processing(item.id).await?;
            let send_result = self
                .notification
                .send_email_message(
                    &item.to_email,
                    &item.subject,
                    &item.text_body,
                    item.html_body.as_deref(),
                )
                .await;

            match send_result {
                Ok(()) => {
                    self.repo.mark_sent(item.id).await?;
                    processed += 1;
                }
                Err(err) => {
                    tracing::error!(
                        "Email send failed for {} (attempt {}): {}",
                        item.to_email,
                        attempts,
                        err
                    );
                    if attempts >= max_attempts {
                        self.repo.mark_failed(item.id, &err.to_string()).await?;
                    } else {
                        let delay_minutes = retry_base_minutes * attempts as i64;
                        let retry_at = Utc::now() + Duration::minutes(delay_minutes);
                        self.repo
                            .reschedule(item.id, &err.to_string(), retry_at)
                            .await?;
                    }
                }
            }
        }

        Ok(processed)
    }
}

pub struct EmailOutboxWorker {
    service: Arc<EmailOutboxService>,
    batch_size: i64,
    max_attempts: i32,
    retry_base_minutes: i64,
    poll_secs: u64,
}

impl EmailOutboxWorker {
    pub fn new(service: Arc<EmailOutboxService>) -> Self {
        let batch_size = std::env::var("EMAIL_OUTBOX_BATCH_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);

        let max_attempts = std::env::var("EMAIL_OUTBOX_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let retry_base_minutes = std::env::var("EMAIL_OUTBOX_RETRY_BASE_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let poll_secs = std::env::var("EMAIL_OUTBOX_POLL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        Self {
            service,
            batch_size,
            max_attempts,
            retry_base_minutes,
            poll_secs,
        }
    }

    pub async fn run(self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(self.poll_secs));
        loop {
            interval.tick().await;
            match self
                .service
                .process_pending_batch(self.batch_size, self.max_attempts, self.retry_base_minutes)
                .await
            {
                Ok(count) if count > 0 => {
                    tracing::info!("Email outbox processed {} messages", count);
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::error!("Email outbox processing failed: {}", err);
                }
            }
        }
    }
}

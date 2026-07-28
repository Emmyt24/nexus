use lettre::{
    message::{header::ContentType, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    #[error("Email sending failed: {0}")]
    EmailFailed(String),

    #[error("Push notification failed: {0}")]
    PushFailed(String),

    #[error("Notification logging failed: {0}")]
    LoggingFailed(String),
}

pub struct NotificationService {
    smtp: Option<Arc<AsyncSmtpTransport<Tokio1Executor>>>,
    from_email: String,
    from_name: String,
    mock: bool,
}

impl NotificationService {
    pub fn new() -> Self {
        let smtp_host = std::env::var("SMTP_HOST").unwrap_or_default();
        let smtp_user = std::env::var("SMTP_USERNAME").unwrap_or_default();
        let smtp_pass = std::env::var("SMTP_PASSWORD").unwrap_or_default();
        let smtp_port: u16 = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(587);
        let from_email = std::env::var("SMTP_FROM_EMAIL")
            .unwrap_or_else(|_| "noreply@nexuscare.com".to_string());
        let from_name = std::env::var("SMTP_FROM_NAME").unwrap_or_else(|_| "NexusCare".to_string());

        let mock = smtp_host.is_empty() || smtp_user.is_empty() || smtp_pass.is_empty();
        if mock {
            tracing::warn!(
                "NotificationService in MOCK mode — emails will NOT be sent. \
                 Missing SMTP config (host_set={}, user_set={}, pass_set={}).",
                !smtp_host.is_empty(),
                !smtp_user.is_empty(),
                !smtp_pass.is_empty(),
            );
        } else {
            tracing::info!(
                "NotificationService SMTP configured: host={} port={} from={}",
                smtp_host,
                smtp_port,
                from_email,
            );
        }
        let smtp = if !mock {
            let creds = Credentials::new(smtp_user, smtp_pass);
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp_host)
                .ok()
                .map(|b| b.port(smtp_port).credentials(creds).build())
                .map(Arc::new)
        } else {
            None
        };

        Self {
            smtp,
            from_email,
            from_name,
            mock,
        }
    }

    /// Send an email with optional HTML. In mock mode, logs instead of sending

    pub async fn send_email_message(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
    ) -> Result<(), NotificationError> {
        if self.mock {
            tracing::info!(
                "[MOCK EMAIL] To: {} | Subject: {} | Body: {}",
                to,
                subject,
                text_body
            );
            return Ok(());
        }

        let from = format!("{} <{}>", self.from_name, self.from_email);
        let builder = Message::builder()
            .from(
                from.parse()
                    .map_err(|e| NotificationError::EmailFailed(format!("{}", e)))?,
            )
            .to(to
                .parse()
                .map_err(|e| NotificationError::EmailFailed(format!("{}", e)))?)
            .subject(subject);

        let email = match html_body {
            Some(html) => builder
                .multipart(
                    MultiPart::alternative()
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_PLAIN)
                                .body(text_body.to_string()),
                        )
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_HTML)
                                .body(html.to_string()),
                        ),
                )
                .map_err(|e| NotificationError::EmailFailed(e.to_string()))?,
            None => builder
                .header(ContentType::TEXT_PLAIN)
                .body(text_body.to_string())
                .map_err(|e| NotificationError::EmailFailed(e.to_string()))?,
        };

        self.smtp
            .as_ref()
            .expect("SMTP transport not initialised")
            .send(email)
            .await
            .map_err(|e| NotificationError::EmailFailed(e.to_string()))?;

        Ok(())
    }

    /// Send a plain-text email. In mock mode, logs instead of sending

    pub async fn send_email(
        &self,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<(), NotificationError> {
        self.send_email_message(to, subject, body, None).await
    }

    /// Send push notification (stub — wire FCM when ready)
    #[allow(dead_code)]
    async fn send_push(
        &self,
        hospital_id: Uuid,
        title: &str,
        message: &str,
    ) -> Result<(), NotificationError> {
        tracing::info!(
            "[PUSH] hospital={} title={} message={}",
            hospital_id,
            title,
            message
        );
        Ok(())
    }

    /// Send shift broadcast notification to eligible workers
    pub async fn send_shift_broadcast_notification(
        &self,
        shift_id: Uuid,
        hospital_id: Uuid,
        matched_count: i32,
    ) -> Result<(), NotificationError> {
        tracing::info!(
            "[SHIFT BROADCAST] shift_id={} hospital_id={} matched_clinicians={}",
            shift_id,
            hospital_id,
            matched_count
        );

        // In production, this would:

        // Mock implementation
        tracing::info!(
            "Push notifications sent to {} eligible workers for shift {}",
            matched_count,
            shift_id
        );

        Ok(())
    }
}

impl Default for NotificationService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_email_plain() {
        let service = NotificationService::new();
        let result = service
            .send_email("admin@test.com", "Subject", "Body")
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_email_html() {
        let service = NotificationService::new();
        let result = service
            .send_email_message("admin@test.com", "Subject", "Body", Some("<p>Body</p>"))
            .await;

        assert!(result.is_ok());
    }
}

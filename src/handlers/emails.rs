//! Generic transactional-email relay. The frontend renders its own template
//! (subject + HTML/text) and posts it here; the backend just enqueues it onto
//! the same outbox → SMTP pipeline used by the built-in notifications.
//!
//! Requires a valid bearer token so it can't be used as an open mail relay.

use axum::{extract::State, http::HeaderMap, Json};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use validator::Validate;

use crate::routes::AppState;
use crate::services::email_templates::EmailContent;
use crate::utils::errors::{AppError, AppResult};
use crate::utils::extract_claims;

#[derive(Debug, Deserialize, ToSchema, Validate)]
pub struct SendEmailRequest {
    /// Recipient email address.
    #[validate(email(message = "A valid recipient email is required"))]
    pub to: String,
    /// Email subject line.
    #[validate(length(min = 1, max = 255, message = "subject is required"))]
    pub subject: String,
    /// Rendered HTML body (frontend-owned template). Optional if `text` is set.
    pub html: Option<String>,
    /// Plain-text body. Optional if `html` is set.
    pub text: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SendEmailResponse {
    /// Outbox row id; the worker delivers it via SMTP shortly after.
    pub queued_id: String,
    pub message: String,
}

/// POST /api/v1/emails/send
#[utoipa::path(
    post,
    path = "/api/v1/emails/send",
    request_body = SendEmailRequest,
    responses(
        (status = 202, description = "Email queued for delivery", body = SendEmailResponse),
        (status = 401, description = "Missing or invalid token"),
        (status = 422, description = "Validation error")
    ),
    tag = "emails",
    summary = "Send a frontend-templated email",
    description = "Queues an email whose subject/body are supplied by the caller (frontend-owned template). Delivered via the shared outbox → SMTP pipeline."
)]
pub async fn send_email(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SendEmailRequest>,
) -> AppResult<(axum::http::StatusCode, Json<SendEmailResponse>)> {
    // Authenticated callers only — prevents an open spam relay.
    let _claims = extract_claims(&headers)?;

    req.validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    if req.html.is_none() && req.text.is_none() {
        return Err(AppError::Validation(
            "either `html` or `text` body is required".to_string(),
        ));
    }

    // EmailContent needs both bodies; derive whichever the caller omitted.
    let html_body = req.html.clone().unwrap_or_else(|| {
        // Minimal HTML wrapper around the plain text.
        format!("<pre>{}</pre>", req.text.clone().unwrap_or_default())
    });
    let text_body = req.text.clone().unwrap_or_else(|| req.html.clone().unwrap_or_default());

    let content = EmailContent {
        subject: req.subject,
        text_body,
        html_body,
    };

    let queued_id = state
        .email_outbox
        .enqueue_email(&req.to, &content)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to queue email: {e}")))?;

    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(SendEmailResponse {
            queued_id: queued_id.to_string(),
            message: "Email queued for delivery".to_string(),
        }),
    ))
}

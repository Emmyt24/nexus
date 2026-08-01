//! Device-token registration and the notification center (E4).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::models::notification::{NotificationPage, RegisterDeviceRequest, RevokeDeviceRequest};
use crate::routes::AppState;
use crate::services::push_service::PushError;
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

const NOTIF_DEFAULT_LIMIT: i64 = 50;
const NOTIF_MAX_LIMIT: i64 = 100;

fn map_push_error(e: PushError) -> AppError {
    match e {
        PushError::Database(e) => AppError::Database(e),
    }
}

fn caller_id(headers: &HeaderMap) -> Result<Uuid, AppError> {
    let claims = extract_claims(headers)?;
    Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))
}

/// POST /api/v1/devices/token
#[utoipa::path(
    post,
    path = "/api/v1/devices/token",
    request_body = RegisterDeviceRequest,
    responses(
        (status = 204, description = "Device token registered"),
        (status = 400, description = "Empty token", body = ErrorResponse),
        (status = 401, body = ErrorResponse)
    ),
    tag = "notifications",
    summary = "Register a device push token",
    description = "Register or refresh the caller's FCM device token so they can receive push notifications. Idempotent per (user, token)."
)]
pub async fn register_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RegisterDeviceRequest>,
) -> AppResult<StatusCode> {
    let user_id = caller_id(&headers)?;
    if payload.token.trim().is_empty() {
        return Err(AppError::BadRequest("token must not be empty".to_string()));
    }
    state
        .push_service
        .register_device(user_id, payload.platform, payload.token.trim())
        .await
        .map_err(map_push_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/v1/devices/token
#[utoipa::path(
    delete,
    path = "/api/v1/devices/token",
    request_body = RevokeDeviceRequest,
    responses(
        (status = 204, description = "Device token revoked"),
        (status = 401, body = ErrorResponse)
    ),
    tag = "notifications",
    summary = "Revoke a device push token",
    description = "Revoke the caller's device token, e.g. on logout. No-op if the token is unknown."
)]
pub async fn revoke_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RevokeDeviceRequest>,
) -> AppResult<StatusCode> {
    let user_id = caller_id(&headers)?;
    state
        .push_service
        .revoke_device(user_id, payload.token.trim())
        .await
        .map_err(map_push_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct ListNotificationsQuery {
    /// Return only unread notifications when true.
    pub unread: Option<bool>,
    /// Page size (default 50, max 100).
    pub limit: Option<i64>,
    /// Rows to skip (default 0).
    pub offset: Option<i64>,
}

/// GET /api/v1/notifications
#[utoipa::path(
    get,
    path = "/api/v1/notifications",
    params(ListNotificationsQuery),
    responses(
        (status = 200, description = "Notification center page", body = NotificationPage),
        (status = 401, body = ErrorResponse)
    ),
    tag = "notifications",
    summary = "List notifications",
    description = "List the caller's notifications, newest first, with the unread badge count. Optionally filter to unread only."
)]
pub async fn list_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListNotificationsQuery>,
) -> AppResult<Json<NotificationPage>> {
    let user_id = caller_id(&headers)?;
    let limit = query.limit.unwrap_or(NOTIF_DEFAULT_LIMIT).clamp(1, NOTIF_MAX_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let page = state
        .push_service
        .list_notifications(user_id, query.unread.unwrap_or(false), limit, offset)
        .await
        .map_err(map_push_error)?;
    Ok(Json(page))
}

/// POST /api/v1/notifications/{notification_id}/read
#[utoipa::path(
    post,
    path = "/api/v1/notifications/{notification_id}/read",
    params(("notification_id" = Uuid, Path, description = "Notification id")),
    responses(
        (status = 204, description = "Notification marked read"),
        (status = 401, body = ErrorResponse),
        (status = 404, description = "Notification not found", body = ErrorResponse)
    ),
    tag = "notifications",
    summary = "Mark a notification read",
    description = "Mark one of the caller's notifications as read."
)]
pub async fn mark_notification_read(
    State(state): State<AppState>,
    Path(notification_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let user_id = caller_id(&headers)?;
    let updated = state
        .push_service
        .mark_read(user_id, notification_id)
        .await
        .map_err(map_push_error)?;
    if updated {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("Notification not found".to_string()))
    }
}

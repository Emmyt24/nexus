use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// Platform a device push token belongs to. iOS is delivered via FCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "device_platform", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DevicePlatform {
    Ios,
    Android,
    Web,
}

/// Request body for `POST /api/v1/devices/token`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RegisterDeviceRequest {
    pub platform: DevicePlatform,
    /// The FCM registration token from the device.
    pub token: String,
}

/// Request body for `DELETE /api/v1/devices/token`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RevokeDeviceRequest {
    pub token: String,
}

/// A single notification-center entry.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Notification {
    pub id: Uuid,
    pub user_id: Uuid,
    /// Stable event key, e.g. `shift_offered`.
    pub kind: String,
    pub title: String,
    pub body: String,
    /// Structured payload for deep-linking (e.g. `{ "shift_id": "..." }`).
    pub data: serde_json::Value,
    pub read_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Paged notification list returned by `GET /api/v1/notifications`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotificationPage {
    pub notifications: Vec<Notification>,
    /// Count of unread notifications for the user (badge count).
    pub unread_count: i64,
}

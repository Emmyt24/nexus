// ! LiveKit video consultation domain types.
// !
// ! `video_sessions.status` and `video_session_participants.participant_role`
// ! are TEXT + CHECK columns rather than Postgres enums (see
// ! `migrations/20240057_video_consultations.sql`), so the enums below map to
// ! TEXT and every new value stays a plain `ALTER TABLE`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

// Enums

/// Lifecycle of a consultation room. Status only ever moves forward:
/// `pending -> active -> ended`, with `failed` reserved for a room we could
/// never bring up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum VideoSessionStatus {
    /// Row exists and a token has been minted, but nobody has joined yet.
    Pending,
    /// LiveKit has reported at least one participant in the room.
    Active,
    /// The room is over — `room_finished`, ended by the hospital, or reconciled.
    Ended,
    /// Reserved: the room could not be brought up.
    Failed,
}

impl VideoSessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoSessionStatus::Pending => "pending",
            VideoSessionStatus::Active => "active",
            VideoSessionStatus::Ended => "ended",
            VideoSessionStatus::Failed => "failed",
        }
    }
}

/// Who a participant is in the room. `Patient` and `Agent` are seams for
/// ad-hoc consults and AI agents — nothing mints tokens for them today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ParticipantRole {
    /// The clinician assigned to the shift. Joining clocks them in.
    Clinician,
    /// An admin of the shift's own hospital.
    HospitalObserver,
    /// Future scope: a patient joining through a guest link.
    Patient,
    /// Future scope: an automated participant.
    Agent,
}

impl ParticipantRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParticipantRole::Clinician => "clinician",
            ParticipantRole::HospitalObserver => "hospital_observer",
            ParticipantRole::Patient => "patient",
            ParticipantRole::Agent => "agent",
        }
    }
}

/// How the caller wants to join. `Observer` is hospital-admin only; a worker
/// asking for it gets a 403.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    /// Camera and microphone on — the default.
    #[default]
    Participant,
    /// Watch-only: subscribe but never publish.
    Observer,
}

impl JoinMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            JoinMode::Participant => "participant",
            JoinMode::Observer => "observer",
        }
    }
}

// Rows

/// One LiveKit room. `shift_id` is NULL for a future ad-hoc consult.
#[derive(Debug, Clone, FromRow)]
pub struct VideoSession {
    pub id: Uuid,
    pub shift_id: Option<Uuid>,
    pub hospital_id: Uuid,
    pub created_by: Option<Uuid>,

    pub room_name: String,
    pub livekit_room_sid: Option<String>,
    pub status: VideoSessionStatus,

    pub max_participants: i32,
    pub departure_timeout_s: i32,
    pub empty_timeout_s: i32,

    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub ended_reason: Option<String>,

    /// Recording seam — always FALSE / NULL in this release.
    pub recording_enabled: bool,
    pub recording_consent: Option<serde_json::Value>,

    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One `(session, LiveKit identity)` pair. Created when a token is minted, then
/// updated by the `participant_joined` / `participant_left` webhooks.
#[derive(Debug, Clone, FromRow)]
pub struct VideoSessionParticipant {
    pub id: Uuid,
    pub session_id: Uuid,
    pub identity: String,
    pub user_id: Option<Uuid>,
    pub clinician_id: Option<Uuid>,
    pub display_name: Option<String>,

    pub participant_role: ParticipantRole,
    pub can_publish: bool,

    pub token_issued_at: DateTime<Utc>,
    pub token_expires_at: DateTime<Utc>,
    pub token_issue_count: i32,

    pub participant_sid: Option<String>,
    pub joined_at: Option<DateTime<Utc>>,
    pub left_at: Option<DateTime<Utc>>,
    pub disconnect_reason: Option<String>,
    /// Set by `claim_clockin_slot`; the row lock behind it is what makes two
    /// concurrent `participant_joined` deliveries produce one clock-in.
    pub clocked_in_at: Option<DateTime<Utc>>,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One row of the reconciler's post-consult clock-out sweep: a session that has
/// ended while its shift is still `in_progress` with an open attendance row.
#[derive(Debug, Clone, FromRow)]
pub struct PendingClockout {
    pub session_id: Uuid,
    pub room_name: String,
    pub shift_id: Uuid,
    pub clinician_id: Uuid,
}

/// An append-only audit line. `occurred_at` is LiveKit's clock for webhook rows
/// and ours for operator actions.
#[derive(Debug, Clone)]
pub struct NewVideoSessionEvent {
    pub session_id: Option<Uuid>,
    pub room_name: String,
    pub event_type: String,
    pub identity: Option<String>,
    pub actor_user_id: Option<Uuid>,
    pub livekit_event_id: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub occurred_at: DateTime<Utc>,
}

// DTOs

/// `POST /api/v1/shifts/{shift_id}/consult/token`. Every field is optional —
/// send `{}` if you have nothing to say.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct JoinConsultRequest {
    /// `"participant"` (default) or `"observer"` (hospital admin only).
    #[serde(default)]
    pub mode: Option<JoinMode>,
    /// Free text, stored in the audit trail only.
    #[serde(default)]
    pub device_label: Option<String>,
}

/// The shift a consultation belongs to, denormalised for the join screen.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConsultShiftSummary {
    pub id: Uuid,
    pub role_title: String,
    pub hospital_name: Option<String>,
    pub scheduled_start: DateTime<Utc>,
    pub scheduled_end: DateTime<Utc>,
    pub status: String,
    pub shift_type: String,
}

/// Clock-in state, so the client knows whether to show "clocking you in…" and
/// where the manual fallback lives.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConsultClockInView {
    /// `"auto_on_join"` when the backend clocks in from the webhook,
    /// `"manual"` when `LIVEKIT_VIRTUAL_CLOCKIN_ENABLED` is off.
    pub mode: String,
    pub already_clocked_in: bool,
    pub clocked_in_at: Option<DateTime<Utc>>,
    /// The permanent fallback: `POST` it with `{"method":"virtual"}`.
    pub fallback_endpoint: String,
}

/// Recording state. Always `{ "enabled": false, "status": null }` in this
/// release; it ships now so the recording indicator has a stable shape.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConsultRecordingView {
    pub enabled: bool,
    pub status: Option<String>,
}

impl ConsultRecordingView {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            status: None,
        }
    }
}

/// `200` from the token endpoint. Hand `url` + `token` straight to the LiveKit
/// client SDK; everything else is UI state.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct JoinConsultResponse {
    pub session_id: Uuid,
    pub room_name: String,
    /// `wss://…` — passed verbatim to `room.connect(url, token)`.
    pub url: String,
    pub token: String,
    pub identity: String,
    pub display_name: String,
    pub participant_role: ParticipantRole,
    pub mode: JoinMode,
    pub can_publish: bool,
    pub can_subscribe: bool,
    /// A join deadline, not a call deadline: LiveKit validates the token only
    /// at `connect()`. Call the endpoint again if the user sits on the pre-join
    /// screen past it.
    pub expires_at: DateTime<Utc>,
    pub session_status: VideoSessionStatus,
    pub shift: ConsultShiftSummary,
    pub clock_in: ConsultClockInView,
    pub recording: ConsultRecordingView,
    /// `true` when the backend has no LiveKit credentials — the token is fake.
    pub mock: bool,
}

/// One participant on `GET /consult`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConsultParticipantView {
    pub identity: String,
    pub display_name: Option<String>,
    pub participant_role: ParticipantRole,
    pub connected: bool,
    pub joined_at: Option<DateTime<Utc>>,
    pub left_at: Option<DateTime<Utc>>,
    pub is_publisher: bool,
    pub clocked_in_at: Option<DateTime<Utc>>,
}

/// `200` from `GET /api/v1/shifts/{shift_id}/consult`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConsultSessionView {
    pub session_id: Uuid,
    pub shift_id: Option<Uuid>,
    pub room_name: String,
    pub status: VideoSessionStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub ended_reason: Option<String>,
    /// `true` when the participant list was reconciled against LiveKit on this
    /// request; `false` means LiveKit was unreachable and this is the DB's last
    /// known webhook-fed state.
    pub live: bool,
    pub clock_in_recorded: bool,
    pub participants: Vec<ConsultParticipantView>,
    pub recording: ConsultRecordingView,
}

/// `200` from `POST /consult/leave`. Idempotent; does not end the call for
/// anyone else and does not clock the worker out.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LeaveConsultResponse {
    pub session_id: Uuid,
    pub identity: String,
    pub left_at: DateTime<Utc>,
    pub session_status: VideoSessionStatus,
    pub remaining_participants: i64,
}

/// `POST /consult/end` — the reason is optional and audit-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct EndConsultRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// `200` from `POST /consult/end`. Ending the room never clocks the worker out.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EndConsultResponse {
    pub session_id: Uuid,
    pub status: VideoSessionStatus,
    pub ended_at: Option<DateTime<Utc>>,
    pub ended_reason: Option<String>,
    pub clock_out_required: bool,
    pub clock_out_hint: String,
}

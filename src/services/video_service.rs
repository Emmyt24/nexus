// ! Business rules for LiveKit video consultations: who may join a shift's
// ! room, what grants they get, what a webhook delivery means, and the sweep
// ! that recovers from a lost one.
// !
// ! Two invariants are load-bearing and easy to break:
// !
// ! 1. **No LiveKit call ever happens inside a database transaction.** Rooms
// !    are created lazily at token-issue time, outside `begin()`, and a failed
// !    `ensure_room` is logged rather than propagated — LiveKit auto-creates
// !    rooms on first join, so the only cost is losing `max_participants`.
// ! 2. **Leaving a room never clocks anybody out.** `participant_left` fires on
// !    every transient disconnect, and `record_clockout_tx` flips the shift to
// !    `completed`, which is what the payout scheduler keys off. Clock-out is
// !    automated only by the reconciler.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::models::patient_record::{
    ConsultQueuePatientView, ConsultWaitingRoomView, CreateHandoffRequest, CreateHandoffResponse,
};
use crate::models::shift::{Shift, ShiftStatus, ShiftType};
use crate::models::user::{Claims, UserRole};
use crate::models::video_session::{
    ConsultClockInView, ConsultParticipantView, ConsultRecordingView, ConsultSessionView,
    ConsultShiftSummary, EndConsultResponse, JoinConsultRequest, JoinConsultResponse, JoinMode,
    LeaveConsultResponse, NewVideoSessionEvent, ParticipantRole, VideoSession,
    VideoSessionParticipant, VideoSessionStatus,
};
use crate::repositories::patient_record::PatientRecordRepository;
use crate::repositories::shift::ShiftRepository;
use crate::repositories::video_session::VideoSessionRepository;
use crate::services::livekit::{grants_for, LiveKitClient, LiveKitError, LiveKitWebhookEvent};
use crate::services::push_service::PushService;
use crate::services::shift_service::{ShiftService, VirtualClockinOutcome};

/// A consultation may be joined from an hour before the shift starts to an hour
/// after it ends. The lower bound is not arbitrary: `clock_in` refuses anything
/// outside ±60 minutes of `scheduled_start`, so a token minted earlier would
/// guarantee a `participant_joined` webhook that cannot clock anyone in.
const JOIN_WINDOW_MINUTES: i64 = 60;

/// How long after a token is minted we assume the `participant_joined` webhook
/// is never coming.
const RECONCILE_JOIN_GRACE_MINUTES: i64 = 5;
/// How long an `active` session may sit untouched before we ask LiveKit whether
/// the room still exists.
const RECONCILE_STALE_MINUTES: i64 = 30;
/// How long after a room ends we chase the worker's clock-out.
const RECONCILE_CLOCKOUT_DELAY_MINUTES: i64 = 10;

const ROOM_NAME_PREFIX: &str = "shift-";

/// A clinician must be within this many km of the hospital to join the call.
const CALL_GEOFENCE_KM: f64 = 10.0;
const HANDOVER_REMINDER_EVENT: &str = "handover_reminder_sent";
/// How long a "Continue on phone" code stays redeemable. Short on purpose: the
/// user is looking at the QR code while they scan it, and the code is the whole
/// credential.
const HANDOFF_TTL_SECONDS: i64 = 180;

#[derive(Debug, thiserror::Error)]
pub enum VideoServiceError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("LiveKit error: {0}")]
    LiveKit(#[from] LiveKitError),

    #[error("Shift not found: {0}")]
    ShiftNotFound(Uuid),

    #[error("No consultation session exists for this shift")]
    SessionNotFound,

    #[error("Video consultations are only available for virtual shifts")]
    NotVirtualShift,

    #[error("Not authorized to join this consultation")]
    NotAuthorized,

    #[error("Authenticated user has no clinician profile")]
    NoClinicianProfile,

    #[error("Shift cannot be joined: {0}")]
    ShiftNotJoinable(String),

    #[error("Outside the consultation window (one hour either side of the shift)")]
    OutsideWindow,

    #[error("This consultation has already ended")]
    SessionEnded,

    #[error("Your location is required to join this consultation")]
    LocationRequired,

    #[error("You are {distance_km:.1} km from the hospital — must be within {limit_km:.0} km to join")]
    OutsideGeofence { distance_km: f64, limit_km: f64 },

    #[error("LiveKit is not configured")]
    NotConfigured,
}

/// What a reader is entitled to see. Platform staff get counts and metadata so
/// support can work, but never clinical content or patient identities — the
/// same NDPR line `authorize_shift_access` draws for the video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClinicalAccess {
    /// The owning hospital, or the shift's assigned clinician.
    Full,
    /// `SuperAdmin` / `OperationsAdmin`.
    MetadataOnly,
}

impl ClinicalAccess {
    pub fn is_full(&self) -> bool {
        matches!(self, ClinicalAccess::Full)
    }
}

/// Who the caller is *relative to one shift*. The single tenant decision for
/// everything a shift owns — the room, the waiting room, and the clinical
/// record — so those three can never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftDataRole {
    /// The shift's assigned clinician, carrying their `clinicians.id`.
    Clinician(Uuid),
    /// An admin of the hospital that owns the shift.
    HospitalAdmin,
    /// `SuperAdmin` / `OperationsAdmin` — support, not care.
    PlatformAdmin,
}

impl ShiftDataRole {
    /// Platform staff may read counts and metadata, never clinical content.
    pub fn clinical_access(&self) -> ClinicalAccess {
        match self {
            ShiftDataRole::PlatformAdmin => ClinicalAccess::MetadataOnly,
            _ => ClinicalAccess::Full,
        }
    }
}

/// What a webhook delivery did. All four are 200s — LiveKit retries on non-2xx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookOutcome {
    Processed,
    /// Deduped by `webhook_events`.
    AlreadySeen,
    /// A known event we deliberately only audit (tracks, egress, ingress).
    Ignored,
    /// A room we never minted a token for.
    UnknownRoom,
}

impl WebhookOutcome {
    /// Echoed back to LiveKit and read in logs, so it is spelled out rather
    /// than derived from the variant name.
    pub fn as_str(&self) -> &'static str {
        match self {
            WebhookOutcome::Processed => "processed",
            WebhookOutcome::AlreadySeen => "already_seen",
            WebhookOutcome::Ignored => "ignored",
            WebhookOutcome::UnknownRoom => "unknown_room",
        }
    }
}

/// One reconciler tick, for the log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    pub joins_recovered: usize,
    pub sessions_ended: usize,
    pub clock_outs: usize,
    pub handover_reminders: usize,
}

impl ReconcileReport {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Deterministic, and seeded into `video_sessions.room_name` on first use.
/// Every later lookup goes through the stored column, so the scheme can change
/// and a room can be rotated after an abuse report.
pub fn room_name_for_shift(shift_id: Uuid) -> String {
    format!("{ROOM_NAME_PREFIX}{shift_id}")
}

/// Inverse of [`room_name_for_shift`], for webhook payloads that carry only a
/// room name. `None` for anything that is not one of our shift rooms.
pub fn shift_id_from_room_name(room_name: &str) -> Option<Uuid> {
    room_name
        .strip_prefix(ROOM_NAME_PREFIX)
        .and_then(|id| Uuid::parse_str(id).ok())
}

fn identity_for_user(user_id: Uuid) -> String {
    format!("u:{user_id}")
}

/// The LiveKit identity for one of a user's devices. Slot 1 is the primary
/// device and keeps the bare `"u:<uuid>"`, so every token minted before
/// companion devices existed still maps to the same row.
pub fn identity_for_device(user_id: Uuid, device_ordinal: i32) -> String {
    if device_ordinal <= 1 {
        identity_for_user(user_id)
    } else {
        format!("u:{user_id}#d{device_ordinal}")
    }
}

/// `"u:<uuid>#d2"` → `"u:<uuid>"`; anything else is returned unchanged.
///
/// Used to key the clock-in on the *person*. Claiming per device would let a
/// companion device take a second slot, and checking a sibling row with
/// `NOT EXISTS` instead would not be serialised by `claim_clockin_slot`'s row
/// lock — two devices joining at once would both see no sibling clocked in.
pub fn primary_identity(identity: &str) -> &str {
    identity.split_once('#').map_or(identity, |(base, _)| base)
}

/// 256 bits from the OS, URL-safe and unpadded so it survives being pasted
/// into a URL fragment or encoded as a QR code.
fn generate_handoff_code() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

/// Only this ever reaches the database. A plain SHA-256 is the right primitive:
/// unlike a password the input is 256 bits of uniform randomness, so there is
/// nothing to brute-force and no need for a slow KDF. Same reasoning as
/// `identity_number_hash` (20240039).
fn hash_handoff_code(code: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(code.as_bytes());
    hex::encode(hasher.finalize())
}

/// The second URL. Shaped like `consult_deep_link` — a link into OUR app, not a
/// LiveKit URL — with the code in the **fragment**, which browsers never send to
/// the server. So the credential stays out of our access logs, out of any
/// `Referer`, and out of the proxy chain; the page reads `location.hash` and
/// POSTs it to the redeem endpoint.
fn handoff_url(shift_id: Uuid, code: &str) -> String {
    let base = std::env::var("APP_PUBLIC_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://app.nexuscare.com".to_string());
    let base = base.trim_end_matches('/');
    format!("{base}/consults/{shift_id}/continue#c={code}")
}

pub struct VideoService {
    repo: Arc<VideoSessionRepository>,
    /// Read-only here: the waiting-room counts ride along on `GET /consult` so
    /// the client needs one request, not two. Every *mutation* of the queue
    /// lives in `PatientRecordService`.
    queue_repo: Arc<PatientRecordRepository>,
    shift_repo: Arc<ShiftRepository>,
    shift_service: Arc<ShiftService>,
    livekit: Arc<LiveKitClient>,
    push: Arc<PushService>,
    /// Kill switch for mapping `participant_joined` to a virtual clock-in.
    /// Ships off: the receiver records the audit trail first, and the flag is
    /// flipped once the events look right in production.
    virtual_clockin_enabled: bool,
}

impl VideoService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<VideoSessionRepository>,
        queue_repo: Arc<PatientRecordRepository>,
        shift_repo: Arc<ShiftRepository>,
        shift_service: Arc<ShiftService>,
        livekit: Arc<LiveKitClient>,
        push: Arc<PushService>,
    ) -> Self {
        let virtual_clockin_enabled = std::env::var("LIVEKIT_VIRTUAL_CLOCKIN_ENABLED")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        Self::with_virtual_clockin(
            repo,
            queue_repo,
            shift_repo,
            shift_service,
            livekit,
            push,
            virtual_clockin_enabled,
        )
    }

    /// Explicit-flag constructor, so tests do not have to mutate the process
    /// environment to exercise the clock-in branch.
    #[allow(clippy::too_many_arguments)]
    pub fn with_virtual_clockin(
        repo: Arc<VideoSessionRepository>,
        queue_repo: Arc<PatientRecordRepository>,
        shift_repo: Arc<ShiftRepository>,
        shift_service: Arc<ShiftService>,
        livekit: Arc<LiveKitClient>,
        push: Arc<PushService>,
        virtual_clockin_enabled: bool,
    ) -> Self {
        Self {
            repo,
            queue_repo,
            shift_repo,
            shift_service,
            livekit,
            push,
            virtual_clockin_enabled,
        }
    }

    pub fn virtual_clockin_enabled(&self) -> bool {
        self.virtual_clockin_enabled
    }

    // Token issuance

    /// Mint a join token for the caller. Idempotent and safe to call any number
    /// of times — the frontend is told to call it again if the user sits on the
    /// pre-join screen past `expires_at`.
    pub async fn issue_join_token(
        &self,
        shift_id: Uuid,
        claims: &Claims,
        request: JoinConsultRequest,
    ) -> Result<JoinConsultResponse, VideoServiceError> {
        let user_id = claims_user_id(claims)?;
        let shift = self.load_shift(shift_id).await?;
        let (role, clinician_id) = self.authorize_shift_access(&shift, claims).await?;

        let mode = request.mode.unwrap_or_default();
        // Observing is a hospital affordance; a worker asking for it is a 403
        // rather than a silent downgrade, so the client learns it was wrong.
        if mode == JoinMode::Observer && role != ParticipantRole::HospitalObserver {
            return Err(VideoServiceError::NotAuthorized);
        }

        // Geofence: a clinician must be within 10 km of the hospital to join.
        // Only enforced when the hospital has coordinates on file; observers and
        // hospitals whose location is unknown are exempt.
        if role == ParticipantRole::Clinician {
            if let Some((h_lat, h_lng)) =
                self.shift_repo.get_hospital_coordinates(shift.hospital_id).await?
            {
                let (lat, lng) = match (request.lat, request.lng) {
                    (Some(lat), Some(lng)) => (lat, lng),
                    _ => return Err(VideoServiceError::LocationRequired),
                };
                let distance_km = crate::utils::geo::haversine_km(h_lat, h_lng, lat, lng);
                if distance_km > CALL_GEOFENCE_KM {
                    return Err(VideoServiceError::OutsideGeofence {
                        distance_km,
                        limit_km: CALL_GEOFENCE_KM,
                    });
                }
            }
        }

        let session = self.ensure_session_for_shift(&shift, user_id).await?;
        if session.status == VideoSessionStatus::Ended {
            return Err(VideoServiceError::SessionEnded);
        }

        // Outside the transaction, and non-fatal: LiveKit creates the room on
        // first join anyway, so a hiccup here must never fail a join.
        let mut room_options = self.livekit.room_defaults();
        room_options.max_participants = session.max_participants.max(1) as u32;
        room_options.empty_timeout_s = session.empty_timeout_s.max(0) as u32;
        room_options.departure_timeout_s = session.departure_timeout_s.max(0) as u32;
        if let Err(e) = self.livekit.ensure_room(&session.room_name, room_options).await {
            tracing::warn!(
                "LiveKit ensure_room failed for {} — minting the token anyway: {e}",
                session.room_name
            );
        }

        let display_name = self.display_name_for(claims, clinician_id).await;
        let identity = identity_for_user(user_id);
        let grants = grants_for(role, &session.room_name, mode);
        let ttl = self.livekit.token_ttl();
        // Non-sensitive on purpose: attributes are visible to every other
        // participant and are a UI hint, never the authorization source.
        let attributes = HashMap::from([
            ("nx_role".to_string(), role.as_str().to_string()),
            ("nx_shift_id".to_string(), shift.id.to_string()),
            ("nx_session_id".to_string(), session.id.to_string()),
        ]);

        let minted = self
            .livekit
            .mint_token(&identity, &display_name, &attributes, &grants, ttl)?;

        let participant = self
            .repo
            .upsert_participant_on_token(
                session.id,
                &identity,
                Some(user_id),
                clinician_id,
                &display_name,
                role,
                grants.can_publish,
                minted.expires_at,
                // The primary device. Companion slots are allocated only by
                // `redeem_handoff`, never by a plain token request.
                1,
                request.device_label.as_deref(),
            )
            .await?;

        self.audit(
            Some(session.id),
            &session.room_name,
            "token_issued",
            Some(&identity),
            Some(user_id),
            None,
            Some(serde_json::json!({
                "participant_role": role.as_str(),
                "mode": mode.as_str(),
                "device_label": request.device_label,
                "token_issue_count": participant.token_issue_count,
            })),
            Utc::now(),
        )
        .await;

        let clocked_in_at = self.shift_repo.get_attendance_clockin(shift.id).await?;

        Ok(JoinConsultResponse {
            session_id: session.id,
            room_name: session.room_name.clone(),
            url: self.livekit.ws_url().to_string(),
            token: minted.token,
            identity: minted.identity,
            display_name,
            participant_role: role,
            mode,
            can_publish: grants.can_publish,
            can_subscribe: grants.can_subscribe,
            expires_at: minted.expires_at,
            session_status: session.status,
            shift: shift_summary(&shift),
            clock_in: ConsultClockInView {
                mode: if self.virtual_clockin_enabled {
                    "auto_on_join".to_string()
                } else {
                    "manual".to_string()
                },
                already_clocked_in: clocked_in_at.is_some(),
                clocked_in_at,
                fallback_endpoint: format!("/api/v1/shifts/{}/clockin", shift.id),
            },
            recording: ConsultRecordingView::disabled(),
            mock: self.livekit.is_mock(),
        })
    }

    // Session views

    /// Read the session. Platform admins reach this endpoint for support and
    /// get metadata only — never a token.
    pub async fn get_session(
        &self,
        shift_id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultSessionView, VideoServiceError> {
        let shift = self.load_shift(shift_id).await?;
        let access = self.authorize_session_read(&shift, claims).await?;

        let session = self
            .repo
            .find_by_shift(shift_id)
            .await?
            .ok_or(VideoServiceError::SessionNotFound)?;

        self.session_view(&session, access.is_full()).await
    }

    // Companion devices ("Continue on phone")

    /// Mint a single-use code the user's second device can exchange for a join
    /// token.
    ///
    /// The code is the only credential the phone will hold, so: 256 bits of
    /// entropy, a 3-minute life, single use, and only its SHA-256 digest is
    /// stored. Both the issue and the redeem land in `video_session_events`, so
    /// abuse is visible.
    ///
    /// Authorization is the ordinary join check: if you could not get a token
    /// for yourself, you cannot get one for your phone either.
    pub async fn create_handoff(
        &self,
        shift_id: Uuid,
        claims: &Claims,
        request: CreateHandoffRequest,
    ) -> Result<CreateHandoffResponse, VideoServiceError> {
        let user_id = claims_user_id(claims)?;
        let shift = self.load_shift(shift_id).await?;
        let (role, _clinician_id) = self.authorize_shift_access(&shift, claims).await?;

        let session = self
            .repo
            .find_by_shift(shift_id)
            .await?
            .ok_or(VideoServiceError::SessionNotFound)?;
        if session.status == VideoSessionStatus::Ended {
            return Err(VideoServiceError::SessionEnded);
        }

        // The grant carries the issuer's own publish rights, so a phone handed
        // off from an observer seat cannot quietly upgrade itself to publishing.
        let primary = self
            .repo
            .find_participant(session.id, &identity_for_user(user_id))
            .await?;
        let mode = match &primary {
            Some(p) if !p.can_publish => JoinMode::Observer,
            _ => JoinMode::Participant,
        };

        let device_ordinal = self.repo.next_device_ordinal(session.id, user_id).await?;
        let code = generate_handoff_code();
        let expires_at = Utc::now() + Duration::seconds(HANDOFF_TTL_SECONDS);

        self.repo
            .insert_handoff(
                session.id,
                user_id,
                &hash_handoff_code(&code),
                device_ordinal,
                role,
                mode,
                request.device_label.as_deref(),
                expires_at,
            )
            .await?;

        self.audit(
            Some(session.id),
            &session.room_name,
            "handoff_issued",
            Some(&identity_for_device(user_id, device_ordinal)),
            Some(user_id),
            None,
            // Never the code, nor its hash: this trail is read by support.
            Some(serde_json::json!({
                "device_ordinal": device_ordinal,
                "mode": mode.as_str(),
                "device_label": request.device_label,
            })),
            Utc::now(),
        )
        .await;

        Ok(CreateHandoffResponse {
            session_id: session.id,
            handoff_url: handoff_url(shift_id, &code),
            code,
            device_ordinal,
            expires_at,
        })
    }

    /// Exchange a handoff code for a join token. **Unauthenticated**: the code
    /// is the credential, so nothing here reads `Claims`.
    ///
    /// `claim_handoff` spends the code in one statement, so a replay loses the
    /// race rather than minting a second token. Every failure returns the same
    /// `NotAuthorized`, so a caller cannot distinguish "wrong code" from
    /// "expired" from "already used", and cannot probe for live sessions.
    pub async fn redeem_handoff(
        &self,
        code: &str,
    ) -> Result<JoinConsultResponse, VideoServiceError> {
        let grant = self
            .repo
            .claim_handoff(&hash_handoff_code(code))
            .await?
            .ok_or(VideoServiceError::NotAuthorized)?;

        let session = self
            .repo
            .find_session_by_id(grant.session_id)
            .await?
            .ok_or(VideoServiceError::NotAuthorized)?;
        if session.status == VideoSessionStatus::Ended {
            return Err(VideoServiceError::SessionEnded);
        }

        // Re-checked at redemption, not just at issue: a shift can complete or
        // fall out of its window during the 3 minutes the code is alive.
        let shift_id = session.shift_id.ok_or(VideoServiceError::NotAuthorized)?;
        let shift = self.load_shift(shift_id).await?;
        self.check_join_preconditions(&shift)?;

        // Both copied from the primary row rather than resolved again: the grant
        // already fixed who this device acts as.
        let primary = self
            .repo
            .find_participant(session.id, &identity_for_user(grant.user_id))
            .await?;
        let display_name = primary
            .as_ref()
            .and_then(|p| p.display_name.clone())
            .unwrap_or_else(|| "Companion device".to_string());
        let clinician_id = primary.as_ref().and_then(|p| p.clinician_id);

        let identity = identity_for_device(grant.user_id, grant.device_ordinal);
        let grants = grants_for(grant.participant_role, &session.room_name, grant.mode);
        let attributes = HashMap::from([
            (
                "nx_role".to_string(),
                grant.participant_role.as_str().to_string(),
            ),
            ("nx_shift_id".to_string(), shift.id.to_string()),
            ("nx_session_id".to_string(), session.id.to_string()),
            ("nx_device".to_string(), grant.device_ordinal.to_string()),
        ]);

        let minted = self.livekit.mint_token(
            &identity,
            &display_name,
            &attributes,
            &grants,
            self.livekit.token_ttl(),
        )?;

        self.repo
            .upsert_participant_on_token(
                session.id,
                &identity,
                Some(grant.user_id),
                clinician_id,
                &display_name,
                grant.participant_role,
                grants.can_publish,
                minted.expires_at,
                grant.device_ordinal,
                grant.device_label.as_deref(),
            )
            .await?;

        self.audit(
            Some(session.id),
            &session.room_name,
            "handoff_redeemed",
            Some(&identity),
            Some(grant.user_id),
            None,
            Some(serde_json::json!({
                "device_ordinal": grant.device_ordinal,
                "handoff_id": grant.id,
            })),
            Utc::now(),
        )
        .await;

        let clocked_in_at = self.shift_repo.get_attendance_clockin(shift.id).await?;

        Ok(JoinConsultResponse {
            session_id: session.id,
            room_name: session.room_name.clone(),
            url: self.livekit.ws_url().to_string(),
            token: minted.token,
            identity: minted.identity,
            display_name,
            participant_role: grant.participant_role,
            mode: grant.mode,
            can_publish: grants.can_publish,
            can_subscribe: grants.can_subscribe,
            expires_at: minted.expires_at,
            session_status: session.status,
            shift: shift_summary(&shift),
            clock_in: ConsultClockInView {
                mode: if self.virtual_clockin_enabled {
                    "auto_on_join".to_string()
                } else {
                    "manual".to_string()
                },
                already_clocked_in: clocked_in_at.is_some(),
                clocked_in_at,
                fallback_endpoint: format!("/api/v1/shifts/{}/clockin", shift.id),
            },
            recording: ConsultRecordingView::disabled(),
            mock: self.livekit.is_mock(),
        })
    }

    /// Best-effort departure notice, fired from the Leave button and
    /// `beforeunload` in addition to `room.disconnect()`. Idempotent, and it
    /// does not end the call for anyone else or clock the worker out.
    pub async fn leave_session(
        &self,
        shift_id: Uuid,
        claims: &Claims,
    ) -> Result<LeaveConsultResponse, VideoServiceError> {
        let user_id = claims_user_id(claims)?;
        let shift = self.load_shift(shift_id).await?;
        self.authorize_session_read(&shift, claims).await?;

        let session = self
            .repo
            .find_by_shift(shift_id)
            .await?
            .ok_or(VideoServiceError::SessionNotFound)?;

        let identity = identity_for_user(user_id);
        let left_at = Utc::now();
        self.repo
            .mark_participant_left(session.id, &identity, left_at, Some("client_left"))
            .await?;

        self.audit(
            Some(session.id),
            &session.room_name,
            "participant_left_reported",
            Some(&identity),
            Some(user_id),
            None,
            None,
            left_at,
        )
        .await;

        Ok(LeaveConsultResponse {
            session_id: session.id,
            identity,
            left_at,
            session_status: session.status,
            remaining_participants: self.repo.count_connected_participants(session.id).await?,
        })
    }

    /// End the call for everyone. Only the owning hospital's admin or a
    /// platform admin may do this; a clinician leaving uses `/leave`.
    pub async fn end_session(
        &self,
        shift_id: Uuid,
        claims: &Claims,
        reason: Option<String>,
    ) -> Result<EndConsultResponse, VideoServiceError> {
        let shift = self.load_shift(shift_id).await?;
        self.authorize_session_end(&shift, claims)?;

        let session = self
            .repo
            .find_by_shift(shift_id)
            .await?
            .ok_or(VideoServiceError::SessionNotFound)?;

        // Disconnects everyone. Failing here must not stop us recording the
        // end: the reconciler would otherwise keep the row `active` forever.
        if let Err(e) = self.livekit.delete_room(&session.room_name).await {
            tracing::warn!("LiveKit delete_room failed for {}: {e}", session.room_name);
        }

        let now = Utc::now();
        // `mark_ended` is `WHERE status <> 'ended'`, so a second call returns
        // None and the original `ended_at` is what the caller sees.
        let ended = match self
            .repo
            .mark_ended(&session.room_name, now, "ended_by_hospital")
            .await?
        {
            Some(updated) => {
                self.repo.close_open_participants(updated.id, now).await?;
                self.audit(
                    Some(updated.id),
                    &updated.room_name,
                    "ended_by_hospital",
                    None,
                    claims_user_id(claims).ok(),
                    None,
                    reason.map(|r| serde_json::json!({ "reason": r })),
                    now,
                )
                .await;
                updated
            }
            None => self
                .repo
                .find_by_room_name(&session.room_name)
                .await?
                .unwrap_or(session),
        };

        Ok(EndConsultResponse {
            session_id: ended.id,
            status: ended.status,
            ended_at: ended.ended_at,
            ended_reason: ended.ended_reason,
            clock_out_required: true,
            clock_out_hint:
                "The clinician must submit a handover, then POST /api/v1/shifts/{shift_id}/clockout"
                    .to_string(),
        })
    }

    // Webhooks

    pub fn verify_webhook(
        &self,
        body: &str,
        auth_token: &str,
    ) -> Result<LiveKitWebhookEvent, VideoServiceError> {
        Ok(self.livekit.verify_webhook(body, auth_token)?)
    }

    /// Dedupe, audit, then dispatch. Every branch is idempotent, because
    /// LiveKit guarantees neither delivery nor ordering.
    pub async fn process_webhook_event(
        &self,
        event: LiveKitWebhookEvent,
    ) -> Result<WebhookOutcome, VideoServiceError> {
        let key = event.idempotency_key();
        let Some(webhook_id) = self
            .repo
            .insert_livekit_webhook_if_new(&key, &event.event, &event.raw)
            .await?
        else {
            tracing::debug!("LiveKit webhook {key} already seen");
            return Ok(WebhookOutcome::AlreadySeen);
        };

        let result = self.dispatch_webhook_event(&event).await;
        let error = result.as_ref().err().map(|e| e.to_string());
        self.repo
            .mark_webhook_processed(webhook_id, error.as_deref())
            .await?;
        result
    }

    async fn dispatch_webhook_event(
        &self,
        event: &LiveKitWebhookEvent,
    ) -> Result<WebhookOutcome, VideoServiceError> {
        let Some(room_name) = event.room_name.clone() else {
            tracing::warn!("LiveKit {} event carried no room", event.event);
            return Ok(WebhookOutcome::UnknownRoom);
        };

        let session = self.repo.find_by_room_name(&room_name).await?;

        // Audited before dispatch, and even for a room we do not know, so the
        // trail survives a session row that was never created.
        self.audit(
            session.as_ref().map(|s| s.id),
            &room_name,
            &event.event,
            event.participant_identity.as_deref(),
            None,
            Some(&event.idempotency_key()),
            Some(event.raw.clone()),
            event.created_at,
        )
        .await;

        let Some(session) = session else {
            tracing::warn!("LiveKit event for unknown room {room_name}");
            return Ok(WebhookOutcome::UnknownRoom);
        };

        match event.event.as_str() {
            "room_started" => {
                self.repo
                    .mark_started(&room_name, event.created_at, event.room_sid.as_deref())
                    .await?;
                Ok(WebhookOutcome::Processed)
            }
            "participant_joined" => self.on_participant_joined(&session, event).await,
            "participant_left" | "participant_connection_aborted" => {
                self.on_participant_left(&session, event).await
            }
            "room_finished" => self.on_room_finished(&session, event).await,
            // Audit-only. `egress_*` / `ingress_*` are the recording seam: the
            // payloads are captured from day one so they are already there when
            // recording ships.
            "track_published" | "track_unpublished" => Ok(WebhookOutcome::Ignored),
            other if other.starts_with("egress_") || other.starts_with("ingress_") => {
                Ok(WebhookOutcome::Ignored)
            }
            other => {
                tracing::debug!("Unhandled LiveKit event {other} for room {room_name}");
                Ok(WebhookOutcome::Ignored)
            }
        }
    }

    async fn on_participant_joined(
        &self,
        session: &VideoSession,
        event: &LiveKitWebhookEvent,
    ) -> Result<WebhookOutcome, VideoServiceError> {
        let Some(identity) = event.participant_identity.as_deref() else {
            return Ok(WebhookOutcome::Ignored);
        };

        let joined = self
            .repo
            .mark_participant_joined(
                session.id,
                identity,
                event.participant_sid.as_deref(),
                event.created_at,
            )
            .await?;

        // A room whose first participant we never issued a token for should be
        // impossible — only we hold the API secret.
        let Some(participant) = joined else {
            tracing::warn!(
                "LiveKit participant_joined for unknown identity {identity} in {}",
                session.room_name
            );
            return Ok(WebhookOutcome::Processed);
        };

        self.repo
            .mark_started(&session.room_name, event.created_at, event.room_sid.as_deref())
            .await?;

        self.maybe_clock_in(session, &participant, event.created_at)
            .await?;

        Ok(WebhookOutcome::Processed)
    }

    async fn on_participant_left(
        &self,
        session: &VideoSession,
        event: &LiveKitWebhookEvent,
    ) -> Result<WebhookOutcome, VideoServiceError> {
        let Some(identity) = event.participant_identity.as_deref() else {
            return Ok(WebhookOutcome::Ignored);
        };

        // Video tables only. Clocking out here would flip the shift to
        // `completed` on a dropped WiFi connection, which the payout scheduler
        // would then pay out.
        self.repo
            .mark_participant_left(
                session.id,
                identity,
                event.created_at,
                event.disconnect_reason_name(),
            )
            .await?;

        Ok(WebhookOutcome::Processed)
    }

    async fn on_room_finished(
        &self,
        session: &VideoSession,
        event: &LiveKitWebhookEvent,
    ) -> Result<WebhookOutcome, VideoServiceError> {
        if let Some(ended) = self
            .repo
            .mark_ended(&session.room_name, event.created_at, "room_finished")
            .await?
        {
            self.repo
                .close_open_participants(ended.id, event.created_at)
                .await?;
            self.nudge_missing_handover(&ended).await;
        }

        Ok(WebhookOutcome::Processed)
    }

    /// The join → clock-in mapping, behind three idempotency layers: the
    /// `webhook_events` dedupe above, `claim_clockin_slot`'s row lock here, and
    /// `record_clockin_if_absent_tx`'s `WHERE clockin_at IS NULL` below.
    async fn maybe_clock_in(
        &self,
        session: &VideoSession,
        participant: &VideoSessionParticipant,
        at: DateTime<Utc>,
    ) -> Result<(), VideoServiceError> {
        let skip = if !self.virtual_clockin_enabled {
            Some("clockin_skipped:flag_off")
        } else if participant.participant_role != ParticipantRole::Clinician {
            Some("clockin_skipped:not_clinician")
        } else if session.shift_id.is_none() {
            // Ad-hoc seam: no shift, so nothing to clock in to.
            Some("clockin_skipped:adhoc_session")
        } else if participant.user_id.is_none() {
            // Guest seam: a future patient link has no platform user.
            Some("clockin_skipped:no_user")
        } else {
            None
        };

        if let Some(reason) = skip {
            self.audit_clockin(session, participant, reason, None, at).await;
            return Ok(());
        }

        let (Some(shift_id), Some(user_id)) = (session.shift_id, participant.user_id) else {
            return Ok(());
        };

        // The clock-in belongs to the person, not the device they joined on, so
        // the slot is always claimed on their primary row.
        let clockin_identity = primary_identity(&participant.identity).to_string();

        if self
            .repo
            .claim_clockin_slot(session.id, &clockin_identity, at)
            .await?
            .is_none()
        {
            self.audit_clockin(session, participant, "clockin_skipped:already_claimed", None, at)
                .await;
            return Ok(());
        }

        let outcome = match self
            .shift_service
            .virtual_clock_in_on_join(shift_id, user_id)
            .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                // Only a genuine failure hands the slot back, so the reconciler
                // retries this join instead of the webhook silently losing it.
                self.repo
                    .release_clockin_slot(session.id, &clockin_identity)
                    .await?;
                tracing::error!("Virtual clock-in failed for shift {shift_id}: {e}");
                return Ok(());
            }
        };

        let reason = outcome.audit_reason();
        let detail = match &outcome {
            VirtualClockinOutcome::ClockedIn {
                attendance_id,
                late_minutes,
                late_penalty_applied,
            } => Some(serde_json::json!({
                "attendance_id": attendance_id,
                "late_minutes": late_minutes,
                "late_penalty_applied": late_penalty_applied,
            })),
            VirtualClockinOutcome::OutsideWindow { minutes_from_start } => {
                Some(serde_json::json!({ "minutes_from_start": minutes_from_start }))
            }
            _ => None,
        };
        self.audit_clockin(session, participant, &reason, detail, at)
            .await;

        if let VirtualClockinOutcome::ClockedIn { late_minutes, .. } = outcome {
            self.push
                .notify_best_effort(
                    user_id,
                    "shift_clockin",
                    "You're clocked in",
                    "Joining the consultation recorded your clock-in.",
                    serde_json::json!({ "shift_id": shift_id, "late_minutes": late_minutes }),
                )
                .await;
        }

        Ok(())
    }

    // Reconciler
    //
    // `insert_livekit_webhook_if_new` runs *before* processing, so a crash
    // mid-flight leaves the dedupe row behind and LiveKit's retry is swallowed.
    // This sweep derives state from LiveKit and the database instead of from
    // webhooks, which is what closes that hole. It is the only place clock-out
    // is ever automated.

    pub async fn reconcile_sessions(&self) -> Result<ReconcileReport, VideoServiceError> {
        let mut report = ReconcileReport::default();
        let now = Utc::now();

        report.joins_recovered = self
            .recover_missed_joins(now - chrono::Duration::minutes(RECONCILE_JOIN_GRACE_MINUTES))
            .await?;
        report.sessions_ended = self
            .close_vanished_rooms(now - chrono::Duration::minutes(RECONCILE_STALE_MINUTES), now)
            .await?;
        let (clock_outs, handover_reminders) = self
            .settle_ended_sessions(now - chrono::Duration::minutes(RECONCILE_CLOCKOUT_DELAY_MINUTES))
            .await?;
        report.clock_outs = clock_outs;
        report.handover_reminders = handover_reminders;

        Ok(report)
    }

    /// Branch 1 — a `participant_joined` that never arrived. LiveKit's live
    /// participant list is the authority.
    async fn recover_missed_joins(
        &self,
        token_issued_before: DateTime<Utc>,
    ) -> Result<usize, VideoServiceError> {
        let mut recovered = 0;

        for session in self.repo.sessions_awaiting_join(token_issued_before).await? {
            let live = match self.livekit.list_participants(&session.room_name).await {
                Ok(live) => live,
                // The room was never brought up, so there is no join to
                // recover. Expected for an abandoned pre-join screen, and not
                // worth a warning.
                Err(LiveKitError::RoomNotFound) => continue,
                Err(e) => {
                    tracing::warn!(
                        "Reconciler could not list participants for {}: {e}",
                        session.room_name
                    );
                    continue;
                }
            };

            for present in live {
                let Some(participant) = self
                    .repo
                    .mark_participant_joined(
                        session.id,
                        &present.identity,
                        Some(&present.sid),
                        Utc::now(),
                    )
                    .await?
                else {
                    continue;
                };

                self.repo
                    .mark_started(&session.room_name, Utc::now(), None)
                    .await?;
                self.audit(
                    Some(session.id),
                    &session.room_name,
                    "reconciled_join",
                    Some(&present.identity),
                    None,
                    None,
                    None,
                    Utc::now(),
                )
                .await;
                self.maybe_clock_in(&session, &participant, Utc::now()).await?;
                recovered += 1;
            }
        }

        Ok(recovered)
    }

    /// Branch 2 — a `room_finished` that never arrived.
    async fn close_vanished_rooms(
        &self,
        updated_before: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<usize, VideoServiceError> {
        let mut ended = 0;

        for session in self.repo.stale_active_sessions(updated_before).await? {
            // In mock mode there is no LiveKit to ask, so leave the row alone
            // rather than ending live sessions on local dev.
            if self.livekit.is_mock() {
                continue;
            }

            match self.livekit.list_participants(&session.room_name).await {
                // Still occupied — the room is fine, the webhook was just quiet.
                Ok(participants) if !participants.is_empty() => continue,
                // Empty, or gone entirely. Either way the consult is over and
                // the `room_finished` webhook never reached us. `RoomNotFound`
                // is the common case: LiveKit tears an empty room down after
                // `empty_timeout`, so by the time we sweep it has usually
                // stopped existing rather than merely emptied.
                Ok(_) | Err(LiveKitError::RoomNotFound) => {}
                Err(e) => {
                    tracing::warn!(
                        "Reconciler could not reach LiveKit for {}: {e}",
                        session.room_name
                    );
                    continue;
                }
            }

            if let Some(closed) = self
                .repo
                .mark_ended(&session.room_name, now, "reconciled_missing")
                .await?
            {
                self.repo.close_open_participants(closed.id, now).await?;
                self.nudge_missing_handover(&closed).await;
                ended += 1;
            }
        }

        Ok(ended)
    }

    /// Branch 3 — the consult is over and the worker is still clocked in. This
    /// is the only automated clock-out in the system, and it still refuses
    /// without a handover, exactly as the endpoint does.
    async fn settle_ended_sessions(
        &self,
        ended_before: DateTime<Utc>,
    ) -> Result<(usize, usize), VideoServiceError> {
        let mut clock_outs = 0;
        let mut reminders = 0;

        for pending in self.repo.ended_sessions_pending_clockout(ended_before).await? {
            if self.shift_repo.get_handover(pending.shift_id).await?.is_none() {
                if self.remind_handover(pending.session_id, &pending.room_name, pending.clinician_id).await? {
                    reminders += 1;
                }
                continue;
            }

            let Some(worker_user_id) = self
                .shift_repo
                .get_clinician_user_id(pending.clinician_id)
                .await?
            else {
                continue;
            };

            match self
                .shift_service
                .clock_out(pending.shift_id, worker_user_id)
                .await
            {
                Ok(_) => {
                    self.audit(
                        Some(pending.session_id),
                        &pending.room_name,
                        "reconciled_clock_out",
                        None,
                        None,
                        None,
                        None,
                        Utc::now(),
                    )
                    .await;
                    clock_outs += 1;
                }
                Err(e) => tracing::warn!(
                    "Reconciler could not clock out shift {}: {e}",
                    pending.shift_id
                ),
            }
        }

        Ok((clock_outs, reminders))
    }

    // Helpers

    /// Seed-and-store the room. `ON CONFLICT (shift_id) DO UPDATE` makes this
    /// safe under two concurrent join requests.
    async fn ensure_session_for_shift(
        &self,
        shift: &Shift,
        created_by: Uuid,
    ) -> Result<VideoSession, VideoServiceError> {
        let defaults = self.livekit.room_defaults();
        Ok(self
            .repo
            .upsert_session_for_shift(
                shift.id,
                shift.hospital_id,
                Some(created_by),
                &room_name_for_shift(shift.id),
                defaults.max_participants as i32,
                defaults.empty_timeout_s as i32,
                defaults.departure_timeout_s as i32,
            )
            .await?)
    }

    /// The tenant boundary. There is no RLS — this check *is* it.
    ///
    /// Returns the caller's room role and, for a clinician, their profile id.
    async fn authorize_shift_access(
        &self,
        shift: &Shift,
        claims: &Claims,
    ) -> Result<(ParticipantRole, Option<Uuid>), VideoServiceError> {
        self.check_join_preconditions(shift)?;

        match claims.role {
            UserRole::HealthWorker => {
                let user_id = claims_user_id(claims)?;
                let clinician_id = self
                    .shift_repo
                    .find_clinician_id_for_user(user_id)
                    .await?
                    .ok_or(VideoServiceError::NoClinicianProfile)?;

                // Having applied, been offered, or declined confers no claim on
                // the room — only the accepted assignment does.
                if shift.assigned_clinician_id != Some(clinician_id) {
                    return Err(VideoServiceError::NotAuthorized);
                }
                Ok((ParticipantRole::Clinician, Some(clinician_id)))
            }
            UserRole::HospitalAdmin => {
                if claims_hospital_id(claims) != Some(shift.hospital_id) {
                    return Err(VideoServiceError::NotAuthorized);
                }
                Ok((ParticipantRole::HospitalObserver, None))
            }
            // NDPR gives platform staff no lawful basis to sit inside a clinical
            // consultation. Support needs metadata, not video.
            _ => Err(VideoServiceError::NotAuthorized),
        }
    }

    /// Read access is wider than join access: platform admins can see who
    /// joined and when, and the time window does not apply to looking.
    async fn authorize_session_read(
        &self,
        shift: &Shift,
        claims: &Claims,
    ) -> Result<ClinicalAccess, VideoServiceError> {
        // Reading the *room* still requires a virtual shift; reading the shift's
        // patient data does not, which is the one precondition
        // `authorize_shift_data_access` deliberately leaves out.
        if shift.shift_type != ShiftType::Virtual {
            return Err(VideoServiceError::NotVirtualShift);
        }
        Ok(self
            .authorize_shift_data_access(shift, claims)
            .await?
            .clinical_access())
    }

    /// Load a shift for an authorization decision made by another service.
    ///
    /// `load_shift` is private and `ShiftRepository::get_by_id` returns a bare
    /// `Option`; this gives callers the same `ShiftNotFound` they would get
    /// from the video endpoints, so error mapping stays consistent.
    pub async fn load_shift_for_authz(
        &self,
        shift_id: Uuid,
    ) -> Result<Shift, VideoServiceError> {
        self.load_shift(shift_id).await
    }

    /// The consult session for a shift, if one has been started. `None` is not
    /// an error: a clinical note can be written for an in-person shift, or
    /// before anybody requests a token.
    pub async fn session_id_for_shift(
        &self,
        shift_id: Uuid,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        Ok(self.repo.find_by_shift(shift_id).await?.map(|s| s.id))
    }

    /// The tenant boundary for everything a shift owns. There is no RLS — this
    /// check *is* it.
    ///
    /// Deliberately omits both of `authorize_shift_access`'s join preconditions:
    /// a patient record is not a room, so it is readable for an in-person shift
    /// and long after the consultation window has closed. Writing is narrowed by
    /// the caller, not here.
    ///
    /// This is the `claims.hospital_id == shift.hospital_id` idiom, **not** the
    /// `shift.created_by == claims.sub` one used elsewhere in the shifts domain:
    /// the latter would 403 a second admin at the same hospital, and would break
    /// entirely once the posting admin is deactivated.
    pub async fn authorize_shift_data_access(
        &self,
        shift: &Shift,
        claims: &Claims,
    ) -> Result<ShiftDataRole, VideoServiceError> {
        match claims.role {
            UserRole::SuperAdmin | UserRole::OperationsAdmin => Ok(ShiftDataRole::PlatformAdmin),
            UserRole::HealthWorker => {
                let user_id = claims_user_id(claims)?;
                let clinician_id = self
                    .shift_repo
                    .find_clinician_id_for_user(user_id)
                    .await?
                    .ok_or(VideoServiceError::NoClinicianProfile)?;
                // Having applied, been offered, or declined confers nothing —
                // only the accepted assignment does.
                if shift.assigned_clinician_id != Some(clinician_id) {
                    return Err(VideoServiceError::NotAuthorized);
                }
                Ok(ShiftDataRole::Clinician(clinician_id))
            }
            UserRole::HospitalAdmin => {
                if claims_hospital_id(claims) != Some(shift.hospital_id) {
                    return Err(VideoServiceError::NotAuthorized);
                }
                Ok(ShiftDataRole::HospitalAdmin)
            }
            _ => Err(VideoServiceError::NotAuthorized),
        }
    }

    fn authorize_session_end(
        &self,
        shift: &Shift,
        claims: &Claims,
    ) -> Result<(), VideoServiceError> {
        match claims.role {
            UserRole::SuperAdmin | UserRole::OperationsAdmin => Ok(()),
            UserRole::HospitalAdmin if claims_hospital_id(claims) == Some(shift.hospital_id) => {
                Ok(())
            }
            _ => Err(VideoServiceError::NotAuthorized),
        }
    }

    /// Checked for everyone, before we look at who the caller is.
    fn check_join_preconditions(&self, shift: &Shift) -> Result<(), VideoServiceError> {
        if shift.shift_type != ShiftType::Virtual {
            return Err(VideoServiceError::NotVirtualShift);
        }

        if !matches!(
            shift.status,
            ShiftStatus::Assigned | ShiftStatus::Upcoming | ShiftStatus::InProgress
        ) {
            return Err(VideoServiceError::ShiftNotJoinable(format!(
                "Shift is {:?}",
                shift.status
            )));
        }

        let now = Utc::now();
        let window = chrono::Duration::minutes(JOIN_WINDOW_MINUTES);
        if now < shift.scheduled_start - window || now > shift.scheduled_end + window {
            return Err(VideoServiceError::OutsideWindow);
        }

        Ok(())
    }

    async fn load_shift(&self, shift_id: Uuid) -> Result<Shift, VideoServiceError> {
        self.shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(VideoServiceError::ShiftNotFound(shift_id))
    }

    /// Reconcile the participant list against LiveKit where we can. `live` tells
    /// the client which of the two it is looking at.
    /// `with_patient_detail` is the NDPR split: the owning hospital and the
    /// assigned clinician see who is waiting, platform admins see only how many.
    async fn session_view(
        &self,
        session: &VideoSession,
        with_patient_detail: bool,
    ) -> Result<ConsultSessionView, VideoServiceError> {
        let stored = self.repo.list_participants(session.id).await?;

        let (connected, live) = match self.livekit.list_participants(&session.room_name).await {
            Ok(present) if !self.livekit.is_mock() => (
                Some(
                    present
                        .into_iter()
                        .map(|p| (p.identity, p.is_publisher))
                        .collect::<HashMap<_, _>>(),
                ),
                true,
            ),
            // Mock mode has nothing to reconcile against, so fall back to the
            // webhook-fed state and say so.
            Ok(_) => (None, false),
            // A room that does not exist is an authoritative empty room, so
            // this list *is* reconciled — everyone is disconnected.
            Err(LiveKitError::RoomNotFound) => (Some(HashMap::new()), true),
            Err(e) => {
                tracing::warn!("LiveKit list_participants failed for {}: {e}", session.room_name);
                (None, false)
            }
        };

        let participants = stored
            .iter()
            .map(|p| {
                let live_state = connected.as_ref().map(|c| c.get(&p.identity));
                ConsultParticipantView {
                    identity: p.identity.clone(),
                    display_name: p.display_name.clone(),
                    participant_role: p.participant_role,
                    connected: match live_state {
                        Some(state) => state.is_some(),
                        None => p.joined_at.is_some() && p.left_at.is_none(),
                    },
                    joined_at: p.joined_at,
                    left_at: p.left_at,
                    is_publisher: match live_state.flatten() {
                        Some(is_publisher) => *is_publisher,
                        None => p.can_publish,
                    },
                    clocked_in_at: p.clocked_in_at,
                    device_ordinal: p.device_ordinal,
                    device_label: p.device_label.clone(),
                }
            })
            .collect();

        let waiting_room = self
            .waiting_room_view(session.id, with_patient_detail)
            .await?;

        Ok(ConsultSessionView {
            session_id: session.id,
            shift_id: session.shift_id,
            room_name: session.room_name.clone(),
            status: session.status,
            started_at: session.started_at,
            ended_at: session.ended_at,
            ended_reason: session.ended_reason.clone(),
            live,
            clock_in_recorded: stored.iter().any(|p| p.clocked_in_at.is_some()),
            participants,
            waiting_room,
            recording: ConsultRecordingView::disabled(),
        })
    }

    /// Counts always; the patient list only for a caller entitled to clinical
    /// content. Built here rather than in `PatientRecordService` because
    /// `GET /consult` already holds the session and must stay one round trip.
    pub(crate) async fn waiting_room_view(
        &self,
        session_id: Uuid,
        with_patient_detail: bool,
    ) -> Result<ConsultWaitingRoomView, VideoServiceError> {
        let counts = self.queue_repo.counts_for_session(session_id).await?;

        let patients = if with_patient_detail {
            self.queue_repo
                .list_for_session(session_id)
                .await?
                .into_iter()
                .map(|e| ConsultQueuePatientView {
                    entry_id: e.id,
                    patient_id: e.patient_id,
                    full_name: e.full_name,
                    age: e.age,
                    gender: e.gender,
                    severity_level: e.severity_level,
                    predictive_risk_score: e.predictive_risk_score,
                    state: e.state,
                    position: e.position,
                    called_at: e.called_at,
                    seen_at: e.seen_at,
                    queued_at: e.created_at,
                    has_clinical_note: e.has_clinical_note,
                    has_handover_note: e.has_handover_note,
                })
                .collect()
        } else {
            Vec::new()
        };

        Ok(ConsultWaitingRoomView {
            waiting: counts.waiting,
            in_consult: counts.in_consult,
            seen: counts.seen,
            total: counts.total,
            patients,
        })
    }

    /// Best-effort display name: the clinician's own name where we have one,
    /// otherwise the email on the token.
    async fn display_name_for(&self, claims: &Claims, clinician_id: Option<Uuid>) -> String {
        if let Some(clinician_id) = clinician_id {
            if let Ok(Some((first_name, last_name, _email))) =
                self.shift_repo.get_clinician_contact(clinician_id).await
            {
                return format!("{first_name} {last_name}").trim().to_string();
            }
        }
        claims.email.clone()
    }

    /// One-shot per session, guarded by the audit trail itself.
    async fn remind_handover(
        &self,
        session_id: Uuid,
        room_name: &str,
        clinician_id: Uuid,
    ) -> Result<bool, VideoServiceError> {
        if self.repo.has_event(session_id, HANDOVER_REMINDER_EVENT).await? {
            return Ok(false);
        }

        let Some(user_id) = self.shift_repo.get_clinician_user_id(clinician_id).await? else {
            return Ok(false);
        };

        self.push
            .notify_best_effort(
                user_id,
                "handover_reminder",
                "Submit your handover",
                "Your consultation has ended. Submit a handover to clock out.",
                serde_json::json!({ "session_id": session_id }),
            )
            .await;

        self.audit(
            Some(session_id),
            room_name,
            HANDOVER_REMINDER_EVENT,
            None,
            None,
            None,
            None,
            Utc::now(),
        )
        .await;

        Ok(true)
    }

    /// Nudge the clinician when a room ends with no handover on file.
    async fn nudge_missing_handover(&self, session: &VideoSession) {
        let Some(shift_id) = session.shift_id else {
            return;
        };

        let missing_handover = matches!(self.shift_repo.get_handover(shift_id).await, Ok(None));
        if !missing_handover {
            return;
        }

        let clinician_id = match self.shift_repo.get_by_id(shift_id).await {
            Ok(Some(shift)) => shift.assigned_clinician_id,
            _ => None,
        };
        let Some(clinician_id) = clinician_id else {
            return;
        };

        if let Err(e) = self
            .remind_handover(session.id, &session.room_name, clinician_id)
            .await
        {
            tracing::warn!("Handover reminder failed for session {}: {e}", session.id);
        }
    }

    async fn audit_clockin(
        &self,
        session: &VideoSession,
        participant: &VideoSessionParticipant,
        event_type: &str,
        payload: Option<serde_json::Value>,
        at: DateTime<Utc>,
    ) {
        self.audit(
            Some(session.id),
            &session.room_name,
            event_type,
            Some(&participant.identity),
            None,
            None,
            payload,
            at,
        )
        .await;
    }

    /// The audit trail is NDPR evidence, but it is not worth failing a request
    /// or a webhook over — a failed insert is logged and swallowed.
    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        session_id: Option<Uuid>,
        room_name: &str,
        event_type: &str,
        identity: Option<&str>,
        actor_user_id: Option<Uuid>,
        livekit_event_id: Option<&str>,
        payload: Option<serde_json::Value>,
        occurred_at: DateTime<Utc>,
    ) {
        let event = NewVideoSessionEvent {
            session_id,
            room_name: room_name.to_string(),
            event_type: event_type.to_string(),
            identity: identity.map(str::to_string),
            actor_user_id,
            livekit_event_id: livekit_event_id.map(str::to_string),
            payload,
            occurred_at,
        };
        if let Err(e) = self.repo.insert_event(event).await {
            tracing::warn!("Failed to record video audit event {event_type}: {e}");
        }
    }
}

fn shift_summary(shift: &Shift) -> ConsultShiftSummary {
    ConsultShiftSummary {
        id: shift.id,
        role_title: shift.role_title.clone(),
        hospital_name: shift.hospital_name.clone(),
        scheduled_start: shift.scheduled_start,
        scheduled_end: shift.scheduled_end,
        status: format!("{:?}", shift.status).to_lowercase(),
        shift_type: match shift.shift_type {
            ShiftType::Virtual => "virtual".to_string(),
            ShiftType::InPerson => "in_person".to_string(),
        },
    }
}

fn claims_user_id(claims: &Claims) -> Result<Uuid, VideoServiceError> {
    Uuid::parse_str(&claims.sub).map_err(|_| VideoServiceError::NotAuthorized)
}

fn claims_hospital_id(claims: &Claims) -> Option<Uuid> {
    claims
        .hospital_id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_name_round_trips() {
        let shift_id = Uuid::new_v4();
        let room = room_name_for_shift(shift_id);
        assert_eq!(room, format!("shift-{shift_id}"));
        assert_eq!(shift_id_from_room_name(&room), Some(shift_id));
    }

    #[test]
    fn shift_id_from_room_name_rejects_anything_else() {
        for bad in ["garbage", "", "shift-", "shift-not-a-uuid", "SHIFT-", "shift"] {
            assert_eq!(shift_id_from_room_name(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn identity_is_prefixed_with_the_user_marker() {
        let user_id = Uuid::new_v4();
        assert_eq!(identity_for_user(user_id), format!("u:{user_id}"));
    }
}

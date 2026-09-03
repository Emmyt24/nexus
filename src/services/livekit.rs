// ! LiveKit transport. Every `livekit_api` type is confined to this module —
// ! the same containment `here_maps.rs` gives reqwest — so the rest of the
// ! codebase talks in local DTOs and mock mode stays a one-line branch.
// !
// ! Empty `LIVEKIT_API_KEY` / `LIVEKIT_API_SECRET` flips the client into mock
// ! mode: fake tokens, no-op room calls, and unsigned webhooks accepted. That
// ! is the default for local dev and CI, neither of which has credentials.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use livekit_api::access_token::{AccessToken, TokenVerifier, VideoGrants};
use livekit_api::services::room::{CreateRoomOptions, RoomClient};
use livekit_api::services::{ServerError, ServerErrorCode, ServiceError};
use livekit_api::webhooks::WebhookReceiver;
use livekit_protocol as proto;

use crate::models::video_session::{JoinMode, ParticipantRole};

/// Default join-token lifetime. A join ticket, not a call deadline: LiveKit
/// validates the token at `connect()` only, so a short TTL never drops a live
/// call.
const DEFAULT_TOKEN_TTL_SECONDS: u64 = 900;
const DEFAULT_MAX_PARTICIPANTS: u32 = 4;
const DEFAULT_EMPTY_TIMEOUT_SECONDS: u32 = 900;
const DEFAULT_DEPARTURE_TIMEOUT_SECONDS: u32 = 120;
const ROOM_API_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum LiveKitError {
    #[error("LiveKit token minting failed: {0}")]
    Token(String),

    #[error("LiveKit room API call failed: {0}")]
    Service(String),

    /// LiveKit answered, and the answer was that the room is gone. A negative
    /// fact, not a failure — the reconciler needs to tell it apart from an
    /// unreachable server, because "the room vanished" is precisely the signal
    /// that a `room_finished` webhook was lost.
    #[error("LiveKit room does not exist")]
    RoomNotFound,

    #[error("LiveKit webhook verification failed: {0}")]
    WebhookVerification(String),

    #[error("LiveKit is not configured (mock mode)")]
    NotConfigured,
}

/// The grants we are willing to mint, as business policy rather than SDK
/// vocabulary. Kept local so `grants_for` is unit-testable without the SDK, and
/// so the fields we never grant (`room_create`, `room_record`, `hidden`) are
/// not even representable here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenGrants {
    pub room: String,
    pub can_publish: bool,
    pub can_subscribe: bool,
    pub can_publish_data: bool,
    pub room_admin: bool,
}

impl TokenGrants {
    /// Lower to the SDK's grant set. Everything not in `TokenGrants` is
    /// explicitly `false` — this is the single place that decides that.
    pub fn to_video_grants(&self) -> VideoGrants {
        VideoGrants {
            room_join: true,
            room: self.room.clone(),
            can_publish: self.can_publish,
            can_subscribe: self.can_subscribe,
            can_publish_data: self.can_publish_data,
            room_admin: self.room_admin,
            // Never granted, to anyone. `room_record` is the egress seam and
            // stays off until recording ships with consent capture; `hidden`
            // would put an invisible participant inside a clinical call;
            // `room_create` would be a resource-exhaustion vector.
            room_create: false,
            room_record: false,
            hidden: false,
            room_list: false,
            recorder: false,
            ingress_admin: false,
            can_update_own_metadata: false,
            // Left empty on purpose: all sources allowed, because screen share
            // is useful in telemedicine.
            can_publish_sources: Vec::new(),
            destination_room: String::new(),
        }
    }
}

/// The authorization surface, as a pure function. Table-tested in this module.
pub fn grants_for(role: ParticipantRole, room: &str, mode: JoinMode) -> TokenGrants {
    let observing = mode == JoinMode::Observer;
    TokenGrants {
        room: room.to_string(),
        // An observer subscribes but never publishes.
        can_publish: !observing,
        can_subscribe: true,
        can_publish_data: !observing,
        // Only the hospital gets moderation rights, so it can mute or remove a
        // participant from the client SDK without a server round trip.
        room_admin: role == ParticipantRole::HospitalObserver,
    }
}

#[derive(Debug, Clone)]
pub struct MintedToken {
    pub token: String,
    pub identity: String,
    pub expires_at: DateTime<Utc>,
}

/// Room lifecycle knobs. LiveKit auto-creates rooms on first join, so these are
/// an optimisation — losing them to a failed `ensure_room` must never fail a
/// join.
#[derive(Debug, Clone, Copy)]
pub struct RoomOptions {
    pub max_participants: u32,
    pub empty_timeout_s: u32,
    pub departure_timeout_s: u32,
}

impl Default for RoomOptions {
    fn default() -> Self {
        Self {
            max_participants: DEFAULT_MAX_PARTICIPANTS,
            empty_timeout_s: DEFAULT_EMPTY_TIMEOUT_SECONDS,
            departure_timeout_s: DEFAULT_DEPARTURE_TIMEOUT_SECONDS,
        }
    }
}

/// A participant as LiveKit currently sees them — the authoritative answer to
/// "who is in the room right now", used to reconcile our webhook-fed state.
#[derive(Debug, Clone)]
pub struct LiveParticipant {
    pub identity: String,
    pub sid: String,
    pub name: String,
    pub is_publisher: bool,
}

/// Local mirror of `livekit_protocol::WebhookEvent`, so no SDK type escapes
/// this module.
#[derive(Debug, Clone)]
pub struct LiveKitWebhookEvent {
    pub event: String,
    pub event_id: String,
    pub created_at: DateTime<Utc>,
    pub room_name: Option<String>,
    pub room_sid: Option<String>,
    pub participant_identity: Option<String>,
    pub participant_sid: Option<String>,
    pub participant_name: Option<String>,
    pub disconnect_reason: Option<i32>,
    pub raw: serde_json::Value,
}

impl LiveKitWebhookEvent {
    /// Key for `webhook_events.provider_event_id`. LiveKit's `id` is a unique
    /// event uuid and is what we want; older builds omit it, so fall back to a
    /// digest of the fields that identify the delivery.
    pub fn idempotency_key(&self) -> String {
        if !self.event_id.trim().is_empty() {
            return self.event_id.clone();
        }
        format!(
            "{}:{}:{}:{}",
            self.event,
            self.room_name.as_deref().unwrap_or(""),
            self.participant_identity.as_deref().unwrap_or(""),
            self.created_at.timestamp()
        )
    }

    /// LiveKit's `DisconnectReason` as its protobuf name, for the audit trail.
    pub fn disconnect_reason_name(&self) -> Option<&'static str> {
        self.disconnect_reason
            .and_then(|code| proto::DisconnectReason::try_from(code).ok())
            .map(|reason| reason.as_str_name())
    }
}

pub struct LiveKitClient {
    /// `wss://…`, handed to the frontend verbatim for the client SDK.
    ws_url: String,
    api_key: String,
    api_secret: String,
    token_ttl: Duration,
    room_defaults: RoomOptions,
    /// `None` in mock mode.
    room: Option<RoomClient>,
}

impl LiveKitClient {
    pub fn new(ws_url: String, api_key: String, api_secret: String) -> Self {
        Self::with_options(ws_url, api_key, api_secret, Duration::from_secs(DEFAULT_TOKEN_TTL_SECONDS), RoomOptions::default())
    }

    pub fn with_options(
        ws_url: String,
        api_key: String,
        api_secret: String,
        token_ttl: Duration,
        room_defaults: RoomOptions,
    ) -> Self {
        let is_mock = api_key.trim().is_empty() || api_secret.trim().is_empty();
        let room = (!is_mock).then(|| {
            RoomClient::with_api_key(&api_host_from_ws_url(&ws_url), &api_key, &api_secret)
                .with_request_timeout(ROOM_API_TIMEOUT)
        });
        Self {
            ws_url,
            api_key,
            api_secret,
            token_ttl,
            room_defaults,
            room,
        }
    }

    pub fn from_env() -> Self {
        let client = Self::with_options(
            std::env::var("LIVEKIT_URL").unwrap_or_default(),
            std::env::var("LIVEKIT_API_KEY").unwrap_or_default(),
            std::env::var("LIVEKIT_API_SECRET").unwrap_or_default(),
            Duration::from_secs(env_u64("LIVEKIT_TOKEN_TTL_SECONDS", DEFAULT_TOKEN_TTL_SECONDS)),
            RoomOptions {
                max_participants: env_u32("LIVEKIT_MAX_PARTICIPANTS", DEFAULT_MAX_PARTICIPANTS),
                empty_timeout_s: env_u32(
                    "LIVEKIT_EMPTY_TIMEOUT_SECONDS",
                    DEFAULT_EMPTY_TIMEOUT_SECONDS,
                ),
                departure_timeout_s: env_u32(
                    "LIVEKIT_DEPARTURE_TIMEOUT_SECONDS",
                    DEFAULT_DEPARTURE_TIMEOUT_SECONDS,
                ),
            },
        );

        if client.is_mock() && std::env::var("APP_ENV").as_deref() == Ok("production") {
            tracing::error!(
                "LIVEKIT_API_KEY/LIVEKIT_API_SECRET are unset in production — \
                 video consultations will hand out fake tokens"
            );
        }
        client
    }

    pub fn is_mock(&self) -> bool {
        self.room.is_none()
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    pub fn token_ttl(&self) -> Duration {
        self.token_ttl
    }

    pub fn room_defaults(&self) -> RoomOptions {
        self.room_defaults
    }

    /// Mint a join token. Pure CPU — no network call, so this is safe anywhere.
    pub fn mint_token(
        &self,
        identity: &str,
        display_name: &str,
        attributes: &HashMap<String, String>,
        grants: &TokenGrants,
        ttl: Duration,
    ) -> Result<MintedToken, LiveKitError> {
        let expires_at = Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(DEFAULT_TOKEN_TTL_SECONDS as i64));

        if self.is_mock() {
            return Ok(MintedToken {
                token: format!("mock.{}.{}", grants.room, identity),
                identity: identity.to_string(),
                expires_at,
            });
        }

        let token = AccessToken::with_api_key(&self.api_key, &self.api_secret)
            .with_identity(identity)
            .with_name(display_name)
            .with_ttl(ttl)
            .with_attributes(attributes.clone())
            .with_grants(grants.to_video_grants())
            .to_jwt()
            .map_err(|e| LiveKitError::Token(e.to_string()))?;

        Ok(MintedToken {
            token,
            identity: identity.to_string(),
            expires_at,
        })
    }

    /// Idempotent: LiveKit returns the existing room when the name is taken.
    /// Returns the room SID when LiveKit gave us one.
    pub async fn ensure_room(
        &self,
        room_name: &str,
        opts: RoomOptions,
    ) -> Result<Option<String>, LiveKitError> {
        let Some(room) = self.room.as_ref() else {
            tracing::debug!("LiveKit mock mode: skipping create_room for {room_name}");
            return Ok(None);
        };

        let created = room
            .create_room(
                room_name,
                CreateRoomOptions {
                    empty_timeout: opts.empty_timeout_s,
                    departure_timeout: opts.departure_timeout_s,
                    max_participants: opts.max_participants,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| LiveKitError::Service(e.to_string()))?;

        Ok((!created.sid.is_empty()).then_some(created.sid))
    }

    pub async fn list_participants(
        &self,
        room_name: &str,
    ) -> Result<Vec<LiveParticipant>, LiveKitError> {
        let Some(room) = self.room.as_ref() else {
            return Ok(Vec::new());
        };

        let participants = room
            .list_participants(room_name)
            .await
            .map_err(classify_service_error)?;

        Ok(participants
            .into_iter()
            .map(|p| LiveParticipant {
                identity: p.identity,
                sid: p.sid,
                name: p.name,
                is_publisher: p.is_publisher,
            })
            .collect())
    }

    /// Disconnects everyone still in the room. LiveKit treats deleting an
    /// absent room as success, so this is safe to repeat.
    pub async fn delete_room(&self, room_name: &str) -> Result<(), LiveKitError> {
        let Some(room) = self.room.as_ref() else {
            tracing::debug!("LiveKit mock mode: skipping delete_room for {room_name}");
            return Ok(());
        };

        match room.delete_room(room_name).await.map_err(classify_service_error) {
            // Deleting a room that is already gone is the outcome we wanted.
            Err(LiveKitError::RoomNotFound) | Ok(()) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Verify a webhook delivery. `body` must be the raw request bytes as
    /// received: the JWT's `sha256` claim digests exactly those bytes, so any
    /// re-serialisation makes the digest fail.
    ///
    /// In mock mode the body is parsed unsigned, which is what lets the whole
    /// webhook path be exercised locally with plain `curl`.
    pub fn verify_webhook(
        &self,
        body: &str,
        auth_token: &str,
    ) -> Result<LiveKitWebhookEvent, LiveKitError> {
        let event = if self.is_mock() {
            serde_json::from_str::<proto::WebhookEvent>(body)
                .map_err(|e| LiveKitError::WebhookVerification(e.to_string()))?
        } else {
            WebhookReceiver::new(TokenVerifier::with_api_key(&self.api_key, &self.api_secret))
                .receive(body, auth_token)
                .map_err(|e| LiveKitError::WebhookVerification(e.to_string()))?
        };

        let raw = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        let participant = event.participant.as_ref();

        Ok(LiveKitWebhookEvent {
            created_at: Utc
                .timestamp_opt(event.created_at, 0)
                .single()
                .unwrap_or_else(Utc::now),
            event: event.event,
            event_id: event.id,
            room_name: event.room.as_ref().map(|r| r.name.clone()),
            room_sid: event
                .room
                .as_ref()
                .map(|r| r.sid.clone())
                .filter(|sid| !sid.is_empty()),
            participant_identity: participant.map(|p| p.identity.clone()),
            participant_sid: participant
                .map(|p| p.sid.clone())
                .filter(|sid| !sid.is_empty()),
            participant_name: participant.map(|p| p.name.clone()),
            disconnect_reason: participant.map(|p| p.disconnect_reason),
            raw,
        })
    }
}

/// Separate "the room is not there" from "we could not ask". LiveKit answers a
/// missing room with a Twirp `not_found`, which is a definitive negative rather
/// than a transport failure, and the two lead to opposite decisions upstream.
fn classify_service_error(error: ServiceError) -> LiveKitError {
    match &error {
        ServiceError::Twirp(ServerError::Twirp(code))
            if code.code == ServerErrorCode::NOT_FOUND =>
        {
            LiveKitError::RoomNotFound
        }
        _ => LiveKitError::Service(error.to_string()),
    }
}

/// LiveKit's HTTPS API lives on the same host as the signalling URL — only the
/// scheme differs.
fn api_host_from_ws_url(ws_url: &str) -> String {
    let trimmed = ws_url.trim().trim_end_matches('/');
    match trimmed.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(("ws", rest)) => format!("http://{rest}"),
        _ => trimmed.to_string(),
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    const KEY: &str = "APItestkey";
    const SECRET: &str = "a-test-secret-that-is-long-enough";
    const ROOM: &str = "shift-11111111-1111-1111-1111-111111111111";

    fn real_client() -> LiveKitClient {
        LiveKitClient::new(
            "wss://nexuscare-test.livekit.cloud".to_string(),
            KEY.to_string(),
            SECRET.to_string(),
        )
    }

    fn mock_client() -> LiveKitClient {
        LiveKitClient::new("wss://nexuscare-test.livekit.cloud".to_string(), String::new(), String::new())
    }

    /// Sign a webhook the way LiveKit does: a JWT whose `sha256` claim is the
    /// base64 digest of the exact body bytes.
    fn sign_webhook(body: &str, secret: &str) -> String {
        let digest = Sha256::digest(body.as_bytes());
        AccessToken::with_api_key(KEY, secret)
            .with_ttl(Duration::from_secs(300))
            .with_sha256(&base64::engine::general_purpose::STANDARD.encode(digest))
            .to_jwt()
            .expect("failed to sign webhook token")
    }

    fn webhook_body(event: &str, event_id: &str) -> String {
        format!(
            r#"{{"event":"{event}","id":"{event_id}","createdAt":1755763200,
                 "room":{{"name":"{ROOM}","sid":"RM_test"}},
                 "participant":{{"identity":"u:9d4e","sid":"PA_test","name":"Dr Test"}}}}"#
        )
    }

    // 1. Grants matrix — the authorization surface.

    #[test]
    fn grants_matrix_covers_every_caller() {
        let cases = [
            // (role, mode, can_publish, room_admin)
            (ParticipantRole::Clinician, JoinMode::Participant, true, false),
            (ParticipantRole::HospitalObserver, JoinMode::Participant, true, true),
            (ParticipantRole::HospitalObserver, JoinMode::Observer, false, true),
        ];

        for (role, mode, can_publish, room_admin) in cases {
            let grants = grants_for(role, ROOM, mode);
            let video = grants.to_video_grants();

            assert_eq!(grants.room, ROOM, "{role:?}/{mode:?} got the wrong room");
            assert_eq!(video.room, ROOM, "{role:?}/{mode:?} minted the wrong room");
            assert_eq!(grants.can_publish, can_publish, "{role:?}/{mode:?} can_publish");
            assert_eq!(grants.room_admin, room_admin, "{role:?}/{mode:?} room_admin");
            // An observer must still receive the call.
            assert!(grants.can_subscribe, "{role:?}/{mode:?} must be able to subscribe");
            assert_eq!(grants.can_publish_data, can_publish);

            assert!(video.room_join, "{role:?}/{mode:?} must be able to join");
            assert!(!video.room_create, "{role:?}/{mode:?} must never create rooms");
            assert!(!video.room_record, "{role:?}/{mode:?} must never hold a record grant");
            assert!(!video.hidden, "{role:?}/{mode:?} must never be hidden");
            assert!(!video.recorder);
            assert!(!video.room_list);
            assert!(!video.ingress_admin);
        }
    }

    #[test]
    fn clinician_never_gets_room_admin() {
        for mode in [JoinMode::Participant, JoinMode::Observer] {
            assert!(!grants_for(ParticipantRole::Clinician, ROOM, mode).room_admin);
        }
    }

    // 2. Token round-trip against the real SDK — no network.

    #[test]
    fn minted_token_round_trips_through_the_verifier() {
        let client = real_client();
        let grants = grants_for(ParticipantRole::Clinician, ROOM, JoinMode::Participant);
        let attributes = HashMap::from([("nx_role".to_string(), "clinician".to_string())]);

        let before = Utc::now();
        let minted = client
            .mint_token("u:9d4e", "Dr. Amina Bello", &attributes, &grants, Duration::from_secs(900))
            .expect("minting should succeed with real credentials");

        let claims = TokenVerifier::with_api_key(KEY, SECRET)
            .verify(&minted.token)
            .expect("a freshly minted token must verify");

        assert_eq!(claims.sub, "u:9d4e");
        assert_eq!(claims.name, "Dr. Amina Bello");
        assert_eq!(claims.video.room, ROOM);
        assert!(claims.video.room_join && claims.video.can_publish);
        assert!(!claims.video.room_admin && !claims.video.room_record && !claims.video.hidden);
        assert_eq!(claims.attributes.get("nx_role").map(String::as_str), Some("clinician"));

        // exp ~= now + TTL, allowing a few seconds of test-execution slack.
        let skew = (claims.exp as i64) - (before.timestamp() + 900);
        assert!(skew.abs() <= 5, "exp drifted by {skew}s");
        assert!((minted.expires_at - before).num_seconds() >= 895);
    }

    #[test]
    fn observer_token_cannot_publish() {
        let client = real_client();
        let grants = grants_for(ParticipantRole::HospitalObserver, ROOM, JoinMode::Observer);
        let minted = client
            .mint_token("u:admin", "Admin", &HashMap::new(), &grants, Duration::from_secs(60))
            .unwrap();

        let claims = TokenVerifier::with_api_key(KEY, SECRET).verify(&minted.token).unwrap();
        assert!(!claims.video.can_publish);
        assert!(!claims.video.can_publish_data);
        assert!(claims.video.can_subscribe);
        assert!(claims.video.room_admin);
    }

    // 3. Webhook verification.

    #[test]
    fn verify_webhook_accepts_a_correctly_signed_body() {
        let client = real_client();
        let body = webhook_body("participant_joined", "evt-1");
        let auth = sign_webhook(&body, SECRET);

        let event = client.verify_webhook(&body, &auth).expect("valid signature must verify");
        assert_eq!(event.event, "participant_joined");
        assert_eq!(event.event_id, "evt-1");
        assert_eq!(event.room_name.as_deref(), Some(ROOM));
        assert_eq!(event.participant_identity.as_deref(), Some("u:9d4e"));
        assert_eq!(event.created_at.timestamp(), 1755763200);
    }

    #[test]
    fn verify_webhook_rejects_a_tampered_body() {
        let client = real_client();
        let auth = sign_webhook(&webhook_body("participant_joined", "evt-1"), SECRET);
        let tampered = webhook_body("participant_joined", "evt-2");

        assert!(client.verify_webhook(&tampered, &auth).is_err());
    }

    #[test]
    fn verify_webhook_rejects_a_foreign_secret() {
        let client = real_client();
        let body = webhook_body("participant_joined", "evt-1");
        let auth = sign_webhook(&body, "a-completely-different-secret");

        assert!(client.verify_webhook(&body, &auth).is_err());
    }

    #[test]
    fn verify_webhook_rejects_a_missing_or_malformed_header() {
        let client = real_client();
        let body = webhook_body("participant_joined", "evt-1");

        assert!(client.verify_webhook(&body, "").is_err());
        assert!(client.verify_webhook(&body, "not-a-jwt").is_err());
        assert!(client.verify_webhook(&body, "Bearer ").is_err());
    }

    // 4. Idempotency-key extraction.

    #[test]
    fn idempotency_key_prefers_the_livekit_event_id() {
        let client = mock_client();
        let event = client
            .verify_webhook(&webhook_body("participant_joined", "evt-42"), "")
            .unwrap();
        assert_eq!(event.idempotency_key(), "evt-42");
    }

    #[test]
    fn idempotency_key_falls_back_to_a_synthetic_key() {
        let client = mock_client();
        let event = client
            .verify_webhook(&webhook_body("participant_joined", ""), "")
            .unwrap();
        assert_eq!(
            event.idempotency_key(),
            format!("participant_joined:{ROOM}:u:9d4e:1755763200")
        );
    }

    // 5. Mock mode.

    #[tokio::test]
    async fn mock_mode_never_touches_the_network() {
        let client = mock_client();
        assert!(client.is_mock());

        let grants = grants_for(ParticipantRole::Clinician, ROOM, JoinMode::Participant);
        let minted = client
            .mint_token("u:9d4e", "Dr Test", &HashMap::new(), &grants, Duration::from_secs(900))
            .expect("mock minting never fails");
        assert_eq!(minted.token, format!("mock.{ROOM}.u:9d4e"));

        assert_eq!(client.ensure_room(ROOM, RoomOptions::default()).await.unwrap(), None);
        assert!(client.delete_room(ROOM).await.is_ok());
        assert!(client.list_participants(ROOM).await.unwrap().is_empty());

        // Unsigned bodies are accepted, which is what makes the curl loop work.
        let event = client.verify_webhook(&webhook_body("room_started", "evt-9"), "").unwrap();
        assert_eq!(event.event, "room_started");
    }

    #[test]
    fn a_configured_client_is_not_mock() {
        assert!(!real_client().is_mock());
    }

    #[test]
    fn api_host_swaps_the_scheme() {
        assert_eq!(
            api_host_from_ws_url("wss://nexuscare.livekit.cloud"),
            "https://nexuscare.livekit.cloud"
        );
        assert_eq!(api_host_from_ws_url("ws://localhost:7880/"), "http://localhost:7880");
        assert_eq!(api_host_from_ws_url(""), "");
    }
}

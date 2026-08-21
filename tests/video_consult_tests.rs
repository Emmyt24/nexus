//! Integration tests for LiveKit video consultations. Requires a reachable
//! Postgres — set TEST_DATABASE_URL (falls back to a local `nexuscare_test`
//! database). Skips (prints a notice and returns early) rather than failing the
//! suite if no database is reachable, consistent with this repo not having
//! CI-provisioned Postgres today.
//!
//! `LiveKitClient` runs in mock mode throughout, so nothing here touches the
//! network: tokens are fake and webhook bodies are accepted unsigned. The HTTP
//! layer is never exercised — services are constructed directly, as everywhere
//! else in this suite.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nexuscare_backend::models::user::{Claims, UserRole};
use nexuscare_backend::models::video_session::{
    JoinConsultRequest, JoinMode, ParticipantRole, VideoSessionStatus,
};
use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::shift::ShiftRepository;
use nexuscare_backend::repositories::video_session::VideoSessionRepository;
use nexuscare_backend::repositories::wallet::WalletRepository;
use nexuscare_backend::repositories::EmailOutboxRepository;
use nexuscare_backend::services::email_outbox_service::EmailOutboxService;
use nexuscare_backend::services::fcm::FcmClient;
use nexuscare_backend::services::livekit::LiveKitClient;
use nexuscare_backend::services::notification_service::NotificationService;
use nexuscare_backend::services::push_service::PushService;
use nexuscare_backend::services::safehaven::SafeHavenClient;
use nexuscare_backend::services::shift_service::ShiftService;
use nexuscare_backend::services::video_service::{
    room_name_for_shift, VideoService, VideoServiceError, WebhookOutcome,
};
use nexuscare_backend::services::wallet_service::WalletService;
use sqlx::PgPool;
use uuid::Uuid;

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ndii@localhost:5432/nexuscare_test".to_string());

    let pool = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIPPED: no test database reachable at {url}: {e}");
            return None;
        }
    };

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations against test database");

    Some(pool)
}

/// Wires a `VideoService` over a mock LiveKit client. `virtual_clockin_enabled`
/// is explicit so the clock-in branch can be exercised without touching the
/// process environment.
fn video_service(pool: &PgPool, virtual_clockin_enabled: bool) -> Arc<VideoService> {
    let shift_repo = Arc::new(ShiftRepository::new(pool.clone()));
    let notification_service = Arc::new(NotificationService::new());
    let email_outbox = Arc::new(EmailOutboxService::new(
        Arc::new(EmailOutboxRepository::new(pool.clone())),
        notification_service.clone(),
    ));
    let wallet_service = Arc::new(WalletService::new(
        Arc::new(WalletRepository::new(pool.clone())),
        Arc::new(SafeHavenClient::from_env()),
        pool.clone(),
    ));
    let push = Arc::new(PushService::new(
        Arc::new(NotificationRepository::new(pool.clone())),
        Arc::new(FcmClient::from_env()),
    ));
    let shift_service = Arc::new(ShiftService::new(
        shift_repo.clone(),
        pool.clone(),
        notification_service,
        email_outbox,
        wallet_service,
        push.clone(),
    ));

    Arc::new(VideoService::with_virtual_clockin(
        Arc::new(VideoSessionRepository::new(pool.clone())),
        shift_repo,
        shift_service,
        // Empty credentials == mock mode.
        Arc::new(LiveKitClient::new(
            "wss://mock.livekit.test".to_string(),
            String::new(),
            String::new(),
        )),
        push,
        virtual_clockin_enabled,
    ))
}

/// A hospital, its admin, a worker with a clinician profile, and one shift.
struct Fixture {
    hospital_id: Uuid,
    admin_user_id: Uuid,
    worker_user_id: Uuid,
    clinician_id: Uuid,
    shift_id: Uuid,
}

impl Fixture {
    fn worker_claims(&self) -> Claims {
        claims(self.worker_user_id, UserRole::HealthWorker, None)
    }

    fn admin_claims(&self) -> Claims {
        claims(
            self.admin_user_id,
            UserRole::HospitalAdmin,
            Some(self.hospital_id),
        )
    }

    fn clinician_identity(&self) -> String {
        format!("u:{}", self.worker_user_id)
    }

    fn admin_identity(&self) -> String {
        format!("u:{}", self.admin_user_id)
    }

    fn room_name(&self) -> String {
        room_name_for_shift(self.shift_id)
    }
}

fn claims(user_id: Uuid, role: UserRole, hospital_id: Option<Uuid>) -> Claims {
    Claims {
        sub: user_id.to_string(),
        email: format!("{user_id}@example.test"),
        role,
        hospital_id: hospital_id.map(|id| id.to_string()),
        exp: (Utc::now() + Duration::hours(1)).timestamp() as usize,
        iat: Utc::now().timestamp() as usize,
    }
}

async fn seed_hospital(pool: &PgPool) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO hospitals (name, registration_number, email, address, phone_number)
        VALUES ($1, $2, $3, 'Test Address', '08000000000')
        RETURNING id
        "#,
    )
    .bind(format!("Test Hospital {}", Uuid::new_v4()))
    .bind(format!("RC-{}", &Uuid::new_v4().to_string()[..8]))
    .bind(format!("{}@example.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .expect("failed to seed hospital")
}

async fn seed_user(pool: &PgPool, role: &str, hospital_id: Option<Uuid>) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO users (email, first_name, last_name, password_hash, role, hospital_id)
        VALUES ($1, 'Test', 'User', 'not-a-real-hash', $2::user_role, $3)
        RETURNING id
        "#,
    )
    .bind(format!("{}@example.test", Uuid::new_v4()))
    .bind(role)
    .bind(hospital_id)
    .fetch_one(pool)
    .await
    .expect("failed to seed user")
}

async fn seed_clinician(pool: &PgPool, user_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO clinicians (user_id, first_name, last_name, specialty, role_title)
        VALUES ($1, 'Amina', 'Bello', 'emergency_medicine', 'Emergency Doctor')
        RETURNING id
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("failed to seed clinician")
}

/// A shift owned by `hospital_id`, assigned to `clinician_id` if given, with an
/// accepted `shift_assignments` row so the worker really is the booked one.
async fn seed_shift(
    pool: &PgPool,
    hospital_id: Uuid,
    created_by: Uuid,
    clinician_id: Option<Uuid>,
    shift_type: &str,
    status: &str,
    scheduled_start: DateTime<Utc>,
) -> Uuid {
    let shift_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, shift_type, status,
            scheduled_start, duration_hours, scheduled_end,
            assigned_clinician_id, pay_type, rate_kobo_per_hour,
            grand_total_kobo, created_by
        )
        VALUES ($1, 'doctor', 'Emergency Doctor', $2::shift_type, $3::shift_status,
                $4, 4, $4 + INTERVAL '4 hours',
                $5, 'hourly_rate', 800000, 3200000, $6)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(shift_type)
    .bind(status)
    .bind(scheduled_start)
    .bind(clinician_id)
    .bind(created_by)
    .fetch_one(pool)
    .await
    .expect("failed to seed shift");

    if let Some(clinician_id) = clinician_id {
        sqlx::query(
            r#"
            INSERT INTO shift_assignments (shift_id, clinician_id, status, expires_at, responded_at)
            VALUES ($1, $2, 'accepted', NOW() + INTERVAL '1 day', NOW())
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .execute(pool)
        .await
        .expect("failed to seed shift assignment");
    }

    shift_id
}

/// The common case: a virtual shift starting now, assigned and accepted.
async fn seed_fixture(pool: &PgPool) -> Fixture {
    seed_fixture_with(pool, "virtual", "assigned", Utc::now()).await
}

async fn seed_fixture_with(
    pool: &PgPool,
    shift_type: &str,
    status: &str,
    scheduled_start: DateTime<Utc>,
) -> Fixture {
    let hospital_id = seed_hospital(pool).await;
    let admin_user_id = seed_user(pool, "hospital_admin", Some(hospital_id)).await;
    let worker_user_id = seed_user(pool, "health_worker", None).await;
    let clinician_id = seed_clinician(pool, worker_user_id).await;
    let shift_id = seed_shift(
        pool,
        hospital_id,
        admin_user_id,
        Some(clinician_id),
        shift_type,
        status,
        scheduled_start,
    )
    .await;

    Fixture {
        hospital_id,
        admin_user_id,
        worker_user_id,
        clinician_id,
        shift_id,
    }
}

/// A LiveKit webhook body, exactly as the provider sends it. Mock mode accepts
/// it unsigned, which is what makes this loop cheap.
fn webhook_body(event: &str, event_id: &str, room: &str, identity: Option<&str>) -> String {
    let participant = identity
        .map(|id| {
            format!(
                r#","participant":{{"identity":"{id}","sid":"PA_{event_id}","name":"Dr Test"}}"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"{{"event":"{event}","id":"{event_id}","createdAt":{},"room":{{"name":"{room}","sid":"RM_test"}}{participant}}}"#,
        Utc::now().timestamp()
    )
}

async fn deliver(
    service: &VideoService,
    event: &str,
    event_id: &str,
    room: &str,
    identity: Option<&str>,
) -> WebhookOutcome {
    let body = webhook_body(event, event_id, room, identity);
    let parsed = service
        .verify_webhook(&body, "")
        .expect("mock mode accepts unsigned bodies");
    service
        .process_webhook_event(parsed)
        .await
        .expect("webhook processing should not fail")
}

async fn count_sessions(pool: &PgPool, shift_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM video_sessions WHERE shift_id = $1")
        .bind(shift_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn attendance(
    pool: &PgPool,
    shift_id: Uuid,
) -> Option<(Option<DateTime<Utc>>, Option<String>, Option<i32>)> {
    sqlx::query_as::<_, (Option<DateTime<Utc>>, Option<String>, Option<i32>)>(
        "SELECT clockin_at, clockin_method::text, late_minutes
           FROM shift_attendance WHERE shift_id = $1",
    )
    .bind(shift_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn shift_status(pool: &PgPool, shift_id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM shifts WHERE id = $1")
        .bind(shift_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// 1 — the session is seeded once and reused.

#[tokio::test]
async fn issuing_two_tokens_creates_one_session() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    for _ in 0..2 {
        service
            .issue_join_token(
                fixture.shift_id,
                &fixture.worker_claims(),
                JoinConsultRequest::default(),
            )
            .await
            .expect("the assigned clinician may join");
    }

    assert_eq!(count_sessions(&pool, fixture.shift_id).await, 1);
}

// 2 — the assigned clinician gets a publishing token, and a re-issue bumps the
// count rather than creating a second participant row.

#[tokio::test]
async fn the_assigned_clinician_gets_a_publishing_token() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    let first = service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .expect("the assigned clinician may join");

    assert_eq!(first.participant_role, ParticipantRole::Clinician);
    assert_eq!(first.mode, JoinMode::Participant);
    assert!(first.can_publish && first.can_subscribe);
    assert_eq!(first.room_name, fixture.room_name());
    assert!(first.mock, "the test client has no credentials");

    let issue_count: i32 = sqlx::query_scalar(
        "SELECT token_issue_count FROM video_session_participants
          WHERE session_id = $1 AND identity = $2",
    )
    .bind(first.session_id)
    .bind(fixture.clinician_identity())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(issue_count, 1);

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let (issue_count, rows): (i32, i64) = sqlx::query_as(
        "SELECT token_issue_count, COUNT(*) OVER () FROM video_session_participants
          WHERE session_id = $1 AND identity = $2",
    )
    .bind(first.session_id)
    .bind(fixture.clinician_identity())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(issue_count, 2, "a re-issue bumps the count");
    assert_eq!(rows, 1, "and does not create a second participant row");
}

// 3 — having applied, been offered, or declined confers no claim on the room.

#[tokio::test]
async fn another_health_worker_is_refused() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    let stranger_user_id = seed_user(&pool, "health_worker", None).await;
    seed_clinician(&pool, stranger_user_id).await;

    let error = service
        .issue_join_token(
            fixture.shift_id,
            &claims(stranger_user_id, UserRole::HealthWorker, None),
            JoinConsultRequest::default(),
        )
        .await
        .expect_err("an unassigned worker has no claim on the room");

    assert!(matches!(error, VideoServiceError::NotAuthorized));
}

// 4 — the in-handler hospital check is the tenant boundary; there is no RLS.

#[tokio::test]
async fn a_foreign_hospital_admin_is_refused() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    let other_hospital = seed_hospital(&pool).await;
    let other_admin = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;

    let error = service
        .issue_join_token(
            fixture.shift_id,
            &claims(other_admin, UserRole::HospitalAdmin, Some(other_hospital)),
            JoinConsultRequest::default(),
        )
        .await
        .expect_err("another hospital's admin must not reach the room");

    assert!(matches!(error, VideoServiceError::NotAuthorized));
}

// 5, 6, 7 — the preconditions everyone is checked against.

#[tokio::test]
async fn an_in_person_shift_has_no_consultation() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture_with(&pool, "in_person", "assigned", Utc::now()).await;

    let error = service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .expect_err("in-person shifts have no room");

    assert!(matches!(error, VideoServiceError::NotVirtualShift));
}

#[tokio::test]
async fn a_completed_shift_cannot_be_joined() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture_with(&pool, "virtual", "completed", Utc::now()).await;

    let error = service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .expect_err("a finished shift is not joinable");

    assert!(matches!(error, VideoServiceError::ShiftNotJoinable(_)));
}

/// The window matches `clock_in`'s ±60 minutes on purpose: a token minted
/// outside it would guarantee a join webhook that cannot clock anyone in.
#[tokio::test]
async fn joining_three_hours_early_is_refused() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture =
        seed_fixture_with(&pool, "virtual", "assigned", Utc::now() + Duration::hours(3)).await;

    let error = service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .expect_err("three hours early is outside the consultation window");

    assert!(matches!(error, VideoServiceError::OutsideWindow));
}

/// A worker asking to observe is a 403 rather than a silent downgrade.
#[tokio::test]
async fn a_worker_cannot_request_observer_mode() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    let error = service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest {
                mode: Some(JoinMode::Observer),
                device_label: None,
            },
        )
        .await
        .expect_err("observer mode is a hospital affordance");

    assert!(matches!(error, VideoServiceError::NotAuthorized));
}

// 8 — joining is clocking in.

#[tokio::test]
async fn the_clinician_joining_records_a_virtual_clock_in() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let outcome = deliver(
        &service,
        "participant_joined",
        &Uuid::new_v4().to_string(),
        &fixture.room_name(),
        Some(&fixture.clinician_identity()),
    )
    .await;
    assert_eq!(outcome, WebhookOutcome::Processed);

    let (clockin_at, method, _late) = attendance(&pool, fixture.shift_id)
        .await
        .expect("a clock-in should have been recorded");
    assert!(clockin_at.is_some());
    assert_eq!(method.as_deref(), Some("virtual"));
    assert_eq!(shift_status(&pool, fixture.shift_id).await, "in_progress");
}

// 9 — LiveKit retries; the dedupe row swallows the replay.

#[tokio::test]
async fn replaying_the_same_event_id_is_deduped() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;
    let event_id = Uuid::new_v4().to_string();

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let room = fixture.room_name();
    let identity = fixture.clinician_identity();
    assert_eq!(
        deliver(&service, "participant_joined", &event_id, &room, Some(&identity)).await,
        WebhookOutcome::Processed
    );
    assert_eq!(
        deliver(&service, "participant_joined", &event_id, &room, Some(&identity)).await,
        WebhookOutcome::AlreadySeen
    );

    let attendance_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM shift_attendance WHERE shift_id = $1")
            .bind(fixture.shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attendance_rows, 1);
}

// 10 — the regression that matters most: a rejoin must not move clockin_at.

#[tokio::test]
async fn a_rejoin_does_not_move_the_clock_in() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;
    let room = fixture.room_name();
    let identity = fixture.clinician_identity();

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    deliver(&service, "participant_joined", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;
    let (first_clockin, _, first_late) = attendance(&pool, fixture.shift_id).await.unwrap();
    assert!(first_clockin.is_some());

    // The worker drops off and comes back: a new event id, same identity.
    deliver(&service, "participant_left", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;
    // A fresh token, as the client would request on reconnect.
    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    deliver(&service, "participant_joined", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;

    let (second_clockin, _, second_late) = attendance(&pool, fixture.shift_id).await.unwrap();
    assert_eq!(
        first_clockin, second_clockin,
        "a rejoin must not reset clockin_at — it would shorten the worker's paid hours"
    );
    assert_eq!(first_late, second_late, "late_minutes must not be recomputed");
}

// 11 — the hospital admin is an observer, not the worker.

#[tokio::test]
async fn the_hospital_admin_joining_records_no_attendance() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.admin_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .expect("the owning hospital's admin may join");

    deliver(
        &service,
        "participant_joined",
        &Uuid::new_v4().to_string(),
        &fixture.room_name(),
        Some(&fixture.admin_identity()),
    )
    .await;

    assert!(attendance(&pool, fixture.shift_id).await.is_none());
}

// 12, 13, 14 — departures touch the video tables and nothing else.

#[tokio::test]
async fn leaving_does_not_clock_the_worker_out() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;
    let room = fixture.room_name();
    let identity = fixture.clinician_identity();

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    deliver(&service, "participant_joined", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;
    let (clockin_before, _, _) = attendance(&pool, fixture.shift_id).await.unwrap();

    deliver(&service, "participant_left", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;

    let left_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT p.left_at FROM video_session_participants p
           JOIN video_sessions s ON s.id = p.session_id
          WHERE s.shift_id = $1 AND p.identity = $2",
    )
    .bind(fixture.shift_id)
    .bind(&identity)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(left_at.is_some(), "the departure is recorded");

    let (clockin_after, _, _) = attendance(&pool, fixture.shift_id).await.unwrap();
    assert_eq!(clockin_before, clockin_after);
    assert_eq!(
        shift_status(&pool, fixture.shift_id).await,
        "in_progress",
        "a dropped connection must never complete a shift"
    );
    let clockout: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT clockout_at FROM shift_attendance WHERE shift_id = $1")
            .bind(fixture.shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(clockout.is_none());
}

/// LiveKit guarantees neither ordering nor delivery.
#[tokio::test]
async fn a_left_before_a_joined_does_not_clobber_the_join() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;
    let room = fixture.room_name();
    let identity = fixture.clinician_identity();

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    deliver(&service, "participant_left", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;
    deliver(&service, "participant_joined", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;

    let (joined_at, left_at): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT p.joined_at, p.left_at FROM video_session_participants p
           JOIN video_sessions s ON s.id = p.session_id
          WHERE s.shift_id = $1 AND p.identity = $2",
    )
    .bind(fixture.shift_id)
    .bind(&identity)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert!(joined_at.is_some(), "the join is still recorded");
    assert!(left_at.is_none(), "a stale departure is cleared by the join");
}

#[tokio::test]
async fn a_departure_for_an_unknown_identity_is_a_no_op() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let outcome = deliver(
        &service,
        "participant_left",
        &Uuid::new_v4().to_string(),
        &fixture.room_name(),
        Some(&format!("u:{}", Uuid::new_v4())),
    )
    .await;

    assert_eq!(outcome, WebhookOutcome::Processed, "not an error");
}

// 15, 16 — the room ending is not the shift ending.

#[tokio::test]
async fn room_finished_ends_the_session_without_clocking_out() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;
    let room = fixture.room_name();
    let identity = fixture.clinician_identity();

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    deliver(&service, "participant_joined", &Uuid::new_v4().to_string(), &room, Some(&identity))
        .await;

    deliver(&service, "room_finished", &Uuid::new_v4().to_string(), &room, None).await;

    let (status, ended_at, open_participants): (String, Option<DateTime<Utc>>, i64) =
        sqlx::query_as(
            "SELECT s.status, s.ended_at,
                    (SELECT COUNT(*) FROM video_session_participants p
                      WHERE p.session_id = s.id AND p.left_at IS NULL)
               FROM video_sessions s WHERE s.shift_id = $1",
        )
        .bind(fixture.shift_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(status, "ended");
    assert!(ended_at.is_some());
    assert_eq!(open_participants, 0, "every participant is closed out");

    let clockout: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT clockout_at FROM shift_attendance WHERE shift_id = $1")
            .bind(fixture.shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(clockout.is_none(), "ending the room must not clock the worker out");
    assert_eq!(shift_status(&pool, fixture.shift_id).await, "in_progress");
}

#[tokio::test]
async fn a_second_room_finished_does_not_move_ended_at() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;
    let room = fixture.room_name();

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    deliver(&service, "room_finished", &Uuid::new_v4().to_string(), &room, None).await;
    let first: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT ended_at FROM video_sessions WHERE shift_id = $1")
            .bind(fixture.shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    deliver(&service, "room_finished", &Uuid::new_v4().to_string(), &room, None).await;
    let second: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT ended_at FROM video_sessions WHERE shift_id = $1")
            .bind(fixture.shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(first, second);
}

// 17 — a join we could never clock in is audited, not acted on.

#[tokio::test]
async fn joining_ninety_minutes_early_records_no_attendance() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;
    let room = fixture.room_name();
    let identity = fixture.clinician_identity();

    // Mint the token inside the window, then move the shift out of it, which is
    // how a slow pre-join screen reaches the webhook too late.
    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE shifts SET scheduled_start = NOW() + INTERVAL '90 minutes',
                           scheduled_end   = NOW() + INTERVAL '5 hours' WHERE id = $1",
    )
    .bind(fixture.shift_id)
    .execute(&pool)
    .await
    .unwrap();

    let outcome =
        deliver(&service, "participant_joined", &Uuid::new_v4().to_string(), &room, Some(&identity))
            .await;

    assert_eq!(outcome, WebhookOutcome::Processed, "the webhook still succeeds");
    assert!(attendance(&pool, fixture.shift_id).await.is_none());

    let audited: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM video_session_events e
               JOIN video_sessions s ON s.id = e.session_id
              WHERE s.shift_id = $1 AND e.event_type = 'clockin_skipped:outside_window')",
    )
    .bind(fixture.shift_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(audited, "the skip is on the audit trail");
}

// 18 — the pre-join screen must not 404.

#[tokio::test]
async fn get_consult_before_anyone_joins_is_pending_not_missing() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let view = service
        .get_session(fixture.shift_id, &fixture.worker_claims())
        .await
        .expect("a session exists as soon as a token has been issued");

    assert_eq!(view.status, VideoSessionStatus::Pending);
    assert!(!view.clock_in_recorded);
    assert!(view.participants.iter().all(|p| !p.connected));
    assert!(!view.recording.enabled);
}

// 19, 20 — ending is the owning hospital's call, and it is idempotent.

#[tokio::test]
async fn a_foreign_hospital_admin_cannot_end_the_call() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let other_hospital = seed_hospital(&pool).await;
    let other_admin = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;

    let error = service
        .end_session(
            fixture.shift_id,
            &claims(other_admin, UserRole::HospitalAdmin, Some(other_hospital)),
            None,
        )
        .await
        .expect_err("only the owning hospital may end the call");

    assert!(matches!(error, VideoServiceError::NotAuthorized));
}

#[tokio::test]
async fn ending_twice_keeps_the_original_ended_at() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let first = service
        .end_session(
            fixture.shift_id,
            &fixture.admin_claims(),
            Some("Consultation complete".to_string()),
        )
        .await
        .unwrap();
    let second = service
        .end_session(fixture.shift_id, &fixture.admin_claims(), None)
        .await
        .unwrap();

    assert_eq!(first.status, VideoSessionStatus::Ended);
    assert_eq!(first.ended_reason.as_deref(), Some("ended_by_hospital"));
    assert_eq!(second.ended_reason.as_deref(), Some("ended_by_hospital"));
    assert_eq!(first.ended_at, second.ended_at);
    assert!(first.clock_out_required);
}

/// An ended session cannot be rejoined.
#[tokio::test]
async fn a_token_is_refused_after_the_call_has_ended() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    service
        .end_session(fixture.shift_id, &fixture.admin_claims(), None)
        .await
        .unwrap();

    let error = service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .expect_err("an ended consultation cannot be rejoined");

    assert!(matches!(error, VideoServiceError::SessionEnded));
}

/// The flag is a kill switch: the receiver still records the audit trail.
#[tokio::test]
async fn the_clockin_flag_gates_the_attendance_write() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    deliver(
        &service,
        "participant_joined",
        &Uuid::new_v4().to_string(),
        &fixture.room_name(),
        Some(&fixture.clinician_identity()),
    )
    .await;

    assert!(attendance(&pool, fixture.shift_id).await.is_none());
    let audited: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM video_session_events e
               JOIN video_sessions s ON s.id = e.session_id
              WHERE s.shift_id = $1 AND e.event_type = 'clockin_skipped:flag_off')",
    )
    .bind(fixture.shift_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(audited);
}

/// An event for a room nobody has a token for is acknowledged, not retried.
#[tokio::test]
async fn an_event_for_an_unknown_room_is_acknowledged() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);

    let outcome = deliver(
        &service,
        "participant_joined",
        &Uuid::new_v4().to_string(),
        &room_name_for_shift(Uuid::new_v4()),
        Some(&format!("u:{}", Uuid::new_v4())),
    )
    .await;

    assert_eq!(outcome, WebhookOutcome::UnknownRoom);
}

/// Reporting a departure over HTTP is idempotent and never blocks the UI.
#[tokio::test]
async fn leave_is_idempotent() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let first = service
        .leave_session(fixture.shift_id, &fixture.worker_claims())
        .await
        .unwrap();
    let second = service
        .leave_session(fixture.shift_id, &fixture.worker_claims())
        .await
        .unwrap();

    assert_eq!(first.identity, fixture.clinician_identity());
    assert_eq!(first.session_id, second.session_id);
    assert_eq!(second.remaining_participants, 0);
}

/// Platform staff get metadata, never a token.
#[tokio::test]
async fn platform_admins_can_read_but_never_join() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;
    let ops_admin = claims(seed_user(&pool, "operations_admin", None).await, UserRole::OperationsAdmin, None);

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    service
        .get_session(fixture.shift_id, &ops_admin)
        .await
        .expect("support needs to see who joined and when");

    let error = service
        .issue_join_token(fixture.shift_id, &ops_admin, JoinConsultRequest::default())
        .await
        .expect_err("NDPR gives platform staff no basis to sit inside a consultation");
    assert!(matches!(error, VideoServiceError::NotAuthorized));

    // Silence the unused-field warning for fixtures that only assert on ids.
    let _ = fixture.clinician_id;
}

// The reconciler's sweep queries. The LiveKit-facing branches need a real
// server and are covered by hand; these cover the SQL that decides what gets
// swept at all.

/// A token minted long enough ago with no join is exactly what branch 1 looks
/// for — but only while the consultation could still be recovered.
#[tokio::test]
async fn the_join_sweep_picks_up_a_token_that_never_produced_a_join() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;
    let repo = VideoSessionRepository::new(pool.clone());

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    // Nothing is due yet — the grace period has not elapsed.
    let due = repo
        .sessions_awaiting_join(Utc::now() - Duration::minutes(5))
        .await
        .unwrap();
    assert!(
        !due.iter().any(|s| s.shift_id == Some(fixture.shift_id)),
        "a token issued seconds ago is not yet overdue"
    );

    // Age the token past the grace period.
    sqlx::query(
        "UPDATE video_session_participants SET token_issued_at = NOW() - INTERVAL '10 minutes'
          WHERE session_id IN (SELECT id FROM video_sessions WHERE shift_id = $1)",
    )
    .bind(fixture.shift_id)
    .execute(&pool)
    .await
    .unwrap();

    let due = repo
        .sessions_awaiting_join(Utc::now() - Duration::minutes(5))
        .await
        .unwrap();
    assert!(
        due.iter().any(|s| s.shift_id == Some(fixture.shift_id)),
        "an overdue join should be swept"
    );
}

/// Once the consultation window has closed, recovering the join could not clock
/// anyone in — so the row stops being swept rather than costing a LiveKit call
/// every tick forever.
#[tokio::test]
async fn the_join_sweep_drops_sessions_past_their_window() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;
    let repo = VideoSessionRepository::new(pool.clone());

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE video_session_participants SET token_issued_at = NOW() - INTERVAL '10 minutes'
          WHERE session_id IN (SELECT id FROM video_sessions WHERE shift_id = $1)",
    )
    .bind(fixture.shift_id)
    .execute(&pool)
    .await
    .unwrap();

    // Move the whole shift into the past, well beyond scheduled_end + 1 hour.
    sqlx::query(
        "UPDATE shifts SET scheduled_start = NOW() - INTERVAL '10 hours',
                           scheduled_end   = NOW() - INTERVAL '6 hours' WHERE id = $1",
    )
    .bind(fixture.shift_id)
    .execute(&pool)
    .await
    .unwrap();

    let due = repo
        .sessions_awaiting_join(Utc::now() - Duration::minutes(5))
        .await
        .unwrap();
    assert!(
        !due.iter().any(|s| s.shift_id == Some(fixture.shift_id)),
        "a session past its consultation window should no longer be swept"
    );
}

/// Branch 2 only ever considers sessions someone actually joined.
///
/// The rows are inserted rather than updated on purpose: `video_sessions` has a
/// `BEFORE UPDATE` trigger that rewrites `updated_at` to NOW(), so an UPDATE
/// cannot backdate it. That trigger is also what makes the production query
/// correct — a session anything touches stops looking stale.
#[tokio::test]
async fn the_stale_sweep_only_considers_active_sessions() {
    let Some(pool) = test_pool().await else { return };
    let repo = VideoSessionRepository::new(pool.clone());
    let fixture = seed_fixture(&pool).await;

    let insert_aged_session = |status: &'static str| {
        let pool = pool.clone();
        let hospital_id = fixture.hospital_id;
        async move {
            sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO video_sessions
                    (hospital_id, room_name, status, started_at, updated_at, created_at)
                VALUES ($1, $2, $3, NOW() - INTERVAL '3 hours',
                        NOW() - INTERVAL '2 hours', NOW() - INTERVAL '3 hours')
                RETURNING id
                "#,
            )
            .bind(hospital_id)
            .bind(format!("shift-{}", Uuid::new_v4()))
            .bind(status)
            .fetch_one(&pool)
            .await
        }
    };

    let pending_id = insert_aged_session("pending").await.unwrap();
    let active_id = insert_aged_session("active").await.unwrap();

    let stale = repo
        .stale_active_sessions(Utc::now() - Duration::minutes(30))
        .await
        .unwrap();
    let ids: Vec<Uuid> = stale.iter().map(|s| s.id).collect();

    assert!(
        ids.contains(&active_id),
        "an active session nothing has touched for hours is a candidate"
    );
    assert!(
        !ids.contains(&pending_id),
        "a pending session means nobody ever joined — there is no room to close"
    );
}

/// An ended session whose worker is still clocked in is what branch 3 chases.
#[tokio::test]
async fn the_clockout_sweep_finds_an_ended_session_with_an_open_attendance() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, true);
    let fixture = seed_fixture(&pool).await;
    let repo = VideoSessionRepository::new(pool.clone());

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();
    deliver(
        &service,
        "participant_joined",
        &Uuid::new_v4().to_string(),
        &fixture.room_name(),
        Some(&fixture.clinician_identity()),
    )
    .await;
    deliver(
        &service,
        "room_finished",
        &Uuid::new_v4().to_string(),
        &fixture.room_name(),
        None,
    )
    .await;

    sqlx::query(
        "UPDATE video_sessions SET ended_at = NOW() - INTERVAL '30 minutes' WHERE shift_id = $1",
    )
    .bind(fixture.shift_id)
    .execute(&pool)
    .await
    .unwrap();

    let pending = repo
        .ended_sessions_pending_clockout(Utc::now() - Duration::minutes(10))
        .await
        .unwrap();
    let row = pending
        .iter()
        .find(|p| p.shift_id == fixture.shift_id)
        .expect("the ended session should be chased for clock-out");
    assert_eq!(row.clinician_id, fixture.clinician_id);
    assert_eq!(row.room_name, fixture.room_name());
}

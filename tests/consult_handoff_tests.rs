//! Integration tests for companion devices — the "Continue on phone" handoff.
//!
//! Requires a reachable Postgres — set TEST_DATABASE_URL (falls back to a local
//! `nexuscare_test` database). Skips (prints a notice and returns early) rather
//! than failing the suite if no database is reachable, matching
//! `video_consult_tests.rs`.
//!
//! The load-bearing case here is `a_companion_device_does_not_clock_in_twice`:
//! a second device must not produce a second `shift_attendance` row, and must
//! not move `clockin_at`. Both drive payroll.
//!
//! `LiveKitClient` runs in mock mode throughout, so nothing touches the
//! network. The HTTP layer is never exercised — services are constructed
//! directly, as everywhere else in this suite.


use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nexuscare_backend::models::user::{Claims, UserRole};
use nexuscare_backend::models::video_session::{
    JoinConsultRequest, JoinMode, ParticipantRole,
};
use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::patient_record::PatientRecordRepository;
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
    identity_for_device, primary_identity, room_name_for_shift, VideoService, VideoServiceError,
    WebhookOutcome,
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
        Arc::new(PatientRecordRepository::new(pool.clone())),
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


// Helpers specific to handoffs.

async fn participant_rows(pool: &PgPool, shift_id: Uuid) -> Vec<(String, i32, Option<DateTime<Utc>>)> {
    sqlx::query_as::<_, (String, i32, Option<DateTime<Utc>>)>(
        "SELECT p.identity, p.device_ordinal, p.clocked_in_at
           FROM video_session_participants p
           JOIN video_sessions s ON s.id = p.session_id
          WHERE s.shift_id = $1
          ORDER BY p.device_ordinal",
    )
    .bind(shift_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn attendance_row_count(pool: &PgPool, shift_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM shift_attendance WHERE shift_id = $1")
        .bind(shift_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Backdate a live grant so it reads as expired, without waiting 3 minutes.
async fn expire_handoffs(pool: &PgPool, shift_id: Uuid) {
    sqlx::query(
        "UPDATE consult_device_handoffs h
            SET expires_at = NOW() - INTERVAL '1 minute'
          WHERE h.session_id IN (SELECT id FROM video_sessions WHERE shift_id = $1)",
    )
    .bind(shift_id)
    .execute(pool)
    .await
    .unwrap();
}

// 1 — identity derivation.

#[test]
fn device_identities_round_trip_to_the_primary() {
    let user = Uuid::new_v4();
    // Slot 1 keeps the bare identity, so every token minted before companion
    // devices existed still maps to the same participant row.
    assert_eq!(identity_for_device(user, 1), format!("u:{user}"));
    assert_eq!(identity_for_device(user, 2), format!("u:{user}#d2"));
    assert_eq!(primary_identity(&identity_for_device(user, 2)), format!("u:{user}"));
    assert_eq!(primary_identity(&identity_for_device(user, 1)), format!("u:{user}"));
}

// 2 — the happy path: two devices, one person.

#[tokio::test]
async fn a_redeemed_handoff_adds_a_second_participant_row() {
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
        .expect("the assigned clinician may join");

    let handoff = service
        .create_handoff(
            fixture.shift_id,
            &fixture.worker_claims(),
            Default::default(),
        )
        .await
        .expect("the clinician may hand off to their own phone");

    assert_eq!(handoff.device_ordinal, 2, "the first handoff takes slot 2");
    assert!(
        handoff.handoff_url.contains("#c="),
        "the code must ride in the URL fragment, not the query: {}",
        handoff.handoff_url
    );

    let joined = service
        .redeem_handoff(&handoff.code)
        .await
        .expect("a fresh code redeems");

    assert_eq!(joined.identity, identity_for_device(fixture.worker_user_id, 2));
    assert_eq!(joined.session_id, handoff.session_id);

    let rows = participant_rows(&pool, fixture.shift_id).await;
    assert_eq!(rows.len(), 2, "one person, two devices, two rows: {rows:?}");
    assert_eq!(rows[0].1, 1);
    assert_eq!(rows[1].1, 2);
}

// 3 — the payroll regression. This is the test that matters most.

#[tokio::test]
async fn a_companion_device_does_not_clock_in_twice() {
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

    // The laptop joins and is clocked in.
    deliver(
        &service,
        "participant_joined",
        "evt-primary",
        &fixture.room_name(),
        Some(&fixture.clinician_identity()),
    )
    .await;

    let (first_clockin, method, _late) = attendance(&pool, fixture.shift_id)
        .await
        .expect("the laptop join clocks the worker in");
    assert!(first_clockin.is_some());
    assert_eq!(method.as_deref(), Some("virtual"));
    assert_eq!(attendance_row_count(&pool, fixture.shift_id).await, 1);

    // Now the phone.
    let handoff = service
        .create_handoff(
            fixture.shift_id,
            &fixture.worker_claims(),
            Default::default(),
        )
        .await
        .unwrap();
    service.redeem_handoff(&handoff.code).await.unwrap();

    deliver(
        &service,
        "participant_joined",
        "evt-companion",
        &fixture.room_name(),
        Some(&identity_for_device(fixture.worker_user_id, 2)),
    )
    .await;

    // Still exactly one attendance row, and the clock-in has not moved: a
    // second one would pay the worker twice, and a moved one would shorten
    // their paid hours.
    assert_eq!(
        attendance_row_count(&pool, fixture.shift_id).await,
        1,
        "a companion device must not create a second attendance row"
    );
    let (second_clockin, _, _) = attendance(&pool, fixture.shift_id).await.unwrap();
    assert_eq!(
        first_clockin, second_clockin,
        "the phone joining must not move clockin_at"
    );

    // And the slot is held by the primary row, not the phone's.
    let rows = participant_rows(&pool, fixture.shift_id).await;
    assert!(rows[0].2.is_some(), "the primary row holds the clock-in");
    assert!(
        rows[1].2.is_none(),
        "the companion row must not hold a clock-in: {rows:?}"
    );
}

// 4 — one person on two devices is one participant.

#[tokio::test]
async fn two_devices_count_as_one_participant() {
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
    let handoff = service
        .create_handoff(
            fixture.shift_id,
            &fixture.worker_claims(),
            Default::default(),
        )
        .await
        .unwrap();
    service.redeem_handoff(&handoff.code).await.unwrap();

    for (event_id, identity) in [
        ("evt-a", fixture.clinician_identity()),
        ("evt-b", identity_for_device(fixture.worker_user_id, 2)),
    ] {
        deliver(
            &service,
            "participant_joined",
            event_id,
            &fixture.room_name(),
            Some(&identity),
        )
        .await;
    }

    // `remaining_participants` answers "am I alone?", so it must count people.
    let left = service
        .leave_session(fixture.shift_id, &fixture.worker_claims())
        .await
        .unwrap();
    assert_eq!(
        left.remaining_participants, 1,
        "one person on two devices is one participant"
    );
}

// 5 — the code is a credential: single use, short-lived, unguessable.

#[tokio::test]
async fn a_handoff_code_is_single_use() {
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
    let handoff = service
        .create_handoff(
            fixture.shift_id,
            &fixture.worker_claims(),
            Default::default(),
        )
        .await
        .unwrap();

    service
        .redeem_handoff(&handoff.code)
        .await
        .expect("the first redeem succeeds");

    let second = service.redeem_handoff(&handoff.code).await;
    assert!(
        matches!(second, Err(VideoServiceError::NotAuthorized)),
        "a spent code must not redeem again, got {second:?}"
    );
}

#[tokio::test]
async fn an_expired_code_and_an_unknown_code_fail_identically() {
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
    let handoff = service
        .create_handoff(
            fixture.shift_id,
            &fixture.worker_claims(),
            Default::default(),
        )
        .await
        .unwrap();
    expire_handoffs(&pool, fixture.shift_id).await;

    let expired = service.redeem_handoff(&handoff.code).await;
    let unknown = service.redeem_handoff("not-a-real-code").await;

    // Indistinguishable on purpose: otherwise the endpoint reports whether a
    // code ever existed, which is a probe for live sessions.
    assert!(matches!(expired, Err(VideoServiceError::NotAuthorized)));
    assert!(matches!(unknown, Err(VideoServiceError::NotAuthorized)));
}

#[tokio::test]
async fn only_the_hash_of_a_code_is_stored() {
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
    let handoff = service
        .create_handoff(
            fixture.shift_id,
            &fixture.worker_claims(),
            Default::default(),
        )
        .await
        .unwrap();

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT code_hash FROM consult_device_handoffs
          WHERE session_id IN (SELECT id FROM video_sessions WHERE shift_id = $1)",
    )
    .bind(fixture.shift_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0], handoff.code, "the plaintext code must never be stored");
    assert_eq!(stored[0].len(), 64, "a SHA-256 hex digest is 64 chars");
}

// 6 — authorization carries over from the join check.

#[tokio::test]
async fn a_foreign_hospital_admin_cannot_create_a_handoff() {
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
    let intruder = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;
    let result = service
        .create_handoff(
            fixture.shift_id,
            &claims(intruder, UserRole::HospitalAdmin, Some(other_hospital)),
            Default::default(),
        )
        .await;

    assert!(matches!(result, Err(VideoServiceError::NotAuthorized)));
}

#[tokio::test]
async fn an_ended_session_cannot_be_handed_off() {
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

    let result = service
        .create_handoff(
            fixture.shift_id,
            &fixture.worker_claims(),
            Default::default(),
        )
        .await;
    assert!(matches!(result, Err(VideoServiceError::SessionEnded)));
}

// 7 — an observer's phone stays an observer.

#[tokio::test]
async fn an_observer_handoff_cannot_publish() {
    let Some(pool) = test_pool().await else { return };
    let service = video_service(&pool, false);
    let fixture = seed_fixture(&pool).await;

    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.admin_claims(),
            JoinConsultRequest {
                mode: Some(JoinMode::Observer),
                ..Default::default()
            },
        )
        .await
        .expect("a hospital admin may observe");

    let handoff = service
        .create_handoff(
            fixture.shift_id,
            &fixture.admin_claims(),
            Default::default(),
        )
        .await
        .unwrap();
    let joined = service.redeem_handoff(&handoff.code).await.unwrap();

    assert_eq!(joined.participant_role, ParticipantRole::HospitalObserver);
    assert!(
        !joined.can_publish,
        "a phone handed off from an observer seat must not gain publish rights"
    );
}

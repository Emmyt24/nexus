//! Integration tests for the consultation waiting room — patient count during a
//! session, and hospitals adding patients mid-call.
//!
//! Requires a reachable Postgres — set TEST_DATABASE_URL (falls back to a local
//! `nexuscare_test` database). Skips (prints a notice and returns early) rather
//! than failing the suite if no database is reachable, matching
//! `video_consult_tests.rs`.
//!
//! `LiveKitClient` and `MlClient` both run in mock mode, so nothing touches the
//! network and no Python service is needed. The HTTP layer is never exercised —
//! services are constructed directly, as everywhere else in this suite.


use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nexuscare_backend::models::user::{Claims, UserRole};
use nexuscare_backend::models::video_session::{
    JoinConsultRequest,
};
use nexuscare_backend::models::patient::NewPatientRequest;
use nexuscare_backend::models::patient_record::AddPatientToQueueRequest;
use nexuscare_backend::repositories::consultation_note::ConsultationNoteRepository;
use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::patient::PatientRepository;
use nexuscare_backend::repositories::patient_prediction::PatientPredictionRepository;
use nexuscare_backend::repositories::patient_record::PatientRecordRepository;
use nexuscare_backend::services::consultation_note_service::ConsultationNoteService;
use nexuscare_backend::services::ml_client::MlClient;
use nexuscare_backend::services::patient_prediction_service::PatientPredictionService;
use nexuscare_backend::services::patient_record_service::{
    PatientRecordService, PatientRecordServiceError,
};
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
    room_name_for_shift, VideoService, WebhookOutcome,
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


// A PatientRecordService over the same mock-mode stack.

fn patient_record_service(
    pool: &PgPool,
    video: Arc<VideoService>,
) -> Arc<PatientRecordService> {
    let patient_repo = Arc::new(PatientRepository::new(pool.clone()));
    // Empty base URL == mock mode, mirroring SafeHavenClient.
    let ml_client = Arc::new(MlClient::new(String::new()));
    let (tx, _rx) = tokio::sync::broadcast::channel(256);
    let prediction_service = Arc::new(PatientPredictionService::new(
        pool.clone(),
        patient_repo.clone(),
        Arc::new(PatientPredictionRepository::new(pool.clone())),
        ml_client.clone(),
        Arc::new(tx),
    ));
    let notes = Arc::new(ConsultationNoteService::new(
        Arc::new(ConsultationNoteRepository::new(pool.clone())),
        patient_repo.clone(),
        video.clone(),
        ml_client,
    ));

    Arc::new(PatientRecordService::new(
        Arc::new(PatientRecordRepository::new(pool.clone())),
        Arc::new(VideoSessionRepository::new(pool.clone())),
        video,
        notes,
        patient_repo,
        prediction_service,
        pool.clone(),
    ))
}

fn new_patient(name: &str) -> NewPatientRequest {
    serde_json::from_value(serde_json::json!({
        "full_name": name,
        "age": 34.0,
        "symptoms": "fever, headache",
    }))
    .expect("the intake DTO defaults everything but name and age")
}

/// Open a session so the queue has something to attach to.
async fn start_session(service: &VideoService, fixture: &Fixture) {
    service
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .expect("the assigned clinician may join");
}

async fn queue_a_new_patient(
    records: &PatientRecordService,
    fixture: &Fixture,
    name: &str,
) -> Uuid {
    records
        .add_patient_to_queue(
            fixture.shift_id,
            &fixture.admin_claims(),
            AddPatientToQueueRequest {
                patient: Some(new_patient(name)),
                ..Default::default()
            },
        )
        .await
        .expect("the hospital may queue a patient")
        .id
}

// 1 — hospitals add patients during a call session.

#[tokio::test]
async fn a_hospital_can_add_a_patient_mid_session() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    let entry_id = queue_a_new_patient(&records, &fixture, "Ada Queue").await;

    let room = records
        .waiting_room(fixture.shift_id, &fixture.admin_claims())
        .await
        .unwrap();
    assert_eq!(room.waiting, 1);
    assert_eq!(room.total, 1);
    assert_eq!(room.patients.len(), 1);
    assert_eq!(room.patients[0].entry_id, entry_id);
    assert_eq!(room.patients[0].full_name, "Ada Queue");
    assert!(!room.patients[0].has_clinical_note);
    assert!(!room.patients[0].has_handover_note);
}

/// The intake must still go through the ML pipeline — the point of reusing
/// `ingest_patient_in_tx` rather than inserting a patient row directly.
#[tokio::test]
async fn adding_a_patient_queues_an_ml_prediction() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    queue_a_new_patient(&records, &fixture, "Triage Me").await;

    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM patient_predictions pr
           JOIN patients p ON p.id = pr.patient_id
          WHERE p.full_name = 'Triage Me' AND pr.status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending, 1, "the patient must be queued for ML triage");
}

// 2 — the count moves through the states.

#[tokio::test]
async fn the_waiting_count_follows_the_consultation() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    let first = queue_a_new_patient(&records, &fixture, "First Up").await;
    queue_a_new_patient(&records, &fixture, "Second Up").await;

    let room = records
        .waiting_room(fixture.shift_id, &fixture.worker_claims())
        .await
        .unwrap();
    assert_eq!((room.waiting, room.in_consult, room.seen), (2, 0, 0));

    records
        .call_patient(fixture.shift_id, first, &fixture.worker_claims())
        .await
        .expect("the assigned clinician calls a patient in");
    let room = records
        .waiting_room(fixture.shift_id, &fixture.worker_claims())
        .await
        .unwrap();
    assert_eq!((room.waiting, room.in_consult, room.seen), (1, 1, 0));

    records
        .mark_patient_seen(fixture.shift_id, first, &fixture.worker_claims())
        .await
        .expect("and releases them");
    let room = records
        .waiting_room(fixture.shift_id, &fixture.worker_claims())
        .await
        .unwrap();
    assert_eq!((room.waiting, room.in_consult, room.seen), (1, 0, 1));
    assert_eq!(room.total, 2, "a seen patient stays in the total");
}

// 3 — the invariants that make the count meaningful.

#[tokio::test]
async fn only_one_patient_can_be_in_consultation() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    let first = queue_a_new_patient(&records, &fixture, "In Room").await;
    let second = queue_a_new_patient(&records, &fixture, "Still Waiting").await;

    records
        .call_patient(fixture.shift_id, first, &fixture.worker_claims())
        .await
        .unwrap();

    // Enforced by the partial unique index, not by a read-then-write.
    let result = records
        .call_patient(fixture.shift_id, second, &fixture.worker_claims())
        .await;
    assert!(
        matches!(
            result,
            Err(PatientRecordServiceError::AnotherPatientInConsult)
        ),
        "a second in_consult must be refused, got {result:?}"
    );
}

#[tokio::test]
async fn an_illegal_transition_is_refused_not_ignored() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    let entry = queue_a_new_patient(&records, &fixture, "Not Called Yet").await;

    // `seen` requires `in_consult`; skipping the call is a conflict, and must
    // not silently succeed.
    let result = records
        .mark_patient_seen(fixture.shift_id, entry, &fixture.worker_claims())
        .await;
    assert!(
        matches!(
            result,
            Err(PatientRecordServiceError::IllegalQueueTransition { .. })
        ),
        "got {result:?}"
    );
}

#[tokio::test]
async fn the_same_patient_cannot_be_queued_twice() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    queue_a_new_patient(&records, &fixture, "Only Once").await;
    let patient_id: Uuid =
        sqlx::query_scalar("SELECT id FROM patients WHERE full_name = 'Only Once'")
            .fetch_one(&pool)
            .await
            .unwrap();

    let result = records
        .add_patient_to_queue(
            fixture.shift_id,
            &fixture.admin_claims(),
            AddPatientToQueueRequest {
                patient_id: Some(patient_id),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(result, Err(PatientRecordServiceError::PatientAlreadyQueued)),
        "got {result:?}"
    );
}

// 4 — authorization.

#[tokio::test]
async fn a_worker_cannot_run_the_queue() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    // Queueing is the hospital's act — they have the patient in front of them.
    let result = records
        .add_patient_to_queue(
            fixture.shift_id,
            &fixture.worker_claims(),
            AddPatientToQueueRequest {
                patient: Some(new_patient("Wrong Actor")),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(result, Err(PatientRecordServiceError::NotTheOwningHospital)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn a_hospital_admin_cannot_call_a_patient_in() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;

    let entry = queue_a_new_patient(&records, &fixture, "Doctor's Call").await;
    let result = records
        .call_patient(fixture.shift_id, entry, &fixture.admin_claims())
        .await;
    assert!(
        matches!(
            result,
            Err(PatientRecordServiceError::NotTheAssignedClinician)
        ),
        "got {result:?}"
    );
}

#[tokio::test]
async fn a_foreign_hospital_cannot_see_the_waiting_room() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;
    queue_a_new_patient(&records, &fixture, "Private Patient").await;

    let other_hospital = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;
    let result = records
        .waiting_room(
            fixture.shift_id,
            &claims(intruder, UserRole::HospitalAdmin, Some(other_hospital)),
        )
        .await;

    assert!(
        matches!(result, Err(PatientRecordServiceError::NotAuthorized)),
        "the tenant boundary must hold, got {result:?}"
    );
}

/// Platform staff get the numbers so support can work, and no identities.
#[tokio::test]
async fn a_platform_admin_sees_counts_but_no_patients() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());
    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;
    queue_a_new_patient(&records, &fixture, "Confidential").await;

    let platform = seed_user(&pool, "operations_admin", None).await;
    let room = records
        .waiting_room(
            fixture.shift_id,
            &claims(platform, UserRole::OperationsAdmin, None),
        )
        .await
        .expect("platform admins may read metadata");

    assert_eq!(room.waiting, 1, "the count is support-visible");
    assert!(
        room.patients.is_empty(),
        "but no patient identities: {:?}",
        room.patients
    );
}

// 5 — the shift id in the path is not decorative.

#[tokio::test]
async fn an_entry_from_another_shift_cannot_be_mutated() {
    let Some(pool) = test_pool().await else { return };
    let video = video_service(&pool, false);
    let records = patient_record_service(&pool, video.clone());

    let fixture = seed_fixture(&pool).await;
    start_session(&video, &fixture).await;
    let entry = queue_a_new_patient(&records, &fixture, "Other Shift").await;

    // A second shift at the same hospital, with the same clinician — so the
    // caller is genuinely authorized *on that shift*, and only the entry's own
    // shift_id stops them.
    let second_shift = seed_shift(
        &pool,
        fixture.hospital_id,
        fixture.admin_user_id,
        Some(fixture.clinician_id),
        "virtual",
        "assigned",
        Utc::now(),
    )
    .await;

    let result = records
        .call_patient(second_shift, entry, &fixture.worker_claims())
        .await;
    assert!(
        matches!(
            result,
            Err(PatientRecordServiceError::QueueEntryNotFound(_))
        ),
        "an entry must belong to the shift in the path, got {result:?}"
    );
}

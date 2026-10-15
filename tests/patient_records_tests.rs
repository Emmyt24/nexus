//! Integration tests for patient records: the doctor's shift-scoped
//! consultation notes, the on-site party's per-patient handover notes, and the
//! hospital's roll-up read.
//!
//! Requires a reachable Postgres — set TEST_DATABASE_URL (falls back to a local
//! `nexuscare_test` database). Skips (prints a notice and returns early) rather
//! than failing the suite if no database is reachable, matching
//! `video_consult_tests.rs`.
//!
//! The first three tests are the regression for a cross-tenant leak found while
//! re-vetting this work: `get_note`, `list_for_patient` and `update_note` were
//! role-gated but never compared hospitals, so any health worker or hospital
//! admin could read and edit every hospital's clinical notes by id.
//!
//! `LiveKitClient` and `MlClient` both run in mock mode. The HTTP layer is never
//! exercised — services are constructed directly.


use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nexuscare_backend::models::user::{Claims, UserRole};
use nexuscare_backend::models::video_session::{
    JoinConsultRequest,
};
use nexuscare_backend::models::consultation_note::UpdateConsultationNoteRequest;
use nexuscare_backend::models::patient_record::{
    AddPatientToQueueRequest, SubmitPatientHandoverNoteRequest,
};
use nexuscare_backend::repositories::consultation_note::ConsultationNoteRepository;
use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::patient::PatientRepository;
use nexuscare_backend::repositories::patient_prediction::PatientPredictionRepository;
use nexuscare_backend::services::consultation_note_service::{
    ConsultationNoteError, ConsultationNoteService,
};
use nexuscare_backend::services::ml_client::MlClient;
use nexuscare_backend::services::patient_prediction_service::PatientPredictionService;
use nexuscare_backend::services::patient_record_service::{
    PatientRecordService, PatientRecordServiceError,
};
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


// Builders over the same mock-mode stack.

struct Stack {
    video: Arc<VideoService>,
    notes: Arc<ConsultationNoteService>,
    records: Arc<PatientRecordService>,
}

fn stack(pool: &PgPool) -> Stack {
    let video = video_service(pool, false);
    let patient_repo = Arc::new(PatientRepository::new(pool.clone()));
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
    let records = Arc::new(PatientRecordService::new(
        Arc::new(PatientRecordRepository::new(pool.clone())),
        Arc::new(VideoSessionRepository::new(pool.clone())),
        video.clone(),
        notes.clone(),
        patient_repo,
        prediction_service,
        pool.clone(),
    ));
    Stack {
        video,
        notes,
        records,
    }
}

async fn seed_patient(pool: &PgPool, hospital_id: Uuid, created_by: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO patients (hospital_id, created_by, full_name, age)
         VALUES ($1, $2, $3, 41.0) RETURNING id",
    )
    .bind(hospital_id)
    .bind(created_by)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("failed to seed patient")
}

fn note_fields(assessment: &str) -> UpdateConsultationNoteRequest {
    UpdateConsultationNoteRequest {
        assessment: Some(assessment.to_string()),
        plan: Some("Review in 7 days".to_string()),
        ..Default::default()
    }
}

/// Close a note's edit window without waiting an hour.
async fn close_edit_window(pool: &PgPool, note_id: Uuid) {
    sqlx::query("UPDATE consultation_notes SET editable_until = NOW() - INTERVAL '1 minute' WHERE id = $1")
        .bind(note_id)
        .execute(pool)
        .await
        .unwrap();
}

// 1 — the cross-tenant leak. These three are the regression.

#[tokio::test]
async fn a_foreign_hospital_cannot_read_a_note_by_id() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Leak Check").await;

    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.admin_user_id,
            None,
            None,
            &fixture.admin_claims(),
        )
        .await
        .unwrap();

    let other_hospital = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;
    let intruder_claims = claims(intruder, UserRole::HospitalAdmin, Some(other_hospital));

    // A role guard proves *a* role, never *which hospital*. Without this check
    // a note id is a read capability for the whole platform.
    let read = s.notes.get_detail(note.id, &intruder_claims).await;
    assert!(
        matches!(read, Err(ConsultationNoteError::WrongHospital)),
        "got {read:?}"
    );
}

#[tokio::test]
async fn a_foreign_hospital_cannot_list_a_patients_notes() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Leak List").await;

    let other_hospital = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;

    let listed = s
        .notes
        .list_for_patient(
            patient,
            &claims(intruder, UserRole::HospitalAdmin, Some(other_hospital)),
        )
        .await;
    assert!(
        matches!(listed, Err(ConsultationNoteError::WrongHospital)),
        "got {listed:?}"
    );
}

#[tokio::test]
async fn a_foreign_hospital_cannot_edit_a_note() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Leak Edit").await;

    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.admin_user_id,
            None,
            None,
            &fixture.admin_claims(),
        )
        .await
        .unwrap();

    let other_hospital = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;

    let edited = s
        .notes
        .update_fields(
            note.id,
            &claims(intruder, UserRole::HospitalAdmin, Some(other_hospital)),
            note_fields("Tampered"),
        )
        .await;
    assert!(
        matches!(edited, Err(ConsultationNoteError::WrongHospital)),
        "got {edited:?}"
    );

    // And nothing changed.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT assessment FROM consultation_notes WHERE id = $1")
            .bind(note.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, None);
}

/// Platform staff have no lawful basis for a note body — only metadata.
#[tokio::test]
async fn a_platform_admin_cannot_read_a_note_body() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "NDPR").await;
    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.admin_user_id,
            None,
            None,
            &fixture.admin_claims(),
        )
        .await
        .unwrap();

    let platform = seed_user(&pool, "operations_admin", None).await;
    let read = s
        .notes
        .get_detail(note.id, &claims(platform, UserRole::OperationsAdmin, None))
        .await;
    assert!(
        matches!(read, Err(ConsultationNoteError::NotAuthorized)),
        "got {read:?}"
    );
}

// 2 — shift-scoped notes.

#[tokio::test]
async fn the_assigned_clinician_can_write_a_shift_scoped_note() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Shift Note").await;

    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.worker_user_id,
            Some(fixture.shift_id),
            None,
            &fixture.worker_claims(),
        )
        .await
        .expect("the assigned clinician may record against their shift");

    assert_eq!(note.shift_id, Some(fixture.shift_id));
    assert!(
        note.editable_until.is_some(),
        "a new note gets an edit window"
    );

    let listed = s
        .records
        .list_clinical_notes(fixture.shift_id, patient, &fixture.worker_claims())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, note.id);
}

#[tokio::test]
async fn another_health_worker_cannot_write_against_the_shift() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Not Yours").await;

    let other_user = seed_user(&pool, "health_worker", None).await;
    seed_clinician(&pool, other_user).await;

    let result = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            other_user,
            Some(fixture.shift_id),
            None,
            &claims(other_user, UserRole::HealthWorker, None),
        )
        .await;
    assert!(
        matches!(result, Err(ConsultationNoteError::NotTheAssignedClinician)),
        "got {result:?}"
    );
}

/// A note is shift-keyed, not room-keyed, so an in-person shift works too.
#[tokio::test]
async fn a_note_works_on_an_in_person_shift() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture_with(&pool, "in_person", "assigned", Utc::now()).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "On Site").await;

    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.worker_user_id,
            Some(fixture.shift_id),
            None,
            &fixture.worker_claims(),
        )
        .await
        .expect("a patient record is not a room — in-person shifts qualify");
    assert_eq!(note.shift_id, Some(fixture.shift_id));
}

/// The pre-existing ad-hoc path must not regress.
#[tokio::test]
async fn a_note_with_no_shift_still_works_and_is_absent_from_the_rollup() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Ad Hoc").await;

    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.admin_user_id,
            None,
            None,
            &fixture.admin_claims(),
        )
        .await
        .expect("the ad-hoc flow posts no shift at all");
    assert_eq!(note.shift_id, None);

    let rollup = s
        .records
        .shift_patient_records(fixture.shift_id, &fixture.admin_claims())
        .await
        .unwrap();
    assert_eq!(
        rollup.patients_total, 0,
        "a note with no shift belongs to no roll-up"
    );
}

// 3 — the edit window and the amendment chain.

#[tokio::test]
async fn a_note_is_editable_inside_its_window_and_locked_after() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Windowed").await;

    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.worker_user_id,
            Some(fixture.shift_id),
            None,
            &fixture.worker_claims(),
        )
        .await
        .unwrap();

    let edited = s
        .notes
        .update_fields(note.id, &fixture.worker_claims(), note_fields("Stable angina"))
        .await
        .expect("editable inside the window");
    assert_eq!(edited.assessment.as_deref(), Some("Stable angina"));

    close_edit_window(&pool, note.id).await;

    let late = s
        .notes
        .update_fields(note.id, &fixture.worker_claims(), note_fields("Rewritten"))
        .await;
    assert!(
        matches!(late, Err(ConsultationNoteError::EditWindowClosed)),
        "a locked clinical record must not be rewritten, got {late:?}"
    );
}

/// `editable_until IS NULL` on a row written before 20240063 must read as
/// closed, not open.
#[tokio::test]
async fn a_note_with_no_window_is_treated_as_closed() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Legacy").await;

    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.admin_user_id,
            None,
            None,
            &fixture.admin_claims(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE consultation_notes SET editable_until = NULL WHERE id = $1")
        .bind(note.id)
        .execute(&pool)
        .await
        .unwrap();

    let result = s
        .notes
        .update_fields(note.id, &fixture.admin_claims(), note_fields("Legacy edit"))
        .await;
    assert!(
        matches!(result, Err(ConsultationNoteError::EditWindowClosed)),
        "NULL is not an open window, got {result:?}"
    );
}

#[tokio::test]
async fn an_amendment_is_a_new_row_and_leaves_the_original_intact() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Amended").await;

    let original = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.worker_user_id,
            Some(fixture.shift_id),
            None,
            &fixture.worker_claims(),
        )
        .await
        .unwrap();
    s.notes
        .update_fields(
            original.id,
            &fixture.worker_claims(),
            note_fields("Initial impression"),
        )
        .await
        .unwrap();
    close_edit_window(&pool, original.id).await;

    let amendment = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.worker_user_id,
            Some(fixture.shift_id),
            Some(original.id),
            &fixture.worker_claims(),
        )
        .await
        .expect("a locked note is amended by a new one, not rewritten");

    assert_eq!(amendment.amends_note_id, Some(original.id));
    assert_ne!(amendment.id, original.id);

    let kept: Option<String> =
        sqlx::query_scalar("SELECT assessment FROM consultation_notes WHERE id = $1")
            .bind(original.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept.as_deref(),
        Some("Initial impression"),
        "the superseded note is preserved verbatim"
    );

    let listed = s
        .records
        .list_clinical_notes(fixture.shift_id, patient, &fixture.worker_claims())
        .await
        .unwrap();
    assert_eq!(listed.len(), 2, "both the original and the amendment are kept");
}

#[tokio::test]
async fn a_clinician_cannot_edit_a_colleagues_note() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Someone Else's").await;

    // Authored by the hospital admin.
    let note = s
        .notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.admin_user_id,
            None,
            None,
            &fixture.admin_claims(),
        )
        .await
        .unwrap();

    // A second admin at the same hospital passes the tenant check but is not
    // the author.
    let colleague = seed_user(&pool, "hospital_admin", Some(fixture.hospital_id)).await;
    let result = s
        .notes
        .update_fields(
            note.id,
            &claims(colleague, UserRole::HospitalAdmin, Some(fixture.hospital_id)),
            note_fields("Not mine to edit"),
        )
        .await;
    assert!(
        matches!(result, Err(ConsultationNoteError::NotAuthorized)),
        "got {result:?}"
    );
}

// 4 — per-patient handover notes.

#[tokio::test]
async fn the_handover_note_is_one_row_per_shift_and_patient() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Handed Over").await;

    let first = s
        .records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Stable, awaiting bloods".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("the on-site party files the note");

    let second = s
        .records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Bloods back, discharged".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("a resubmit inside the window updates");

    assert_eq!(first.id, second.id, "one row per (shift, patient)");
    assert_eq!(second.summary, "Bloods back, discharged");

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM patient_handover_notes WHERE shift_id = $1 AND patient_id = $2",
    )
    .bind(fixture.shift_id)
    .bind(patient)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn an_escalation_needs_a_reason() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Escalate").await;

    let result = s
        .records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Deteriorating".to_string(),
                escalation_required: true,
                escalation_reason: Some("   ".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(result, Err(PatientRecordServiceError::Validation(_))),
        "got {result:?}"
    );
}

/// The on-site party may file the per-patient handover note, and on an in-person
/// shift that party is the assigned clinician. Restricting this to HospitalAdmin
/// left nobody able to file one there, and contradicted the shift-level handover,
/// which is HealthWorker-authored.
#[tokio::test]
async fn the_assigned_clinician_can_file_the_handover_note() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Worker Authored").await;

    let note = s
        .records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.worker_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Reviewed at the bedside".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("the assigned clinician files the handover note");

    assert_eq!(note.patient_id, patient);
    // Authorship is taken from the token, never the body.
    assert_eq!(note.author_user_id, fixture.worker_user_id);

    // And the hospital still reads it — this is the record they keep.
    let read = s
        .records
        .get_handover_note(fixture.shift_id, patient, &fixture.admin_claims())
        .await
        .expect("the hospital reads the clinician's note");
    assert_eq!(read.summary, "Reviewed at the bedside");
}

/// Widening authorship to the clinician must not widen it to *any* clinician:
/// `ShiftDataRole::Clinician` is only ever the assigned one.
#[tokio::test]
async fn an_unassigned_worker_cannot_file_the_handover_note() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Wrong Author").await;

    let other_user = seed_user(&pool, "health_worker", None).await;
    seed_clinician(&pool, other_user).await;

    let result = s
        .records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &claims(other_user, UserRole::HealthWorker, None),
            SubmitPatientHandoverNoteRequest {
                summary: "Not my shift".to_string(),
                ..Default::default()
            },
        )
        .await;
    // Refused upstream by authorize_shift_data_access: a health worker who does
    // not hold the assignment is not a party to this shift at all, which is a
    // stronger statement than "wrong hospital".
    assert!(
        matches!(result, Err(PatientRecordServiceError::NotAuthorized)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn a_note_cannot_be_attached_to_another_hospitals_patient() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;

    let other_hospital = seed_hospital(&pool).await;
    let other_admin = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;
    let foreign_patient = seed_patient(&pool, other_hospital, other_admin, "Foreign").await;

    let result = s
        .records
        .submit_handover_note(
            fixture.shift_id,
            foreign_patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Should not land".to_string(),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(
            result,
            Err(PatientRecordServiceError::PatientHospitalMismatch)
        ),
        "got {result:?}"
    );
}

// 5 — the hospital's roll-up, during and after the shift.

#[tokio::test]
async fn the_rollup_survives_shift_completion() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Recorded").await;

    s.notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.worker_user_id,
            Some(fixture.shift_id),
            None,
            &fixture.worker_claims(),
        )
        .await
        .unwrap();
    s.records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Seen and discharged".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // "Accessible after a shift ends" is a guarantee that access persists.
    sqlx::query("UPDATE shifts SET status = 'completed' WHERE id = $1")
        .bind(fixture.shift_id)
        .execute(&pool)
        .await
        .unwrap();

    let rollup = s
        .records
        .shift_patient_records(fixture.shift_id, &fixture.admin_claims())
        .await
        .expect("the hospital reads its own record after completion");

    assert_eq!(rollup.shift_status, "completed");
    assert!(!rollup.metadata_only);
    assert_eq!(rollup.patients_total, 1);
    assert_eq!(rollup.patients.len(), 1);
    assert_eq!(rollup.patients[0].clinical_notes.len(), 1);
    assert!(rollup.patients[0].handover_note.is_some());
}

/// A second admin at the same hospital must not be locked out — the regression
/// the `shift.created_by == claims.sub` idiom would have introduced.
#[tokio::test]
async fn a_colleague_at_the_same_hospital_can_read_the_rollup() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Colleague").await;
    s.records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Filed by a colleague".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let colleague = seed_user(&pool, "hospital_admin", Some(fixture.hospital_id)).await;
    let rollup = s
        .records
        .shift_patient_records(
            fixture.shift_id,
            &claims(colleague, UserRole::HospitalAdmin, Some(fixture.hospital_id)),
        )
        .await
        .expect("any admin of the owning hospital may read");
    assert_eq!(rollup.patients_total, 1);
}

#[tokio::test]
async fn a_foreign_hospital_cannot_read_the_rollup() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;

    let other_hospital = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;
    let result = s
        .records
        .shift_patient_records(
            fixture.shift_id,
            &claims(intruder, UserRole::HospitalAdmin, Some(other_hospital)),
        )
        .await;
    assert!(
        matches!(result, Err(PatientRecordServiceError::NotAuthorized)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn a_platform_admin_gets_counts_without_clinical_content() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Secret Name").await;
    s.records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Confidential summary".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let platform = seed_user(&pool, "super_admin", None).await;
    let rollup = s
        .records
        .shift_patient_records(
            fixture.shift_id,
            &claims(platform, UserRole::SuperAdmin, None),
        )
        .await
        .expect("support may see that a record exists");

    assert!(rollup.metadata_only);
    assert_eq!(rollup.patients_total, 1, "the count is support-visible");
    assert!(
        rollup.patients.is_empty(),
        "but no names and no note bodies: {:?}",
        rollup.patients
    );
}

// 6 — the waiting room reflects filed notes, so the UI needs no extra reads.

#[tokio::test]
async fn the_waiting_room_flags_which_notes_are_filed() {
    let Some(pool) = test_pool().await else { return };
    let s = stack(&pool);
    let fixture = seed_fixture(&pool).await;
    s.video
        .issue_join_token(
            fixture.shift_id,
            &fixture.worker_claims(),
            JoinConsultRequest::default(),
        )
        .await
        .unwrap();

    let patient = seed_patient(&pool, fixture.hospital_id, fixture.admin_user_id, "Flagged").await;
    s.records
        .add_patient_to_queue(
            fixture.shift_id,
            &fixture.admin_claims(),
            AddPatientToQueueRequest {
                patient_id: Some(patient),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let before = s
        .records
        .waiting_room(fixture.shift_id, &fixture.admin_claims())
        .await
        .unwrap();
    assert!(!before.patients[0].has_clinical_note);
    assert!(!before.patients[0].has_handover_note);

    s.notes
        .start(
            patient,
            Some(fixture.hospital_id),
            fixture.worker_user_id,
            Some(fixture.shift_id),
            None,
            &fixture.worker_claims(),
        )
        .await
        .unwrap();
    s.records
        .submit_handover_note(
            fixture.shift_id,
            patient,
            &fixture.admin_claims(),
            SubmitPatientHandoverNoteRequest {
                summary: "Done".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let after = s
        .records
        .waiting_room(fixture.shift_id, &fixture.admin_claims())
        .await
        .unwrap();
    assert!(after.patients[0].has_clinical_note);
    assert!(after.patients[0].has_handover_note);
}

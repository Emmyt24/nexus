//! SCRUM-26 / US-10 — Express Interest in a Shift.
//!
//! Requires a reachable Postgres — set TEST_DATABASE_URL (falls back to a local
//! `nexuscare_test` database). Skips (prints a notice and returns early) rather
//! than failing the suite when no database is reachable, matching
//! `video_consult_tests.rs`. The HTTP layer is never exercised: `ShiftService`
//! is constructed directly, so these assert the service contract the handlers
//! call into — in particular that the caller is identified by `users.id` and
//! resolved to `clinicians.id` inside the service.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nexuscare_backend::models::shift::ShiftApplicationRequest;
use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::shift::ShiftRepository;
use nexuscare_backend::repositories::wallet::WalletRepository;
use nexuscare_backend::repositories::EmailOutboxRepository;
use nexuscare_backend::services::email_outbox_service::EmailOutboxService;
use nexuscare_backend::services::fcm::FcmClient;
use nexuscare_backend::services::notification_service::NotificationService;
use nexuscare_backend::services::push_service::PushService;
use nexuscare_backend::services::safehaven::SafeHavenClient;
use nexuscare_backend::services::shift_service::{ShiftService, ShiftServiceError};
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

fn shift_service(pool: &PgPool) -> Arc<ShiftService> {
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

    Arc::new(ShiftService::new(
        shift_repo,
        pool.clone(),
        notification_service,
        email_outbox,
        wallet_service,
        push,
    ))
}

struct Fixture {
    admin_user_id: Uuid,
    worker_user_id: Uuid,
    clinician_id: Uuid,
    shift_id: Uuid,
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

/// A clinician with a *complete* profile — `license_number` and `clinician_role`
/// are what `apply_for_shift` gates on.
async fn seed_clinician(pool: &PgPool, user_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO clinicians (
            user_id, first_name, last_name, specialty, role_title,
            license_number, clinician_role
        )
        VALUES ($1, 'Amina', 'Bello', 'emergency_medicine', 'Emergency Doctor',
                'MDCN-12345', 'doctor')
        RETURNING id
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("failed to seed clinician")
}

async fn seed_shift(
    pool: &PgPool,
    hospital_id: Uuid,
    created_by: Uuid,
    assigned_clinician_id: Option<Uuid>,
    status: &str,
    scheduled_start: DateTime<Utc>,
) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, shift_type, status,
            scheduled_start, duration_hours, scheduled_end,
            assigned_clinician_id, pay_type, rate_kobo_per_hour,
            grand_total_kobo, created_by
        )
        VALUES ($1, 'doctor', 'Emergency Doctor', 'in_person', $2::shift_status,
                $3, 4, $3 + INTERVAL '4 hours',
                $4, 'hourly_rate', 800000, 3200000, $5)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(status)
    .bind(scheduled_start)
    .bind(assigned_clinician_id)
    .bind(created_by)
    .fetch_one(pool)
    .await
    .expect("failed to seed shift")
}

async fn seed_fixture(pool: &PgPool, status: &str) -> Fixture {
    let hospital_id = seed_hospital(pool).await;
    let admin_user_id = seed_user(pool, "hospital_admin", Some(hospital_id)).await;
    let worker_user_id = seed_user(pool, "health_worker", None).await;
    let clinician_id = seed_clinician(pool, worker_user_id).await;
    // Only an already-taken shift carries an assignee.
    let assignee = (status != "open").then_some(clinician_id);
    let shift_id = seed_shift(
        pool,
        hospital_id,
        admin_user_id,
        assignee,
        status,
        Utc::now() + Duration::days(1),
    )
    .await;

    Fixture {
        admin_user_id,
        worker_user_id,
        clinician_id,
        shift_id,
    }
}

async fn interest_count(pool: &PgPool, shift_id: Uuid, clinician_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM shift_interests WHERE shift_id = $1 AND clinician_id = $2",
    )
    .bind(shift_id)
    .bind(clinician_id)
    .fetch_one(pool)
    .await
    .expect("failed to count interests")
}

/// UT001/UT002 — interest on an open shift is recorded against `clinicians.id`.
///
/// The service is handed the worker's `users.id` — the only id the frontend has
/// from the JWT `sub` — and must resolve it to the clinician profile itself.
/// Passing `users.id` straight through used to violate the
/// `shift_interests.clinician_id -> clinicians (id)` foreign key and surface as
/// a 500.
#[tokio::test]
async fn ut001_interest_recorded_on_open_shift() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "open").await;

    shift_service(&pool)
        .express_interest(fx.shift_id, fx.worker_user_id)
        .await
        .expect("interest recorded");

    assert_eq!(interest_count(&pool, fx.shift_id, fx.clinician_id).await, 1);
}

/// A `health_worker` account with no `clinicians` row gets an actionable 403,
/// not the opaque 500 the foreign-key violation used to produce.
#[tokio::test]
async fn interest_without_clinician_profile_is_rejected() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "open").await;
    let orphan_user_id = seed_user(&pool, "health_worker", None).await;

    let err = shift_service(&pool)
        .express_interest(fx.shift_id, orphan_user_id)
        .await
        .expect_err("no clinician profile");

    assert!(matches!(err, ShiftServiceError::NoClinicianProfile));
}

/// UT007 — expressing interest in a non-open shift is rejected.
#[tokio::test]
async fn ut007_interest_on_assigned_shift_rejected() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "assigned").await;

    let err = shift_service(&pool)
        .express_interest(fx.shift_id, fx.worker_user_id)
        .await
        .expect_err("shift is no longer available");

    assert!(matches!(err, ShiftServiceError::ShiftUnavailable));
}

/// UT008/UT009 — the hospital admin is notified on interest.
#[tokio::test]
async fn ut008_hospital_notified_on_interest() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "open").await;

    shift_service(&pool)
        .express_interest(fx.shift_id, fx.worker_user_id)
        .await
        .expect("interest recorded");

    let notified: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM notifications
        WHERE user_id = $1 AND kind = 'interest_expressed'
        "#,
    )
    .bind(fx.admin_user_id)
    .fetch_one(&pool)
    .await
    .expect("failed to count notifications");

    assert_eq!(notified, 1);
}

/// UT014 — duplicate interest is prevented (one record per shift/clinician).
#[tokio::test]
async fn ut014_duplicate_interest_prevented() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "open").await;
    let svc = shift_service(&pool);

    svc.express_interest(fx.shift_id, fx.worker_user_id)
        .await
        .expect("first interest recorded");

    let err = svc
        .express_interest(fx.shift_id, fx.worker_user_id)
        .await
        .expect_err("second interest rejected");

    assert!(matches!(err, ShiftServiceError::DuplicateInterest));
    assert_eq!(interest_count(&pool, fx.shift_id, fx.clinician_id).await, 1);
}

/// Interest expressed via the service round-trips through withdrawal.
///
/// Both sides resolve the clinician from the same `users.id`, so a POST can
/// always be undone by a DELETE — impossible while the POST took an arbitrary
/// `clinician_id` from the request body.
#[tokio::test]
async fn interest_round_trips_with_withdrawal() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "open").await;
    let svc = shift_service(&pool);

    svc.express_interest(fx.shift_id, fx.worker_user_id)
        .await
        .expect("interest recorded");
    svc.withdraw_interest(fx.shift_id, fx.worker_user_id)
        .await
        .expect("interest withdrawn");

    assert_eq!(interest_count(&pool, fx.shift_id, fx.clinician_id).await, 0);
}

/// Applying records interest too, so the hospital can offer the shift.
///
/// `offer_shift` only offers to a clinician who has expressed interest; an
/// application on its own used to be a dead end (409 `NotInterested`).
#[tokio::test]
async fn apply_records_interest_so_offer_succeeds() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "open").await;
    let svc = shift_service(&pool);

    svc.apply_for_shift(
        fx.shift_id,
        fx.worker_user_id,
        ShiftApplicationRequest {
            years_experience: 5,
            experience_summary: Some("Five years in emergency medicine".to_string()),
        },
    )
    .await
    .expect("application submitted");

    assert_eq!(interest_count(&pool, fx.shift_id, fx.clinician_id).await, 1);

    svc.offer_shift(fx.shift_id, fx.clinician_id, fx.admin_user_id)
        .await
        .expect("hospital can offer the shift to the applicant");
}

/// Applying twice is still a conflict, and the interest insert does not mask it.
#[tokio::test]
async fn duplicate_application_still_conflicts() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let fx = seed_fixture(&pool, "open").await;
    let svc = shift_service(&pool);

    let request = || ShiftApplicationRequest {
        years_experience: 5,
        experience_summary: None,
    };

    svc.apply_for_shift(fx.shift_id, fx.worker_user_id, request())
        .await
        .expect("first application submitted");

    let err = svc
        .apply_for_shift(fx.shift_id, fx.worker_user_id, request())
        .await
        .expect_err("second application rejected");

    assert!(matches!(err, ShiftServiceError::DuplicateApplication));
    assert_eq!(interest_count(&pool, fx.shift_id, fx.clinician_id).await, 1);
}

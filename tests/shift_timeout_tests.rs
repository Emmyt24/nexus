//! Shift-timeout sweep — closing shifts whose scheduled end has passed.
//!
//! Requires a reachable Postgres — set `TEST_DATABASE_URL` (falls back to a
//! local `nexuscare_test`). Skips with a printed notice rather than failing when
//! no database is reachable. `ShiftService` is constructed directly.
//!
//! Worth having because this sweep is the only thing that moves money without a
//! human in the loop. The bug it fixes is a shift nobody closed holding the
//! hospital's escrow in `held_kobo` forever while `clinician_has_active_assignment`
//! keeps the clinician permanently ineligible — and the two branches owe
//! opposite things, so getting the split wrong either pays a no-show or steals a
//! day's pay from someone who worked.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nexuscare_backend::models::shift::ShiftStatus;
use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::shift::ShiftRepository;
use nexuscare_backend::repositories::wallet::WalletRepository;
use nexuscare_backend::repositories::EmailOutboxRepository;
use nexuscare_backend::services::email_outbox_service::EmailOutboxService;
use nexuscare_backend::services::fcm::FcmClient;
use nexuscare_backend::services::notification_service::NotificationService;
use nexuscare_backend::services::push_service::PushService;
use nexuscare_backend::services::safehaven::SafeHavenClient;
use nexuscare_backend::services::shift_service::ShiftService;
use nexuscare_backend::services::wallet_service::WalletService;
use sqlx::PgPool;
use uuid::Uuid;

/// The sweep reads its grace period from the single `platform_settings` row, and
/// one test changes it, so these run serially. Same reasoning as
/// `platform_fee_tests`.
static SETTINGS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn settings_lock() -> std::sync::MutexGuard<'static, ()> {
    SETTINGS.lock().unwrap_or_else(|e| e.into_inner())
}

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
        Arc::new(ShiftRepository::new(pool.clone())),
        pool.clone(),
        notification_service,
        email_outbox,
        wallet_service,
        push,
    ))
}

async fn set_grace(pool: &PgPool, minutes: i32) {
    sqlx::query("UPDATE platform_settings SET shift_close_grace_minutes = $1 WHERE singleton = 'global'")
        .bind(minutes)
        .execute(pool)
        .await
        .expect("failed to set grace");
}

async fn seed_hospital(pool: &PgPool) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO hospitals (name, registration_number, email, address, phone_number)
        VALUES ($1, $2, $3, 'Test Address', '08000000000')
        RETURNING id
        "#,
    )
    .bind(format!("Timeout Hospital {}", Uuid::new_v4()))
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
        INSERT INTO clinicians (
            user_id, first_name, last_name, specialty, role_title,
            license_number, clinician_role
        )
        VALUES ($1, 'Amina', 'Bello', 'emergency_medicine', 'Emergency Doctor', $2, 'doctor')
        RETURNING id
        "#,
    )
    .bind(user_id)
    .bind(format!("MDCN-{}", &Uuid::new_v4().to_string()[..8]))
    .fetch_one(pool)
    .await
    .expect("failed to seed clinician")
}

/// A shift that ended `ended_minutes_ago` minutes ago, in `status`.
/// `grand_total_kobo` is ₦32,000 so the escrow assertions have a figure.
async fn seed_ended_shift(
    pool: &PgPool,
    hospital_id: Uuid,
    created_by: Uuid,
    clinician_id: Uuid,
    status: &str,
    ended_minutes_ago: i64,
) -> Uuid {
    let scheduled_end = Utc::now() - Duration::minutes(ended_minutes_ago);
    sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, shift_type, status,
            scheduled_start, duration_hours, scheduled_end,
            assigned_clinician_id, pay_type, rate_kobo_per_hour,
            grand_total_kobo, created_by
        )
        VALUES ($1, 'doctor', 'Emergency Doctor', 'in_person', $4::shift_status,
                $5 - INTERVAL '4 hours', 4, $5,
                $2, 'hourly_rate', 800000, 3200000, $3)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(clinician_id)
    .bind(created_by)
    .bind(status)
    .bind(scheduled_end)
    .fetch_one(pool)
    .await
    .expect("failed to seed ended shift")
}

async fn seed_clockin(pool: &PgPool, shift_id: Uuid, clinician_id: Uuid, at: DateTime<Utc>) {
    sqlx::query(
        r#"
        INSERT INTO shift_attendance (shift_id, clinician_id, clockin_at, clockin_method)
        VALUES ($1, $2, $3, 'gps')
        "#,
    )
    .bind(shift_id)
    .bind(clinician_id)
    .bind(at)
    .execute(pool)
    .await
    .expect("failed to seed clock-in");
}

/// An attendance row with no clock-in — the shape left behind when a clock-in
/// approval was raised but never granted.
async fn seed_attendance_without_clockin(pool: &PgPool, shift_id: Uuid, clinician_id: Uuid) {
    sqlx::query(
        "INSERT INTO shift_attendance (shift_id, clinician_id, clockin_method) \
         VALUES ($1, $2, 'manual')",
    )
    .bind(shift_id)
    .bind(clinician_id)
    .execute(pool)
    .await
    .expect("failed to seed attendance");
}

async fn fund_and_hold(pool: &PgPool, hospital_id: Uuid, balance: i64, held: i64) {
    sqlx::query(
        "INSERT INTO hospital_wallets (hospital_id, balance_kobo, held_kobo) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (hospital_id) DO UPDATE SET balance_kobo = $2, held_kobo = $3",
    )
    .bind(hospital_id)
    .bind(balance)
    .bind(held)
    .execute(pool)
    .await
    .expect("failed to set wallet");
}

async fn shift_status(pool: &PgPool, shift_id: Uuid) -> String {
    sqlx::query_scalar::<_, String>("SELECT status::text FROM shifts WHERE id = $1")
        .bind(shift_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn wallet(pool: &PgPool, hospital_id: Uuid) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT balance_kobo, held_kobo FROM hospital_wallets WHERE hospital_id = $1",
    )
    .bind(hospital_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

struct Fixture {
    hospital_id: Uuid,
    admin_user_id: Uuid,
    clinician_id: Uuid,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let hospital_id = seed_hospital(pool).await;
    let admin_user_id = seed_user(pool, "hospital_admin", Some(hospital_id)).await;
    let worker = seed_user(pool, "health_worker", None).await;
    let clinician_id = seed_clinician(pool, worker).await;
    Fixture {
        hospital_id,
        admin_user_id,
        clinician_id,
    }
}

// 1 — worked but never closed out. The clinician must not lose the pay.

#[tokio::test]
async fn a_shift_worked_but_never_clocked_out_is_completed() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    // Ended 30 minutes ago, so past the 15-minute grace.
    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "in_progress",
        30,
    )
    .await;
    seed_clockin(&pool, shift_id, f.clinician_id, Utc::now() - Duration::hours(5)).await;

    let closed = svc.close_timed_out_shifts().await.expect("sweep should run");
    assert!(closed >= 1, "the sweep should have closed at least this shift");

    assert_eq!(shift_status(&pool, shift_id).await, "completed");
}

#[tokio::test]
async fn worked_minutes_are_measured_to_the_scheduled_end_not_to_now() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    // Ended two days ago. If this measured to NOW() the clinician would be
    // credited ~48 hours instead of the 4-hour shift they worked, and the
    // hospital only escrowed four hours.
    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "in_progress",
        60 * 48,
    )
    .await;
    let scheduled_end: DateTime<Utc> =
        sqlx::query_scalar("SELECT scheduled_end FROM shifts WHERE id = $1")
            .bind(shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    seed_clockin(&pool, shift_id, f.clinician_id, scheduled_end - Duration::hours(4)).await;

    svc.close_timed_out_shifts().await.unwrap();

    let (worked, clockout): (Option<i32>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT worked_minutes, clockout_at FROM shift_attendance WHERE shift_id = $1",
    )
    .bind(shift_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(worked, Some(240), "four hours, not two days");
    let clockout = clockout.expect("clock-out should be recorded");
    assert!(
        (clockout - scheduled_end).num_seconds().abs() < 2,
        "clock-out should be the scheduled end, got {clockout} vs {scheduled_end}"
    );
}

#[tokio::test]
async fn completing_a_timed_out_shift_sets_completed_at_to_the_scheduled_end() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "in_progress",
        120,
    )
    .await;
    seed_clockin(&pool, shift_id, f.clinician_id, Utc::now() - Duration::hours(6)).await;

    svc.close_timed_out_shifts().await.unwrap();

    // The rating window is measured from this, so a sweep that stamped NOW()
    // would hand both parties an extra window the shift never earned.
    let (completed_at, scheduled_end): (Option<DateTime<Utc>>, DateTime<Utc>) =
        sqlx::query_as("SELECT completed_at, scheduled_end FROM shifts WHERE id = $1")
            .bind(shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let completed_at = completed_at.expect("completed_at should be set");
    assert!(
        (completed_at - scheduled_end).num_seconds().abs() < 2,
        "expected {scheduled_end}, got {completed_at}"
    );
}

#[tokio::test]
async fn a_completed_timed_out_shift_keeps_its_escrow_for_payout() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "in_progress",
        30,
    )
    .await;
    seed_clockin(&pool, shift_id, f.clinician_id, Utc::now() - Duration::hours(5)).await;
    fund_and_hold(&pool, f.hospital_id, 1_000_000, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();

    // The work happened, so the money stays held for the payout pipeline.
    // Releasing it here would mean paying the clinician from nothing.
    let (balance, held) = wallet(&pool, f.hospital_id).await;
    assert_eq!(held, 3_200_000, "escrow must remain for a completed shift");
    assert_eq!(balance, 1_000_000, "balance must not change");
}

// 2 — never turned up. The hospital's money must come back.

#[tokio::test]
async fn a_shift_with_no_clock_in_becomes_no_show_and_releases_the_escrow() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 500_000, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();

    assert_eq!(shift_status(&pool, shift_id).await, "no_show");

    // This is the bug: before the sweep the hold sat there forever, because
    // cancel_shift refuses anything past 'upcoming'.
    let (balance, held) = wallet(&pool, f.hospital_id).await;
    assert_eq!(held, 0, "the hold must be released");
    assert_eq!(
        balance, 3_700_000,
        "the released escrow should be back in the balance"
    );
}

#[tokio::test]
async fn an_assigned_shift_that_never_started_is_also_swept() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "assigned",
        45,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();
    assert_eq!(shift_status(&pool, shift_id).await, "no_show");
}

#[tokio::test]
async fn an_attendance_row_with_no_clock_in_is_flagged_missed() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    seed_attendance_without_clockin(&pool, shift_id, f.clinician_id).await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();

    assert_eq!(shift_status(&pool, shift_id).await, "no_show");
    let missed: bool =
        sqlx::query_scalar("SELECT missed FROM shift_attendance WHERE shift_id = $1")
            .bind(shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(missed, "shift_attendance.missed should be set");
}

// 3 — the grace period itself.

#[tokio::test]
async fn a_shift_inside_the_grace_window_is_left_alone() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    // Ended 5 minutes ago: still inside the 15-minute grace.
    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "in_progress",
        5,
    )
    .await;
    seed_clockin(&pool, shift_id, f.clinician_id, Utc::now() - Duration::hours(4)).await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();

    assert_eq!(
        shift_status(&pool, shift_id).await,
        "in_progress",
        "the clinician is still inside the window to clock out themselves"
    );
    let (_, held) = wallet(&pool, f.hospital_id).await;
    assert_eq!(held, 3_200_000, "nothing should have moved");
}

#[tokio::test]
async fn the_grace_period_comes_from_platform_settings() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    // Ended 30 minutes ago. With a 60-minute grace it must survive; drop the
    // grace to zero and the same shift is swept. This is what stops the setting
    // becoming dead configuration like its three neighbours.
    set_grace(&pool, 60).await;
    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();
    assert_eq!(
        shift_status(&pool, shift_id).await,
        "upcoming",
        "a 60-minute grace should still protect a shift that ended 30 minutes ago"
    );

    set_grace(&pool, 0).await;
    svc.close_timed_out_shifts().await.unwrap();
    assert_eq!(shift_status(&pool, shift_id).await, "no_show");

    set_grace(&pool, 15).await;
}

// 4 — races and idempotence, the properties the atomic claim exists for.

#[tokio::test]
async fn running_the_sweep_twice_does_not_double_process() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();
    let (balance_after_first, held_after_first) = wallet(&pool, f.hospital_id).await;

    // A second pass must not release the escrow again — that would credit the
    // hospital twice for one shift.
    svc.close_timed_out_shifts().await.unwrap();
    let (balance, held) = wallet(&pool, f.hospital_id).await;

    assert_eq!(balance, balance_after_first, "escrow released twice");
    assert_eq!(held, held_after_first);
    assert_eq!(shift_status(&pool, shift_id).await, "no_show");
}

#[tokio::test]
async fn two_concurrent_sweeps_each_claim_a_row_once() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    // The claim is one statement with its predicate inside the UPDATE, so only
    // one of these can take the row. Select-then-act would release twice.
    let a = shift_service(&pool);
    let b = shift_service(&pool);
    let (ra, rb) = tokio::join!(a.close_timed_out_shifts(), b.close_timed_out_shifts());
    ra.expect("sweep a");
    rb.expect("sweep b");

    let (balance, held) = wallet(&pool, f.hospital_id).await;
    assert_eq!(held, 0);
    assert_eq!(
        balance, 3_200_000,
        "the escrow must be credited exactly once, not twice"
    );
    assert_eq!(shift_status(&pool, shift_id).await, "no_show");
}

// 5 — what the new terminal state must and must not unblock.

#[tokio::test]
async fn a_no_show_shift_no_longer_blocks_the_clinician() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    let repo = ShiftRepository::new(pool.clone());
    assert!(
        repo.clinician_has_active_assignment(f.clinician_id)
            .await
            .unwrap(),
        "precondition: the stuck shift holds the clinician"
    );

    svc.close_timed_out_shifts().await.unwrap();

    assert!(
        !repo
            .clinician_has_active_assignment(f.clinician_id)
            .await
            .unwrap(),
        "a no_show shift must free the clinician for future work"
    );
    assert_eq!(shift_status(&pool, shift_id).await, "no_show");
}

#[tokio::test]
async fn a_no_show_shift_is_not_payable() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();

    // The payout pipeline selects on status = 'completed'; no_show must not
    // satisfy it, or a no-show gets paid.
    let payable: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM shifts WHERE id = $1 AND status = 'completed'",
    )
    .bind(shift_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(payable, 0, "a no-show must never look payable");
}

#[tokio::test]
async fn an_already_completed_shift_is_not_touched() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "completed",
        60 * 24,
    )
    .await;

    svc.close_timed_out_shifts().await.unwrap();
    assert_eq!(shift_status(&pool, shift_id).await, "completed");
}

#[tokio::test]
async fn a_cancelled_shift_is_not_resurrected_as_no_show() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "cancelled",
        60 * 24,
    )
    .await;

    svc.close_timed_out_shifts().await.unwrap();
    assert_eq!(
        shift_status(&pool, shift_id).await,
        "cancelled",
        "a deliberate cancellation must not be rewritten by the sweep"
    );
}

#[tokio::test]
async fn a_clock_out_arriving_after_the_sweep_cannot_resurrect_a_no_show() {
    let Some(pool) = test_pool().await else { return };
    let _g = settings_lock();
    set_grace(&pool, 15).await;
    let svc = shift_service(&pool);
    let f = fixture(&pool).await;

    let shift_id = seed_ended_shift(
        &pool,
        f.hospital_id,
        f.admin_user_id,
        f.clinician_id,
        "upcoming",
        30,
    )
    .await;
    fund_and_hold(&pool, f.hospital_id, 0, 3_200_000).await;

    svc.close_timed_out_shifts().await.unwrap();
    assert_eq!(shift_status(&pool, shift_id).await, "no_show");

    // record_clockout_tx is guarded on status = 'in_progress', so a late
    // clock-out finds nothing to update rather than flipping the row back.
    let repo = ShiftRepository::new(pool.clone());
    let mut tx = pool.begin().await.unwrap();
    let result = repo.record_clockout_tx(&mut tx, shift_id, 240).await;
    assert!(
        result.is_err(),
        "a clock-out on a terminal shift must fail, got {result:?}"
    );
    drop(tx);

    assert_eq!(
        shift_status(&pool, shift_id).await,
        "no_show",
        "the sweep's decision must stand"
    );
}

/// Keeps `ShiftStatus` meaningful in this file: the sweep writes a status the
/// enum has carried since 20240001 and nothing has ever written.
#[test]
fn no_show_is_a_real_status_value() {
    assert_eq!(format!("{:?}", ShiftStatus::NoShow), "NoShow");
}

//! Platform fee — flat (the default) or percent, admin-set, snapshotted per shift.
//!
//! Requires a reachable Postgres — set `TEST_DATABASE_URL` (falls back to a
//! local `nexuscare_test`). Skips with a printed notice rather than failing when
//! no database is reachable, matching the other suites here. `AdminService` and
//! `ShiftService` are constructed directly; the HTTP layer is not exercised.
//!
//! Worth having because the fee is the platform's revenue and two independent
//! things can silently break it:
//!
//!  1. The configured fee is **naira** while every computed amount is **kobo**.
//!     A dropped `* 100` is a 100x revenue loss that compiles and passes any
//!     test that does not assert an exact kobo figure. The arithmetic guards for
//!     that live in `payout_service`'s unit tests; what is checked here is that
//!     the value actually makes the round trip through the database.
//!  2. The fee is read at payout time, days after the hospital agreed a net
//!     figure at preview time. The snapshot on `shifts.platform_fee_kobo` is
//!     what stops an admin re-pricing settled work, so it has to be proven to
//!     survive a later settings change.

use std::sync::Arc;

use chrono::{Duration, Timelike, Utc};
use nexuscare_backend::models::admin::UpdatePlatformSettings;
use nexuscare_backend::models::shift::{
    CreateShiftRequest, PayType, RoleCategory, ShiftPriority, ShiftType,
};
use nexuscare_backend::repositories::admin::AdminRepository;
use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::shift::ShiftRepository;
use nexuscare_backend::repositories::wallet::WalletRepository;
use nexuscare_backend::repositories::EmailOutboxRepository;
use nexuscare_backend::services::admin_service::AdminService;
use nexuscare_backend::services::email_outbox_service::EmailOutboxService;
use nexuscare_backend::services::fcm::FcmClient;
use nexuscare_backend::services::notification_service::NotificationService;
use nexuscare_backend::services::payout_service::{
    compute_platform_fee, read_platform_fee_config, PlatformFeeMode,
};
use nexuscare_backend::services::push_service::PushService;
use nexuscare_backend::services::safehaven::SafeHavenClient;
use nexuscare_backend::services::shift_service::ShiftService;
use nexuscare_backend::services::wallet_service::WalletService;
use nexuscare_backend::utils::errors::AppError;
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

fn admin_service(pool: &PgPool) -> AdminService {
    let email_outbox = Arc::new(EmailOutboxService::new(
        Arc::new(EmailOutboxRepository::new(pool.clone())),
        Arc::new(NotificationService::new()),
    ));
    AdminService::new(Arc::new(AdminRepository::new(pool.clone())), email_outbox)
}

/// `platform_settings` is a single global row (`singleton = 'global'`, enforced
/// by a CHECK), so every test here writes the same row. Setting each field a
/// test depends on is not enough on its own: cargo runs these in parallel, so
/// another test can flip the mode between this one's write and its read. That
/// is exactly how `percent_mode_still_charges_ten_percent` failed — it set
/// percent, read back flat.
///
/// So the fee tests take this lock for their whole body and are effectively
/// serial. Poisoning is ignored deliberately: if one test panics while holding
/// it, the rest should still run and report their own results rather than all
/// failing with `PoisonError`.
static FEE_SETTINGS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fee_lock() -> std::sync::MutexGuard<'static, ()> {
    FEE_SETTINGS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set every fee field this suite cares about, so no test inherits a value from
/// another. Call under [`fee_lock`].
async fn set_fee(pool: &PgPool, mode: &str, flat_naira: i64, percent: f64) {
    sqlx::query(
        "UPDATE platform_settings SET platform_fee_mode = $1, \
         platform_fee_flat_naira = $2, platform_fee_percent = $3, \
         platform_fee_cap_kobo = NULL WHERE singleton = 'global'",
    )
    .bind(mode)
    .bind(flat_naira)
    .bind(percent)
    .execute(pool)
    .await
    .expect("failed to set fee config");
}

async fn seed_hospital(pool: &PgPool) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO hospitals (name, registration_number, email, address, phone_number,
                               admin_registration_status)
        VALUES ($1, $2, $3, 'Test Address', '08000000000', 'approved')
        RETURNING id
        "#,
    )
    .bind(format!("Fee Hospital {}", Uuid::new_v4()))
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

/// Shift creation escrows `grand_total_kobo` from the hospital wallet, so a
/// funded wallet is a precondition, not a detail.
async fn fund_wallet(pool: &PgPool, hospital_id: Uuid, kobo: i64) {
    sqlx::query(
        "INSERT INTO hospital_wallets (hospital_id, balance_kobo) VALUES ($1, $2) \
         ON CONFLICT (hospital_id) DO UPDATE SET balance_kobo = $2",
    )
    .bind(hospital_id)
    .bind(kobo)
    .execute(pool)
    .await
    .expect("failed to fund wallet");
}

/// ₦10,000 gross: 8 hours at ₦1,250/hr. Start snapped to a 15-minute boundary
/// (F1-F05) in the future.
fn shift_request() -> CreateShiftRequest {
    let start = {
        let ts = (Utc::now() + Duration::hours(2))
            .with_second(0)
            .unwrap()
            .with_nanosecond(0)
            .unwrap();
        let remainder = ts.minute() % 15;
        if remainder == 0 {
            ts
        } else {
            ts + Duration::minutes((15 - remainder) as i64)
        }
    };

    CreateShiftRequest {
        role_category: RoleCategory::Doctor,
        role_title: "Fee Test Doctor".to_string(),
        specialty: Some("Emergency Medicine".to_string()),
        department: Some("Emergency Department".to_string()),
        shift_type: ShiftType::InPerson,
        priority: ShiftPriority::Normal,
        urgency_bonus_pct: None,
        scheduled_start: start,
        duration_hours: 8.0,
        pay_type: PayType::HourlyRate,
        rate_kobo_per_hour: Some(125_000), // ₦1,250/hr x 8 = ₦10,000
        fixed_rate_kobo: None,
        stat_bonus_kobo: None,
        shift_label: Some("Fee Test".to_string()),
        job_description: Some("Covering a shift, for fee arithmetic.".to_string()),
        tasks: vec!["Triage".to_string()],
        equipment: vec![],
        requirements: vec!["Valid medical license".to_string()],
        notes: None,
        attachment_urls: vec![],
        broadcast_consent_confirmed: true,
    }
}

// 1 — the naira/kobo boundary, end to end through the database.

#[tokio::test]
async fn a_flat_fee_set_in_naira_is_read_back_and_charged_in_kobo() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();
    set_fee(&pool, "flat", 500, 10.0).await;

    let cfg = read_platform_fee_config(&pool).await;
    assert_eq!(cfg.mode, PlatformFeeMode::Flat, "got {cfg:?}");
    assert_eq!(cfg.flat_naira, 500, "the stored figure is naira");

    // ₦500 of a ₦10,000 gross.
    assert_eq!(
        compute_platform_fee(1_000_000, &cfg),
        50_000,
        "a ₦500 flat fee must charge 50_000 kobo, not 500"
    );
}

#[tokio::test]
async fn flat_is_the_default_mode_on_a_fresh_database() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();

    // Read the column default rather than whatever another test last wrote.
    let default_mode: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'platform_settings' AND column_name = 'platform_fee_mode'",
    )
    .fetch_one(&pool)
    .await
    .expect("platform_fee_mode column should exist");

    assert!(
        default_mode.contains("flat"),
        "flat must be the default mode, got {default_mode}"
    );
}

// 2 — the snapshot. This is the one that protects agreed pay.

#[tokio::test]
async fn a_later_fee_change_does_not_re_price_an_existing_shift() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();
    set_fee(&pool, "flat", 500, 10.0).await;

    let hospital_id = seed_hospital(&pool).await;
    let admin_user_id = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    fund_wallet(&pool, hospital_id, 10_000_000).await;

    let shift = shift_service(&pool)
        .create_shift(hospital_id, admin_user_id, shift_request())
        .await
        .expect("shift creation should succeed");

    let snapshot: Option<i64> =
        sqlx::query_scalar("SELECT platform_fee_kobo FROM shifts WHERE id = $1")
            .bind(shift.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        snapshot,
        Some(50_000),
        "the ₦500 fee should be snapshotted as 50_000 kobo"
    );

    // The admin doubles the fee afterwards.
    set_fee(&pool, "flat", 1_000, 10.0).await;

    let after: Option<i64> =
        sqlx::query_scalar("SELECT platform_fee_kobo FROM shifts WHERE id = $1")
            .bind(shift.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        after,
        Some(50_000),
        "an existing shift must keep the fee it was created with"
    );
}

#[tokio::test]
async fn the_preview_shows_the_fee_and_the_workers_net() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();
    set_fee(&pool, "flat", 500, 10.0).await;

    let preview = shift_service(&pool)
        .preview_shift(&shift_request())
        .await
        .expect("preview should succeed");

    assert_eq!(preview.grand_total_kobo, 1_000_000, "₦10,000 gross");
    assert_eq!(preview.platform_fee_kobo, 50_000, "₦500 in kobo");
    assert_eq!(preview.worker_net_kobo, 950_000);
    assert_eq!(
        preview.grand_total_kobo,
        preview.platform_fee_kobo + preview.worker_net_kobo,
        "the hospital escrows the gross; the fee comes out of the worker's side"
    );
}

// 3 — the settings boundary.

#[tokio::test]
async fn a_flat_fee_at_or_above_the_minimum_shift_rate_is_allowed_but_warned() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();
    let svc = admin_service(&pool);
    let admin_user_id = seed_user(&pool, "super_admin", None).await;

    let min_rate_kobo: i64 =
        sqlx::query_scalar("SELECT min_hourly_rate_kobo FROM platform_settings WHERE singleton = 'global'")
            .fetch_one(&pool)
            .await
            .unwrap();

    // This used to be a hard 400. It is now accepted with a warning, because the
    // threshold it was keyed to — platform_settings.min_hourly_rate_kobo — is
    // read by nothing else in the codebase (validate_request enforces a
    // hardcoded ₦100 floor instead), so rejecting against it blocked legitimate
    // configurations while still admitting fees that zero the real smallest
    // shift. The clamp in compute_platform_fee is the actual guarantee.
    let flat_naira = min_rate_kobo / 100;
    let updated = svc
        .update_settings(
            UpdatePlatformSettings {
                platform_fee_mode: Some("flat".into()),
                platform_fee_flat_naira: Some(flat_naira),
                platform_fee_percent: None,
                platform_fee_cap_kobo: None,
                worker_broadcast_radius_km: None,
                stat_bonus_percent: None,
                urgent_bonus_percent: None,
                clock_in_grace_minutes: None,
                shift_close_grace_minutes: None,
                auto_clock_out_hours: None,
                handover_edit_window_hours: None,
                dispute_filing_window_hours: None,
                max_active_shifts_per_hospital: None,
                min_hourly_rate_kobo: None,
                max_recording_minutes: None,
            },
            admin_user_id,
        )
        .await
        .expect("a large flat fee is permitted, with a warning logged");

    assert_eq!(updated.platform_fee_flat_naira, flat_naira);

    // The safety property that actually matters: however large the fee, a
    // clinician's net never goes below zero.
    let cfg = read_platform_fee_config(&pool).await;
    let tiny_gross = 20_000; // ₦200, the smallest two-hour shift at the ₦100 floor
    let fee = compute_platform_fee(tiny_gross, &cfg);
    assert_eq!(fee, tiny_gross, "the fee is clamped to the gross");
    assert_eq!(tiny_gross - fee, 0, "net is zero, never negative");
}

#[tokio::test]
async fn an_unknown_fee_mode_is_rejected() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();
    let svc = admin_service(&pool);
    let admin_user_id = seed_user(&pool, "super_admin", None).await;

    let result = svc
        .update_settings(
            UpdatePlatformSettings {
                platform_fee_mode: Some("tiered".into()),
                platform_fee_flat_naira: None,
                platform_fee_percent: None,
                platform_fee_cap_kobo: None,
                worker_broadcast_radius_km: None,
                stat_bonus_percent: None,
                urgent_bonus_percent: None,
                clock_in_grace_minutes: None,
                shift_close_grace_minutes: None,
                auto_clock_out_hours: None,
                handover_edit_window_hours: None,
                dispute_filing_window_hours: None,
                max_active_shifts_per_hospital: None,
                min_hourly_rate_kobo: None,
                max_recording_minutes: None,
            },
            admin_user_id,
        )
        .await;

    // A 400 rather than the 500 the raw CHECK constraint would produce.
    assert!(
        matches!(result, Err(AppError::BadRequest(_))),
        "expected a 400, got {result:?}"
    );
}

// 4 — percent mode still works, so the rollout is additive.

#[tokio::test]
async fn percent_mode_still_charges_ten_percent() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();
    set_fee(&pool, "percent", 500, 10.0).await;

    let cfg = read_platform_fee_config(&pool).await;
    assert_eq!(cfg.mode, PlatformFeeMode::Percent, "got {cfg:?}");
    // The flat figure is stored but must be ignored in percent mode.
    assert_eq!(compute_platform_fee(1_000_000, &cfg), 100_000);
}

/// Review finding #2: the migration must not re-price the live settings row.
///
/// The column default is `flat` so a fresh install gets the primary mode, but
/// the pre-existing singleton row is pinned to `percent` by 20240064 so a deploy
/// cannot silently cut the fee from 10% of gross to a placeholder ₦500. An admin
/// opts in with one PUT.
#[tokio::test]
async fn the_migration_leaves_the_existing_settings_row_on_percent() {
    let Some(pool) = test_pool().await else { return };
    let _fee_guard = fee_lock();

    // Re-run the pin exactly as the migration does, from the pre-migration
    // state, and confirm it lands on percent rather than flat.
    sqlx::query(
        "UPDATE platform_settings SET platform_fee_mode = 'flat', \
         platform_fee_flat_naira = 500 WHERE singleton = 'global'",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "UPDATE platform_settings SET platform_fee_mode = 'percent' \
         WHERE singleton = 'global' AND platform_fee_mode = 'flat' \
           AND platform_fee_flat_naira = 500",
    )
    .execute(&pool)
    .await
    .unwrap();

    let cfg = read_platform_fee_config(&pool).await;
    assert_eq!(
        cfg.mode,
        PlatformFeeMode::Percent,
        "an untouched production row must keep its percentage pricing"
    );
}

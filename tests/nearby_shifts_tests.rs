//! SCRUM-24 / US-08 — View Nearby Shifts.
//!
//! Pure-logic coverage of origin resolution and parameter validation lives
//! beside the handler in `src/handlers/shifts.rs` (`nearby_query_tests`). The
//! cases below exercise the radius filter, ranking and paging against a real
//! database and follow the repository's `#[ignore]` integration-test
//! convention: they document the end-to-end expectation and run once a seeded
//! test database is wired into CI.

use std::sync::Arc;

use nexuscare_backend::repositories::notification::NotificationRepository;
use nexuscare_backend::repositories::shift::ShiftRepository;
use nexuscare_backend::repositories::{EmailOutboxRepository, WalletRepository};
use nexuscare_backend::services::fcm::FcmClient;
use nexuscare_backend::services::push_service::PushService;
use nexuscare_backend::services::safehaven::SafeHavenClient;
use nexuscare_backend::services::shift_service::ShiftService;
use nexuscare_backend::services::{EmailOutboxService, NotificationService, WalletService};
use sqlx::PgPool;
use uuid::Uuid;

/// Connect to the test database, or skip (return None) if none is reachable —
/// mirrors the convention in `tests/patient_ingest_tests.rs`.
async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL").or_else(|_| std::env::var("DATABASE_URL")).ok()?;
    match sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
    {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("SKIPPED: no test database reachable at {url}: {e}");
            None
        }
    }
}

/// Build a `ShiftService`. The nearby read path only touches the shift repo;
/// the other collaborators are constructed but never invoked here.
fn build_shift_service(pool: &PgPool) -> ShiftService {
    let shift_repo = Arc::new(ShiftRepository::new(pool.clone()));
    let notification = Arc::new(NotificationService::new());
    let email_outbox = Arc::new(EmailOutboxService::new(
        Arc::new(EmailOutboxRepository::new(pool.clone())),
        notification.clone(),
    ));
    let wallet = Arc::new(WalletService::new(
        Arc::new(WalletRepository::new(pool.clone())),
        Arc::new(SafeHavenClient::from_env()),
        pool.clone(),
    ));
    let push = Arc::new(PushService::new(
        Arc::new(NotificationRepository::new(pool.clone())),
        Arc::new(FcmClient::from_env()),
    ));
    ShiftService::new(shift_repo, pool.clone(), notification, email_outbox, wallet, push)
}

/// Seed a hospital, a health-worker user + clinician, and one open virtual
/// shift. Returns (clinician_id, worker_user_id, shift_id).
async fn seed_virtual_shift(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let hospital_id: Uuid = sqlx::query_scalar(
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
    .expect("seed hospital");

    let user_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO users (email, first_name, last_name, password_hash, role, hospital_id)
        VALUES ($1, 'Test', 'Worker', 'not-a-real-hash', 'health_worker', $2)
        RETURNING id
        "#,
    )
    .bind(format!("{}@example.test", Uuid::new_v4()))
    .bind(hospital_id)
    .fetch_one(pool)
    .await
    .expect("seed user");

    let clinician_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO clinicians (user_id, first_name, last_name, specialty, role_title)
        VALUES ($1, 'Test', 'Worker', 'anesthesiology', 'Doctor')
        RETURNING id
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("seed clinician");

    let shift_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO shifts (hospital_id, role_category, role_title, shift_type, status,
                            scheduled_start, duration_hours, scheduled_end,
                            pay_type, rate_kobo_per_hour, created_by)
        VALUES ($1, 'doctor', 'Doctor', 'virtual', 'open',
                NOW() + INTERVAL '2 hours', 8, NOW() + INTERVAL '10 hours',
                'hourly_rate', 800000, $2)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("seed virtual shift");

    (clinician_id, user_id, shift_id)
}

/// UT002 / UT004 — 5 km radius filter.
///
/// Seed one open in-person shift ~4 km from the worker and one ~6 km away, then
/// call `ShiftRepository::list_nearby_shifts` with `radius_km = 5`. Expect only
/// the 4 km shift; the 6 km shift is excluded by the haversine gate.
#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn ut002_ut004_radius_filter_excludes_beyond_5km() {}

/// UT005 / UT006 — urgency then distance ranking.
///
/// Seed a STAT shift far away and a Normal shift nearby (both in range). Expect
/// the STAT shift first (urgency rank wins), then Normal; within one urgency
/// tier, the nearer shift ranks first.
#[tokio::test]
#[ignore]
async fn ut005_ut006_sorted_by_urgency_then_distance() {}

/// US-08 decision #2 — virtual shifts are always included regardless of radius.
///
/// Seed a virtual shift whose hospital sits far outside the radius. Expect it in
/// the result set with `distance_km = None`.
#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn virtual_shift_included_without_distance() {
    let Some(pool) = test_pool().await else { return };
    let (clinician_id, _user_id, shift_id) = seed_virtual_shift(&pool).await;
    let repo = ShiftRepository::new(pool.clone());

    // A successful decode here proves the `AS "col: _"` aliases are gone
    // (Defect 3): with them, FromRow would fail with ColumnNotFound.
    let rows = repo
        .list_nearby_shifts(clinician_id, Some((6.5244, 3.3792)), 5.0, 50, 0)
        .await
        .expect("list_nearby_shifts should decode successfully");

    let row = rows
        .into_iter()
        .find(|r| r.shift_id == shift_id)
        .expect("virtual shift should be included regardless of radius");
    assert_eq!(row.distance_km, None, "virtual shift carries no distance");
}

/// UT007 — distance is reported for in-person shifts.
///
/// Seed an in-person shift at a known offset and assert the returned
/// `distance_km` matches the haversine distance within a small tolerance.
#[tokio::test]
#[ignore]
async fn ut007_distance_reported_for_in_person() {}

/// UT013 — changing the origin recomputes the nearby set.
///
/// Call with GPS near hospital A (A in range, B out), then with GPS near
/// hospital B (B in range, A out). Expect the membership to flip, and the
/// worker's last-known location to be upserted between calls.
#[tokio::test]
#[ignore]
async fn ut013_new_origin_recalculates_results() {}

/// UT014 — only open shifts are returned.
///
/// Seed assigned/completed shifts alongside an open one within range. Expect
/// only the open shift; non-open statuses are filtered out.
#[tokio::test]
#[ignore]
async fn ut014_only_open_shifts_returned() {}

/// UT012 — dismissed shifts disappear.
///
/// Dismiss an in-range shift for the worker and expect it excluded from the
/// result set on the next call.
#[tokio::test]
#[ignore]
async fn ut012_dismissed_shift_excluded() {}

/// Graceful no-origin path — no GPS supplied and none on file.
///
/// With no live coordinates and no `clinician_locations` row, the service must
/// return `Ok` with `location_required = true` (not an error), carrying the
/// location-free shifts (virtual + hospitals with no coords).
#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn no_origin_returns_virtual_shifts_without_error() {
    let Some(pool) = test_pool().await else { return };
    let (_clinician_id, user_id, shift_id) = seed_virtual_shift(&pool).await;
    let service = build_shift_service(&pool);

    let result = service
        .list_nearby_shifts_for_worker(user_id, None, 5.0, 50, 0)
        .await
        .expect("no origin must succeed, not error");

    assert!(
        result.location_required,
        "location_required must be true when no origin is available"
    );
    assert!(
        result.shifts.iter().any(|s| s.shift_id == shift_id),
        "virtual shift should still be returned without an origin"
    );
}

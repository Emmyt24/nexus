//! Hospital-scoped search — `GET /api/v1/search`.
//!
//! Requires a reachable Postgres — set `TEST_DATABASE_URL` (falls back to a
//! local `nexuscare_test`). Skips with a printed notice rather than failing when
//! no database is reachable. `SearchService` is constructed directly.
//!
//! Worth having because a search endpoint is the worst possible place for a
//! missing tenant predicate: it returns whatever matches, so one forgotten
//! `AND hospital_id = $1` turns into a cross-tenant dump that looks like a
//! working feature. There is no RLS behind these queries — the predicates in
//! `repositories::search` are the only thing separating two hospitals.
//!
//! The `clinicians` table is the sharp case. It has no `hospital_id` at all: it
//! is a platform-wide marketplace pool, so "scoped" there means scoped by
//! *relationship*, and the tests below pin both halves — a clinician who has
//! worked with the hospital is found, one who has not is invisible, and neither
//! case ever exposes an email or licence number.

use std::sync::Arc;

use chrono::{Duration, Utc};
use nexuscare_backend::models::user::{Claims, UserRole};
use nexuscare_backend::repositories::search::SearchRepository;
use nexuscare_backend::services::search_service::SearchService;
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

fn search_service(pool: &PgPool) -> SearchService {
    SearchService::new(Arc::new(SearchRepository::new(pool.clone())))
}

fn claims(user_id: Uuid, role: UserRole, hospital_id: Option<Uuid>) -> Claims {
    Claims {
        sub: user_id.to_string(),
        email: format!("{user_id}@example.test"),
        role,
        hospital_id: hospital_id.map(|h| h.to_string()),
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
    .bind(format!("Search Hospital {}", Uuid::new_v4()))
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

/// A clinician whose email and licence number are *distinctive*, so a leak
/// through the search projection would be unmistakable in an assertion.
async fn seed_clinician(pool: &PgPool, last_name: &str) -> (Uuid, String, String) {
    let email = format!("leaky-{}@example.test", Uuid::new_v4());
    let licence = format!("MDCN-LEAK-{}", &Uuid::new_v4().to_string()[..8]);

    let user_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO users (email, first_name, last_name, password_hash, role)
        VALUES ($1, 'Chidi', $2, 'not-a-real-hash', 'health_worker')
        RETURNING id
        "#,
    )
    .bind(&email)
    .bind(last_name)
    .fetch_one(pool)
    .await
    .expect("failed to seed clinician user");

    let clinician_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO clinicians (
            user_id, first_name, last_name, specialty, role_title,
            license_number, clinician_role
        )
        VALUES ($1, 'Chidi', $2, 'emergency_medicine', 'Emergency Doctor', $3, 'doctor')
        RETURNING id
        "#,
    )
    .bind(user_id)
    .bind(last_name)
    .bind(&licence)
    .fetch_one(pool)
    .await
    .expect("failed to seed clinician");

    (clinician_id, email, licence)
}

async fn seed_shift(
    pool: &PgPool,
    hospital_id: Uuid,
    created_by: Uuid,
    role_title: &str,
    assigned_clinician_id: Option<Uuid>,
) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, specialty, department, shift_type,
            status, scheduled_start, duration_hours, scheduled_end,
            assigned_clinician_id, pay_type, rate_kobo_per_hour, grand_total_kobo, created_by
        )
        VALUES ($1, 'doctor', $2, 'Emergency Medicine', 'Emergency Department', 'in_person',
                'open', NOW(), 4, NOW() + INTERVAL '4 hours',
                $3, 'hourly_rate', 800000, 3200000, $4)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(role_title)
    .bind(assigned_clinician_id)
    .bind(created_by)
    .fetch_one(pool)
    .await
    .expect("failed to seed shift")
}

async fn seed_patient(pool: &PgPool, hospital_id: Uuid, created_by: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO patients (hospital_id, full_name, age, gender, symptoms, created_by)
        VALUES ($1, $2, 44, 'female', 'Persistent headache', $3)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(name)
    .bind(created_by)
    .fetch_one(pool)
    .await
    .expect("failed to seed patient")
}

async fn seed_handover(pool: &PgPool, shift_id: Uuid, instructions: &str) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO shift_handovers (
            shift_id, patients_seen, instructions, editable_until, auto_approve_after
        )
        VALUES ($1, 3, $2, NOW() + INTERVAL '1 hour', NOW() + INTERVAL '48 hours')
        RETURNING id
        "#,
    )
    .bind(shift_id)
    .bind(instructions)
    .fetch_one(pool)
    .await
    .expect("failed to seed handover")
}

/// A distinctive token so matches can only come from rows this test seeded.
fn token() -> String {
    format!("Zz{}", &Uuid::new_v4().to_string()[..8]).replace('-', "")
}

// 1 — the cross-tenant boundary. These are the regression.

#[tokio::test]
async fn a_hospital_finds_its_own_shift_by_title() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let needle = token();
    let shift_id = seed_shift(&pool, hospital_id, admin, &format!("{needle} Doctor"), None).await;

    let results = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            &needle,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert_eq!(results.shifts.len(), 1, "got {:?}", results.shifts);
    assert_eq!(results.shifts[0].id, shift_id);
    assert_eq!(results.shifts[0].kind, "shift");
}

#[tokio::test]
async fn a_hospital_does_not_see_another_hospitals_shift() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let needle = token();

    let theirs = seed_hospital(&pool).await;
    let their_admin = seed_user(&pool, "hospital_admin", Some(theirs)).await;
    seed_shift(&pool, theirs, their_admin, &format!("{needle} Doctor"), None).await;

    let mine = seed_hospital(&pool).await;
    let my_admin = seed_user(&pool, "hospital_admin", Some(mine)).await;

    let results = svc
        .search(
            &claims(my_admin, UserRole::HospitalAdmin, Some(mine)),
            &needle,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert!(
        results.shifts.is_empty(),
        "another hospital's shift leaked: {:?}",
        results.shifts
    );
}

#[tokio::test]
async fn a_hospital_does_not_see_another_hospitals_patient() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let needle = token();

    let theirs = seed_hospital(&pool).await;
    let their_admin = seed_user(&pool, "hospital_admin", Some(theirs)).await;
    seed_patient(&pool, theirs, their_admin, &format!("{needle} Patient")).await;

    let mine = seed_hospital(&pool).await;
    let my_admin = seed_user(&pool, "hospital_admin", Some(mine)).await;

    let results = svc
        .search(
            &claims(my_admin, UserRole::HospitalAdmin, Some(mine)),
            &needle,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert!(
        results.patients.is_empty(),
        "another hospital's patient leaked: {:?}",
        results.patients
    );
}

#[tokio::test]
async fn a_hospital_does_not_see_another_hospitals_handover() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let needle = token();

    let theirs = seed_hospital(&pool).await;
    let their_admin = seed_user(&pool, "hospital_admin", Some(theirs)).await;
    let their_shift = seed_shift(&pool, theirs, their_admin, "Emergency Doctor", None).await;
    seed_handover(&pool, their_shift, &format!("Watch bed 4, {needle}")).await;

    let mine = seed_hospital(&pool).await;
    let my_admin = seed_user(&pool, "hospital_admin", Some(mine)).await;

    let results = svc
        .search(
            &claims(my_admin, UserRole::HospitalAdmin, Some(mine)),
            &needle,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert!(
        results.handovers.is_empty(),
        "another hospital's handover leaked: {:?}",
        results.handovers
    );
}

// 2 — clinicians: relationship scoping, because the table has no tenant column.

#[tokio::test]
async fn a_clinician_who_worked_here_is_found() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let surname = token();
    let (clinician_id, _, _) = seed_clinician(&pool, &surname).await;
    seed_shift(
        &pool,
        hospital_id,
        admin,
        "Emergency Doctor",
        Some(clinician_id),
    )
    .await;

    let results = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            &surname,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert_eq!(results.clinicians.len(), 1, "got {:?}", results.clinicians);
    assert_eq!(results.clinicians[0].id, clinician_id);
}

#[tokio::test]
async fn a_clinician_with_no_relationship_to_this_hospital_is_invisible() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    // A clinician on the platform who has never dealt with this hospital. The
    // admin search would return them; this one must not, or every hospital can
    // enumerate the national clinician roster.
    let surname = token();
    seed_clinician(&pool, &surname).await;

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;

    let results = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            &surname,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert!(
        results.clinicians.is_empty(),
        "an unrelated clinician leaked: {:?}",
        results.clinicians
    );
}

#[tokio::test]
async fn a_clinician_who_only_expressed_interest_counts_as_a_relationship() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let surname = token();
    let (clinician_id, _, _) = seed_clinician(&pool, &surname).await;
    let shift_id = seed_shift(&pool, hospital_id, admin, "Emergency Doctor", None).await;

    sqlx::query("INSERT INTO shift_interests (shift_id, clinician_id) VALUES ($1, $2)")
        .bind(shift_id)
        .bind(clinician_id)
        .execute(&pool)
        .await
        .unwrap();

    let results = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            &surname,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert_eq!(
        results.clinicians.len(),
        1,
        "someone who applied to our shift should be findable; got {:?}",
        results.clinicians
    );
}

#[tokio::test]
async fn a_clinician_result_never_carries_an_email_or_licence_number() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let surname = token();
    let (clinician_id, email, licence) = seed_clinician(&pool, &surname).await;
    seed_shift(
        &pool,
        hospital_id,
        admin,
        "Emergency Doctor",
        Some(clinician_id),
    )
    .await;

    let results = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            &surname,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    // Serialise the whole response and check the secrets are nowhere in it —
    // stronger than asserting on individual fields, and it keeps holding if
    // someone widens the projection later.
    let body = serde_json::to_string(&results).unwrap();
    assert!(
        !body.contains(&email),
        "a clinician's email must not appear in a hospital search response"
    );
    assert!(
        !body.contains(&licence),
        "a clinician's licence number must not appear in a hospital search response"
    );
}

// 3 — the query itself.

#[tokio::test]
async fn a_one_character_query_is_refused() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;

    let result = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            "a",
            None,
            None,
        )
        .await;

    assert!(matches!(result, Err(AppError::Validation(_))), "got {result:?}");
}

#[tokio::test]
async fn a_wildcard_query_does_not_match_everything() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    seed_shift(&pool, hospital_id, admin, "Emergency Doctor", None).await;
    seed_patient(&pool, hospital_id, admin, "Ngozi Eze").await;

    // `%%` is two characters, so it passes the length check and reaches the SQL.
    // Unescaped it would match every row the tenant owns.
    let results = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            "%%",
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert!(
        results.shifts.is_empty() && results.patients.is_empty(),
        "LIKE metacharacters must be matched literally, got shifts={:?} patients={:?}",
        results.shifts,
        results.patients
    );
}

#[tokio::test]
async fn types_narrows_which_buckets_are_queried() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let needle = token();
    seed_shift(&pool, hospital_id, admin, &format!("{needle} Doctor"), None).await;
    seed_patient(&pool, hospital_id, admin, &format!("{needle} Patient")).await;

    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let all = svc.search(&c, &needle, None, None).await.unwrap();
    assert_eq!(all.shifts.len(), 1);
    assert_eq!(all.patients.len(), 1);

    let only_shifts = svc.search(&c, &needle, Some("shift"), None).await.unwrap();
    assert_eq!(only_shifts.shifts.len(), 1);
    assert!(
        only_shifts.patients.is_empty(),
        "patients should not have been queried"
    );
}

#[tokio::test]
async fn an_unknown_type_is_refused_rather_than_ignored() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;

    let result = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            "emergency",
            Some("shift,wallet"),
            None,
        )
        .await;

    assert!(matches!(result, Err(AppError::Validation(_))), "got {result:?}");
}

#[tokio::test]
async fn the_per_bucket_limit_is_capped() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let needle = token();
    for _ in 0..3 {
        seed_shift(&pool, hospital_id, admin, &format!("{needle} Doctor"), None).await;
    }

    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let two = svc.search(&c, &needle, Some("shift"), Some(2)).await.unwrap();
    assert_eq!(two.shifts.len(), 2, "limit should be honoured");

    // Absurd and negative limits are clamped, not passed to SQL.
    let huge = svc
        .search(&c, &needle, Some("shift"), Some(10_000))
        .await
        .unwrap();
    assert_eq!(huge.shifts.len(), 3);
    let negative = svc.search(&c, &needle, Some("shift"), Some(-5)).await.unwrap();
    assert_eq!(negative.shifts.len(), 1, "a negative limit clamps to 1");
}

// 4 — patients are role-gated even inside the right tenant.

#[tokio::test]
async fn a_health_worker_cannot_search_at_all() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);
    let worker = seed_user(&pool, "health_worker", None).await;

    let result = svc
        .search(&claims(worker, UserRole::HealthWorker, None), "emergency", None, None)
        .await;

    assert!(matches!(result, Err(AppError::Forbidden(_))), "got {result:?}");
}

#[tokio::test]
async fn a_platform_admin_is_refused_rather_than_shown_every_hospital() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);
    let ops = seed_user(&pool, "operations_admin", None).await;

    // The dangerous failure would be treating "no hospital" as "all hospitals".
    let result = svc
        .search(&claims(ops, UserRole::OperationsAdmin, None), "emergency", None, None)
        .await;

    assert!(matches!(result, Err(AppError::Forbidden(_))), "got {result:?}");
}

#[tokio::test]
async fn a_patient_is_matched_by_name_and_not_by_symptoms() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let name = token();
    seed_patient(&pool, hospital_id, admin, &format!("{name} Adeyemi")).await;

    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let by_name = svc.search(&c, &name, Some("patient"), None).await.unwrap();
    assert_eq!(by_name.patients.len(), 1, "name should match");

    // 'Persistent headache' is the seeded symptom text. Searching it must not
    // surface the patient: a general search box that matches clinical details
    // lets anyone enumerate patients by condition.
    let by_symptom = svc
        .search(&c, "Persistent headache", Some("patient"), None)
        .await
        .unwrap();
    assert!(
        by_symptom.patients.is_empty(),
        "patients must not be searchable by symptoms, got {:?}",
        by_symptom.patients
    );
}

#[tokio::test]
async fn the_handover_bucket_finds_notes_on_our_own_shifts() {
    let Some(pool) = test_pool().await else { return };
    let svc = search_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let shift_id = seed_shift(&pool, hospital_id, admin, "Emergency Doctor", None).await;
    let needle = token();
    let handover_id = seed_handover(&pool, shift_id, &format!("Chase bloods for {needle}")).await;

    let results = svc
        .search(
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            &needle,
            None,
            None,
        )
        .await
        .expect("search should succeed");

    assert_eq!(results.handovers.len(), 1, "got {:?}", results.handovers);
    assert_eq!(results.handovers[0].id, handover_id);
    assert_eq!(
        results.handovers[0].shift_id,
        Some(shift_id),
        "a handover hit should link back to its shift"
    );
}

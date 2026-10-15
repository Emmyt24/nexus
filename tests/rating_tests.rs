//! Mutual shift ratings — FRS §3.9, BR-F1-46 to BR-F1-51.
//!
//! Requires a reachable Postgres — set `TEST_DATABASE_URL` (falls back to a
//! local `nexuscare_test`). Skips with a printed notice rather than failing when
//! no database is reachable. `ShiftService` is constructed directly; the HTTP
//! layer is not exercised.
//!
//! Worth having because the rating surface shipped with no coverage at all —
//! `shift_detail_tests.rs` holds an empty `#[ignore]`d stub where this should
//! have been — and it carries three rules that are invisible until they are
//! wrong: who at a hospital is allowed to rate, how long the window stays open,
//! and whether an anonymous rater can be identified from the response.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nexuscare_backend::models::shift::{
    EditRatingRequest, HospitalRatingDimensions, RateHospitalRequest, RateWorkerRequest,
};
use nexuscare_backend::models::user::{Claims, UserRole};
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
    .bind(format!("Rating Hospital {}", Uuid::new_v4()))
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
        VALUES ($1, 'Ada', 'Okeke', 'not-a-real-hash', $2::user_role, $3)
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
        VALUES ($1, 'Amina', 'Bello', 'emergency_medicine', 'Emergency Doctor',
                $2, 'doctor')
        RETURNING id
        "#,
    )
    .bind(user_id)
    .bind(format!("MDCN-{}", &Uuid::new_v4().to_string()[..8]))
    .fetch_one(pool)
    .await
    .expect("failed to seed clinician")
}

/// A completed shift with `completed_at` set, which is what the rating windows
/// are measured from.
async fn seed_completed_shift(
    pool: &PgPool,
    hospital_id: Uuid,
    created_by: Uuid,
    clinician_id: Uuid,
    completed_at: DateTime<Utc>,
) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, shift_type, status,
            scheduled_start, duration_hours, scheduled_end,
            assigned_clinician_id, pay_type, rate_kobo_per_hour,
            grand_total_kobo, created_by, completed_at
        )
        VALUES ($1, 'doctor', 'Emergency Doctor', 'in_person', 'completed',
                $4 - INTERVAL '4 hours', 4, $4,
                $2, 'hourly_rate', 800000, 3200000, $3, $4)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(clinician_id)
    .bind(created_by)
    .bind(completed_at)
    .fetch_one(pool)
    .await
    .expect("failed to seed completed shift")
}

struct Fixture {
    hospital_id: Uuid,
    /// The admin who posted the shift.
    creator_user_id: Uuid,
    /// A second admin at the same hospital who did *not* post it.
    colleague_user_id: Uuid,
    worker_user_id: Uuid,
    clinician_id: Uuid,
    shift_id: Uuid,
}

async fn seed_fixture_completed_at(pool: &PgPool, completed_at: DateTime<Utc>) -> Fixture {
    let hospital_id = seed_hospital(pool).await;
    let creator_user_id = seed_user(pool, "hospital_admin", Some(hospital_id)).await;
    let colleague_user_id = seed_user(pool, "hospital_admin", Some(hospital_id)).await;
    let worker_user_id = seed_user(pool, "health_worker", None).await;
    let clinician_id = seed_clinician(pool, worker_user_id).await;
    let shift_id =
        seed_completed_shift(pool, hospital_id, creator_user_id, clinician_id, completed_at).await;

    Fixture {
        hospital_id,
        creator_user_id,
        colleague_user_id,
        worker_user_id,
        clinician_id,
        shift_id,
    }
}

async fn seed_fixture(pool: &PgPool) -> Fixture {
    seed_fixture_completed_at(pool, Utc::now()).await
}

fn worker_rating(score: i16) -> RateWorkerRequest {
    RateWorkerRequest {
        score,
        comment: Some("Punctual and professional.".to_string()),
        is_anonymous: true,
    }
}

fn hospital_rating(score: i16) -> RateHospitalRequest {
    RateHospitalRequest {
        score,
        comment: Some("Well-organised department.".to_string()),
        dimensions: HospitalRatingDimensions {
            staff_support: 4,
            equipment_availability: 5,
            communication: 4,
            payment_timeliness: 5,
        },
        is_anonymous: true,
    }
}

// 1 — the authorization fix. Any admin at the hospital may rate, not only the
//     one who happened to post the shift. The foreign-hospital case is the
//     regression that matters.

#[tokio::test]
async fn the_admin_who_posted_the_shift_can_rate_the_worker() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let rating = svc
        .rate_worker(
            f.shift_id,
            &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
            worker_rating(5),
        )
        .await
        .expect("the posting admin should be able to rate");

    assert_eq!(rating.score, 5);
    assert_eq!(rating.ratee_id, f.clinician_id);
    assert_eq!(rating.ratee_kind, "clinician");
}

#[tokio::test]
async fn a_second_admin_at_the_same_hospital_can_also_rate_the_worker() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    // Before the tenant fix this 403'd: the check compared `shift.created_by`
    // against the caller, so a colleague passed `require_role` and was then
    // refused by the service.
    let rating = svc
        .rate_worker(
            f.shift_id,
            &claims(
                f.colleague_user_id,
                UserRole::HospitalAdmin,
                Some(f.hospital_id),
            ),
            worker_rating(4),
        )
        .await
        .expect("a colleague at the same hospital should be able to rate");

    assert_eq!(rating.score, 4);
}

#[tokio::test]
async fn an_admin_at_a_foreign_hospital_cannot_rate_the_worker() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let other_hospital = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other_hospital)).await;

    let result = svc
        .rate_worker(
            f.shift_id,
            &claims(intruder, UserRole::HospitalAdmin, Some(other_hospital)),
            worker_rating(1),
        )
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::NotAuthorized)),
        "got {result:?}"
    );

    // And nothing was written.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM shift_ratings WHERE shift_id = $1")
        .bind(f.shift_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a refused rating must leave no row behind");
}

#[tokio::test]
async fn a_health_worker_cannot_use_the_worker_rating_path() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    // The route guard already excludes this role; the service must not rely on
    // the guard alone.
    let result = svc
        .rate_worker(
            f.shift_id,
            &claims(f.worker_user_id, UserRole::HealthWorker, None),
            worker_rating(5),
        )
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::NotAuthorized)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn the_assigned_clinician_can_rate_the_hospital() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let rating = svc
        .rate_hospital(f.shift_id, f.worker_user_id, hospital_rating(4))
        .await
        .expect("the assigned clinician should be able to rate the hospital");

    assert_eq!(rating.ratee_id, f.hospital_id);
    assert_eq!(rating.ratee_kind, "hospital");
    let dims = rating.dimensions.expect("dimensions should be stored");
    assert_eq!(dims["staff_support"], 4);
    assert_eq!(dims["payment_timeliness"], 5);
}

#[tokio::test]
async fn an_unassigned_clinician_cannot_rate_the_hospital() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let stranger_user = seed_user(&pool, "health_worker", None).await;
    seed_clinician(&pool, stranger_user).await;

    let result = svc
        .rate_hospital(f.shift_id, stranger_user, hospital_rating(1))
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::NotAuthorized)),
        "got {result:?}"
    );
}

// 2 — the window anchor. This is the bug the `completed_at` column fixes.

#[tokio::test]
async fn the_seven_day_window_is_measured_from_completion() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);

    // Completed eight days ago: outside BR-F1-46's seven days.
    let f = seed_fixture_completed_at(&pool, Utc::now() - Duration::days(8)).await;

    let result = svc
        .rate_worker(
            f.shift_id,
            &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
            worker_rating(5),
        )
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::RatingWindowClosed)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn touching_the_shift_row_does_not_reopen_a_closed_window() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture_completed_at(&pool, Utc::now() - Duration::days(8)).await;

    // An unrelated write bumps `updated_at` to now. When the window was
    // anchored to `updated_at` this silently reopened rating for another week.
    sqlx::query("UPDATE shifts SET notes = 'touched', updated_at = NOW() WHERE id = $1")
        .bind(f.shift_id)
        .execute(&pool)
        .await
        .unwrap();

    let result = svc
        .rate_worker(
            f.shift_id,
            &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
            worker_rating(5),
        )
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::RatingWindowClosed)),
        "an unrelated update must not reopen the window; got {result:?}"
    );
}

#[tokio::test]
async fn a_shift_that_is_not_completed_cannot_be_rated() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let worker = seed_user(&pool, "health_worker", None).await;
    let clinician_id = seed_clinician(&pool, worker).await;

    let shift_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, shift_type, status,
            scheduled_start, duration_hours, scheduled_end,
            assigned_clinician_id, pay_type, rate_kobo_per_hour,
            grand_total_kobo, created_by
        )
        VALUES ($1, 'doctor', 'Emergency Doctor', 'in_person', 'in_progress',
                NOW(), 4, NOW() + INTERVAL '4 hours',
                $2, 'hourly_rate', 800000, 3200000, $3)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(clinician_id)
    .bind(admin)
    .fetch_one(&pool)
    .await
    .unwrap();

    let result = svc
        .rate_worker(
            shift_id,
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            worker_rating(5),
        )
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::InvalidStatus(_))),
        "got {result:?}"
    );
}

// 3 — one rating per party per shift.

#[tokio::test]
async fn the_same_party_cannot_rate_a_shift_twice() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;
    let c = claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id));

    svc.rate_worker(f.shift_id, &c, worker_rating(5))
        .await
        .expect("first rating should succeed");

    let second = svc.rate_worker(f.shift_id, &c, worker_rating(1)).await;
    assert!(
        matches!(second, Err(ShiftServiceError::DuplicateRating)),
        "got {second:?}"
    );
}

// 4 — the edit window, and withdrawing a comment.

#[tokio::test]
async fn the_rater_can_edit_inside_the_48_hour_window() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let rating = svc
        .rate_worker(
            f.shift_id,
            &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
            worker_rating(3),
        )
        .await
        .unwrap();

    let edited = svc
        .edit_rating(
            rating.id,
            f.creator_user_id,
            EditRatingRequest {
                score: Some(5),
                comment: Some("On reflection, excellent.".to_string()),
                dimensions: None,
                clear_comment: false,
            },
        )
        .await
        .expect("edit inside the window should succeed");

    assert_eq!(edited.score, 5);
    assert_eq!(edited.comment.as_deref(), Some("On reflection, excellent."));
}

#[tokio::test]
async fn a_comment_can_be_withdrawn_not_just_replaced() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let rating = svc
        .rate_worker(
            f.shift_id,
            &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
            worker_rating(2),
        )
        .await
        .unwrap();

    let edited = svc
        .edit_rating(
            rating.id,
            f.creator_user_id,
            EditRatingRequest {
                score: None,
                comment: None,
                dimensions: None,
                clear_comment: true,
            },
        )
        .await
        .expect("clearing a comment should succeed");

    assert_eq!(edited.comment, None, "the comment should be gone");
    assert_eq!(edited.score, 2, "clearing the comment must not change the score");

    // Confirm at the row, not just in the response.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT comment FROM shift_ratings WHERE id = $1")
            .bind(rating.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, None);
}

#[tokio::test]
async fn someone_who_did_not_write_a_rating_cannot_edit_it() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let rating = svc
        .rate_worker(
            f.shift_id,
            &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
            worker_rating(5),
        )
        .await
        .unwrap();

    // A colleague may *submit* a rating for the hospital, but editing is still
    // personal to whoever wrote it.
    let result = svc
        .edit_rating(
            rating.id,
            f.colleague_user_id,
            EditRatingRequest {
                score: Some(1),
                comment: None,
                dimensions: None,
                clear_comment: false,
            },
        )
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::NotAuthorized)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn the_rater_cannot_edit_after_the_window_closes() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    let rating = svc
        .rate_worker(
            f.shift_id,
            &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
            worker_rating(5),
        )
        .await
        .unwrap();

    // Close the window surgically rather than waiting 48 hours.
    sqlx::query("UPDATE shift_ratings SET editable_until = NOW() - INTERVAL '1 minute' WHERE id = $1")
        .bind(rating.id)
        .execute(&pool)
        .await
        .unwrap();

    let result = svc
        .edit_rating(
            rating.id,
            f.creator_user_id,
            EditRatingRequest {
                score: Some(1),
                comment: None,
                dimensions: None,
                clear_comment: false,
            },
        )
        .await;

    assert!(
        matches!(result, Err(ShiftServiceError::RatingEditWindowClosed)),
        "got {result:?}"
    );
}

// 5 — the read path, which did not exist. Anonymity is the load-bearing part.

#[tokio::test]
async fn a_clinicians_ratings_can_be_read_back_with_the_aggregate() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    svc.rate_worker(
        f.shift_id,
        &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
        worker_rating(4),
    )
    .await
    .unwrap();

    let feed = svc
        .list_ratings("clinician", f.clinician_id, 20, 0)
        .await
        .expect("the feed should be readable");

    assert_eq!(feed.total, 1);
    assert_eq!(feed.average, Some(4.0));
    assert_eq!(feed.items.len(), 1);
    let item = &feed.items[0];
    assert_eq!(item.score, 4);
    assert_eq!(
        item.comment.as_deref(),
        Some("Punctual and professional."),
        "the comment is the whole point of the read path"
    );
    assert_eq!(
        item.shift_role_title.as_deref(),
        Some("Emergency Doctor"),
        "context for the reader"
    );
}

#[tokio::test]
async fn an_anonymous_rating_never_names_its_author() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    svc.rate_worker(
        f.shift_id,
        &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
        RateWorkerRequest {
            score: 2,
            comment: Some("Late twice.".to_string()),
            is_anonymous: true,
        },
    )
    .await
    .unwrap();

    let feed = svc.list_ratings("clinician", f.clinician_id, 20, 0).await.unwrap();
    assert_eq!(
        feed.items[0].rater_name, None,
        "BR-F1-48: an anonymous rater must not be identifiable"
    );
}

#[tokio::test]
async fn a_rater_who_waives_anonymity_is_named() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    svc.rate_worker(
        f.shift_id,
        &claims(f.creator_user_id, UserRole::HospitalAdmin, Some(f.hospital_id)),
        RateWorkerRequest {
            score: 5,
            comment: Some("Would hire again.".to_string()),
            is_anonymous: false,
        },
    )
    .await
    .unwrap();

    let feed = svc.list_ratings("clinician", f.clinician_id, 20, 0).await.unwrap();
    assert_eq!(
        feed.items[0].rater_name.as_deref(),
        Some("Ada Okeke"),
        "is_anonymous = false should surface the rater"
    );
}

#[tokio::test]
async fn a_hospitals_feed_carries_the_four_sub_scores() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    svc.rate_hospital(f.shift_id, f.worker_user_id, hospital_rating(4))
        .await
        .unwrap();

    let feed = svc.list_ratings("hospital", f.hospital_id, 20, 0).await.unwrap();
    assert_eq!(feed.total, 1);
    let dims = feed.items[0]
        .dimensions
        .as_ref()
        .expect("worker-rates-hospital carries dimensions");
    assert_eq!(dims["communication"], 4);
    assert_eq!(dims["equipment_availability"], 5);
}

#[tokio::test]
async fn one_hospitals_ratings_do_not_appear_in_anothers_feed() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);
    let f = seed_fixture(&pool).await;

    svc.rate_hospital(f.shift_id, f.worker_user_id, hospital_rating(5))
        .await
        .unwrap();

    let unrelated = seed_hospital(&pool).await;
    let feed = svc.list_ratings("hospital", unrelated, 20, 0).await.unwrap();
    assert_eq!(feed.total, 0, "ratings are keyed to their ratee");
    assert!(feed.items.is_empty());
    assert_eq!(feed.average, None);
}

#[tokio::test]
async fn an_unknown_ratee_kind_is_refused() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);

    // `ratee_kind` is cast to a Postgres enum, so it is validated rather than
    // passed through from a caller.
    let result = svc.list_ratings("admin", Uuid::new_v4(), 20, 0).await;
    assert!(
        matches!(result, Err(ShiftServiceError::ValidationError(_))),
        "got {result:?}"
    );
}

// 6 — the denormalized cache on `clinicians` must agree with the rows.

#[tokio::test]
async fn the_cached_clinician_rating_matches_the_recomputed_average() {
    let Some(pool) = test_pool().await else { return };
    let svc = shift_service(&pool);

    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let worker = seed_user(&pool, "health_worker", None).await;
    let clinician_id = seed_clinician(&pool, worker).await;

    // Two completed shifts for the same clinician, rated 5 and 2 → mean 3.5.
    for score in [5_i16, 2] {
        let shift_id =
            seed_completed_shift(&pool, hospital_id, admin, clinician_id, Utc::now()).await;
        svc.rate_worker(
            shift_id,
            &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
            worker_rating(score),
        )
        .await
        .unwrap();
    }

    let (cached, count): (Option<f32>, Option<i32>) =
        sqlx::query_as("SELECT rating, rating_count FROM clinicians WHERE id = $1")
            .bind(clinician_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(count, Some(2));
    let cached = cached.expect("rating cache should be populated");
    assert!(
        (cached - 3.5).abs() < 0.05,
        "cached rating {cached} should be the mean of 5 and 2"
    );

    let feed = svc.list_ratings("clinician", clinician_id, 20, 0).await.unwrap();
    assert_eq!(feed.total, 2);
    assert_eq!(feed.average, Some(3.5), "the feed aggregate must agree");
}

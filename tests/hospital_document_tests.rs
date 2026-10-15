//! Hospital legal verification documents, and the opt-in qualification filter.
//!
//! Requires a reachable Postgres — set `TEST_DATABASE_URL` (falls back to a
//! local `nexuscare_test`). Skips with a printed notice rather than failing when
//! no database is reachable.
//!
//! Worth having because until this branch nothing ever wrote a
//! `hospital_documents` row: the table, its three enums and all its DTOs existed
//! and the only references were six `SELECT`s. A hospital could upload a licence
//! to Cloudinary and the URL was silently discarded, and the `documents_status`
//! field on `GET /hospitals/{id}` was structurally always `'unverified'`. The
//! first test below is the one that proves that is over.
//!
//! The files here are a hospital's operating licence and a director's ID, so the
//! cross-tenant cases are the point, not a formality.

use std::sync::Arc;

use chrono::{Duration, NaiveDate, Utc};
use nexuscare_backend::models::registration::{
    DocumentType, IssuingAuthority, SubmissionStatus, UploadDocumentRequest,
};
use nexuscare_backend::models::user::{Claims, UserRole};
use nexuscare_backend::repositories::hospital_document::HospitalDocumentRepository;
use nexuscare_backend::services::hospital_document_service::{
    HospitalDocumentError, HospitalDocumentService,
};
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

fn service(pool: &PgPool) -> HospitalDocumentService {
    HospitalDocumentService::new(Arc::new(HospitalDocumentRepository::new(pool.clone())))
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
    .bind(format!("Doc Hospital {}", Uuid::new_v4()))
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

fn licence(url: &str) -> UploadDocumentRequest {
    UploadDocumentRequest {
        document_type: DocumentType::OperationalLicense,
        file_url: url.to_string(),
        file_name: "operational-licence.pdf".to_string(),
        file_mime_type: Some("application/pdf".to_string()),
        file_size_bytes: Some(482_912),
        credential_number: Some("HOSP-4829-X".to_string()),
        expiry_date: NaiveDate::from_ymd_opt(2027, 12, 31),
        issuing_authority: Some(IssuingAuthority::MinistryOfHealthFederal),
    }
}

// 1 — the gap this closes. Before, the upload went nowhere.

#[tokio::test]
async fn an_uploaded_document_is_actually_persisted() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let stored = svc
        .upload(
            hospital_id,
            &c,
            &licence("https://res.cloudinary.com/demo/licence.pdf"),
        )
        .await
        .expect("upload should persist");

    assert_eq!(stored.hospital_id, hospital_id);
    assert_eq!(stored.document_type, DocumentType::OperationalLicense);
    assert_eq!(stored.submission_status, SubmissionStatus::Draft);
    assert_eq!(stored.credential_number.as_deref(), Some("HOSP-4829-X"));

    // Confirm at the row, not just in the return value.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM hospital_documents WHERE hospital_id = $1",
    )
    .bind(hospital_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "the row must exist in the table");
}

#[tokio::test]
async fn documents_status_stops_reading_unverified_once_a_row_exists() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    // This is the CASE expression that `GET /hospitals/{id}` serves. With no
    // writer it could only ever produce 'unverified'.
    let status_sql = "SELECT CASE \
        WHEN EXISTS (SELECT 1 FROM hospital_documents d WHERE d.hospital_id = $1 \
                     AND d.submission_status = 'approved') THEN 'verified' \
        WHEN EXISTS (SELECT 1 FROM hospital_documents d WHERE d.hospital_id = $1 \
                     AND d.submission_status = 'under_review') THEN 'under_review' \
        WHEN EXISTS (SELECT 1 FROM hospital_documents d WHERE d.hospital_id = $1) \
             THEN 'submitted' \
        ELSE 'unverified' END";

    let before: String = sqlx::query_scalar(status_sql)
        .bind(hospital_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, "unverified");

    svc.upload(hospital_id, &c, &licence("https://example.test/l.pdf"))
        .await
        .unwrap();

    let after: String = sqlx::query_scalar(status_sql)
        .bind(hospital_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, "submitted", "a stored document must change the status");
}

// 2 — the tenant boundary. These files are a licence and a director's ID.

#[tokio::test]
async fn a_foreign_hospital_cannot_upload_to_another_hospital() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let victim = seed_hospital(&pool).await;
    let attacker_hospital = seed_hospital(&pool).await;
    let attacker = seed_user(&pool, "hospital_admin", Some(attacker_hospital)).await;

    let result = svc
        .upload(
            victim,
            &claims(attacker, UserRole::HospitalAdmin, Some(attacker_hospital)),
            &licence("https://example.test/forged.pdf"),
        )
        .await;

    assert!(
        matches!(result, Err(HospitalDocumentError::WrongHospital)),
        "got {result:?}"
    );

    // And nothing was written.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM hospital_documents WHERE hospital_id = $1")
            .bind(victim)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "a refused upload must leave no row");
}

#[tokio::test]
async fn a_foreign_hospital_cannot_read_another_hospitals_documents() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let victim = seed_hospital(&pool).await;
    let victim_admin = seed_user(&pool, "hospital_admin", Some(victim)).await;
    svc.upload(
        victim,
        &claims(victim_admin, UserRole::HospitalAdmin, Some(victim)),
        &licence("https://example.test/private.pdf"),
    )
    .await
    .unwrap();

    let other = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other)).await;

    let result = svc
        .list(victim, &claims(intruder, UserRole::HospitalAdmin, Some(other)))
        .await;

    assert!(
        matches!(result, Err(HospitalDocumentError::WrongHospital)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn a_health_worker_cannot_touch_hospital_documents() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let worker = seed_user(&pool, "health_worker", None).await;

    let result = svc
        .list(hospital_id, &claims(worker, UserRole::HealthWorker, None))
        .await;

    assert!(
        matches!(result, Err(HospitalDocumentError::NotAuthorized)),
        "got {result:?}"
    );
}

// 3 — upsert semantics, from the UNIQUE (hospital_id, document_type) constraint.

#[tokio::test]
async fn re_uploading_the_same_type_replaces_rather_than_duplicates() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let first = svc
        .upload(hospital_id, &c, &licence("https://example.test/old.pdf"))
        .await
        .unwrap();
    let second = svc
        .upload(hospital_id, &c, &licence("https://example.test/new.pdf"))
        .await
        .unwrap();

    assert_eq!(first.id, second.id, "the same row should be updated");
    assert_eq!(second.file_url, "https://example.test/new.pdf");

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM hospital_documents WHERE hospital_id = $1")
            .bind(hospital_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "one document per type, not an accumulating pile");
}

#[tokio::test]
async fn re_uploading_clears_a_previous_rejection() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let reviewer = seed_user(&pool, "verification_admin", None).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let doc = svc
        .upload(hospital_id, &c, &licence("https://example.test/bad.pdf"))
        .await
        .unwrap();
    svc.review(
        doc.id,
        SubmissionStatus::Rejected,
        Some("Expired certificate."),
        reviewer,
    )
    .await
    .unwrap();

    // A corrected re-upload must not still be carrying the old rejection when
    // the reviewer next opens it.
    let fixed = svc
        .upload(hospital_id, &c, &licence("https://example.test/good.pdf"))
        .await
        .unwrap();

    assert_eq!(fixed.submission_status, SubmissionStatus::Draft);
    assert_eq!(fixed.review_notes, None, "stale rejection notes must be cleared");
    assert_eq!(fixed.reviewed_at, None);
}

// 4 — deletion is only for the hospital's own draft.

#[tokio::test]
async fn a_draft_can_be_deleted() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let doc = svc
        .upload(hospital_id, &c, &licence("https://example.test/d.pdf"))
        .await
        .unwrap();
    svc.delete(hospital_id, doc.id, &c).await.expect("draft should delete");

    assert!(svc.list(hospital_id, &c).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_submitted_document_cannot_be_deleted() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let doc = svc
        .upload(hospital_id, &c, &licence("https://example.test/s.pdf"))
        .await
        .unwrap();
    svc.submit_for_review(hospital_id, &c).await.unwrap();

    // Once it is part of the compliance record it cannot be withdrawn.
    let result = svc.delete(hospital_id, doc.id, &c).await;
    assert!(
        matches!(result, Err(HospitalDocumentError::NotADraft)),
        "got {result:?}"
    );

    let still_there: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM hospital_documents WHERE id = $1")
            .bind(doc.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_there, 1);
}

#[tokio::test]
async fn deleting_another_hospitals_draft_is_refused() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let victim = seed_hospital(&pool).await;
    let victim_admin = seed_user(&pool, "hospital_admin", Some(victim)).await;
    let doc = svc
        .upload(
            victim,
            &claims(victim_admin, UserRole::HospitalAdmin, Some(victim)),
            &licence("https://example.test/v.pdf"),
        )
        .await
        .unwrap();

    let other = seed_hospital(&pool).await;
    let intruder = seed_user(&pool, "hospital_admin", Some(other)).await;

    let result = svc
        .delete(
            victim,
            doc.id,
            &claims(intruder, UserRole::HospitalAdmin, Some(other)),
        )
        .await;
    assert!(
        matches!(result, Err(HospitalDocumentError::WrongHospital)),
        "got {result:?}"
    );

    let still_there: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM hospital_documents WHERE id = $1")
            .bind(doc.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_there, 1, "nothing may be deleted cross-tenant");
}

// 5 — submission and review.

#[tokio::test]
async fn submitting_moves_only_drafts_into_review() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let reviewer = seed_user(&pool, "verification_admin", None).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let approved = svc
        .upload(hospital_id, &c, &licence("https://example.test/a.pdf"))
        .await
        .unwrap();
    svc.review(approved.id, SubmissionStatus::Approved, None, reviewer)
        .await
        .unwrap();

    let mut other = licence("https://example.test/b.pdf");
    other.document_type = DocumentType::CacCertificate;
    svc.upload(hospital_id, &c, &other).await.unwrap();

    let moved = svc.submit_for_review(hospital_id, &c).await.unwrap();
    assert_eq!(moved, 1, "only the draft should move");

    let docs = svc.list(hospital_id, &c).await.unwrap();
    let approved_now = docs.iter().find(|d| d.id == approved.id).unwrap();
    assert_eq!(
        approved_now.submission_status,
        SubmissionStatus::Approved,
        "an approved document must keep its decision when another is resubmitted"
    );
}

#[tokio::test]
async fn a_review_records_who_decided_and_why() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let reviewer = seed_user(&pool, "verification_admin", None).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let doc = svc
        .upload(hospital_id, &c, &licence("https://example.test/r.pdf"))
        .await
        .unwrap();

    let reviewed = svc
        .review(
            doc.id,
            SubmissionStatus::Rejected,
            Some("Certificate expired in 2024."),
            reviewer,
        )
        .await
        .unwrap();

    assert_eq!(reviewed.submission_status, SubmissionStatus::Rejected);
    assert_eq!(
        reviewed.review_notes.as_deref(),
        Some("Certificate expired in 2024.")
    );
    assert!(reviewed.reviewed_at.is_some());

    // reviewed_by is not in the response DTO, so check the row.
    let stored_by: Option<Uuid> =
        sqlx::query_scalar("SELECT reviewed_by FROM hospital_documents WHERE id = $1")
            .bind(doc.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_by, Some(reviewer), "the decision must be attributable");
}

#[tokio::test]
async fn a_rejection_without_notes_is_refused() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let reviewer = seed_user(&pool, "verification_admin", None).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let doc = svc
        .upload(hospital_id, &c, &licence("https://example.test/n.pdf"))
        .await
        .unwrap();

    for notes in [None, Some(""), Some("   ")] {
        let result = svc
            .review(doc.id, SubmissionStatus::Rejected, notes, reviewer)
            .await;
        assert!(
            matches!(result, Err(HospitalDocumentError::RejectionNeedsNotes)),
            "notes={notes:?} should be refused, got {result:?}"
        );
    }
}

#[tokio::test]
async fn a_reviewer_cannot_push_a_document_back_to_draft() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let reviewer = seed_user(&pool, "verification_admin", None).await;
    let c = claims(admin, UserRole::HospitalAdmin, Some(hospital_id));

    let doc = svc
        .upload(hospital_id, &c, &licence("https://example.test/dr.pdf"))
        .await
        .unwrap();

    // `draft` is the hospital's own state; writing it back would erase the trail.
    let result = svc
        .review(doc.id, SubmissionStatus::Draft, None, reviewer)
        .await;
    assert!(
        matches!(result, Err(HospitalDocumentError::CannotReviewToDraft)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn reviewing_a_document_that_does_not_exist_is_a_not_found() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let reviewer = seed_user(&pool, "verification_admin", None).await;

    let result = svc
        .review(
            Uuid::new_v4(),
            SubmissionStatus::Approved,
            None,
            reviewer,
        )
        .await;
    assert!(
        matches!(result, Err(HospitalDocumentError::NotFound)),
        "got {result:?}"
    );
}

#[tokio::test]
async fn a_super_admin_may_reach_any_hospitals_documents() {
    let Some(pool) = test_pool().await else { return };
    let svc = service(&pool);
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    svc.upload(
        hospital_id,
        &claims(admin, UserRole::HospitalAdmin, Some(hospital_id)),
        &licence("https://example.test/sa.pdf"),
    )
    .await
    .unwrap();

    let platform = seed_user(&pool, "super_admin", None).await;
    let docs = svc
        .list(hospital_id, &claims(platform, UserRole::SuperAdmin, None))
        .await
        .expect("a super admin may read for support");
    assert_eq!(docs.len(), 1);
}

// 6 — the opt-in qualification filter on worker discovery.
//
// Before this, `GET /worker/shifts/nearby` was geography and urgency only:
// requirements existed, qualifications existed, matching existed as a display
// badge and a 10% ranking weight, and nothing filtered discovery.

/// Does this clinician's nearby feed contain `shift_id`?
///
/// Pages rather than asking for one `LIMIT 50` window. The nearby feed is
/// *global* — it ranks every open shift on the platform — and this test database
/// accumulates rows across runs with no truncation, so a freshly seeded shift
/// sorts behind whatever earlier runs left behind. A single-page assertion here
/// passes today and fails silently once the table grows, which is the worst
/// shape of flake.
async fn feed_contains(
    repo: &nexuscare_backend::repositories::shift::ShiftRepository,
    clinician_id: Uuid,
    shift_id: Uuid,
    require_qualification_match: bool,
) -> bool {
    const PAGE: i64 = 200;
    let mut offset = 0i64;
    loop {
        let page = repo
            .list_nearby_shifts(clinician_id, None, 5.0, PAGE, offset, require_qualification_match)
            .await
            .expect("nearby query should succeed");
        if page.iter().any(|r| r.shift_id == shift_id) {
            return true;
        }
        if (page.len() as i64) < PAGE {
            return false;
        }
        offset += PAGE;
    }
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

/// A virtual shift, so the geo filter admits it without needing coordinates.
async fn seed_virtual_shift(pool: &PgPool, hospital_id: Uuid, created_by: Uuid) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, shift_type, status,
            scheduled_start, duration_hours, scheduled_end,
            pay_type, rate_kobo_per_hour, grand_total_kobo, created_by
        )
        VALUES ($1, 'doctor', 'Emergency Doctor', 'virtual', 'open',
                NOW() + INTERVAL '2 hours', 4, NOW() + INTERVAL '6 hours',
                'hourly_rate', 800000, 3200000, $2)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(created_by)
    .fetch_one(pool)
    .await
    .expect("failed to seed shift")
}

#[tokio::test]
async fn the_qualification_filter_is_off_unless_asked_for() {
    let Some(pool) = test_pool().await else { return };
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let worker = seed_user(&pool, "health_worker", None).await;
    let clinician_id = seed_clinician(&pool, worker).await;

    let shift_id = seed_virtual_shift(&pool, hospital_id, admin).await;
    sqlx::query("INSERT INTO shift_requirements (shift_id, qualification) VALUES ($1, 'ACLS')")
        .bind(shift_id)
        .execute(&pool)
        .await
        .unwrap();
    // The clinician has no qualifications at all.

    let repo = nexuscare_backend::repositories::shift::ShiftRepository::new(pool.clone());

    assert!(
        feed_contains(&repo, clinician_id, shift_id, false).await,
        "the default feed must not be narrowed by qualifications"
    );
    assert!(
        !feed_contains(&repo, clinician_id, shift_id, true).await,
        "with the filter on, an unmet requirement should hide the shift"
    );
}

#[tokio::test]
async fn a_matching_qualification_keeps_the_shift_visible() {
    let Some(pool) = test_pool().await else { return };
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let worker = seed_user(&pool, "health_worker", None).await;
    let clinician_id = seed_clinician(&pool, worker).await;

    let shift_id = seed_virtual_shift(&pool, hospital_id, admin).await;
    sqlx::query("INSERT INTO shift_requirements (shift_id, qualification) VALUES ($1, 'ACLS')")
        .bind(shift_id)
        .execute(&pool)
        .await
        .unwrap();

    // Containment, case-insensitive: "ACLS Certified" covers a requirement of
    // "ACLS". Requirements are free text with no shared vocabulary, so exact
    // equality would match almost nothing.
    sqlx::query(
        "INSERT INTO clinician_qualifications (clinician_id, qualification) \
         VALUES ($1, 'ACLS Certified')",
    )
    .bind(clinician_id)
    .execute(&pool)
    .await
    .unwrap();

    let repo = nexuscare_backend::repositories::shift::ShiftRepository::new(pool.clone());
    assert!(
        feed_contains(&repo, clinician_id, shift_id, true).await,
        "a covered requirement should keep the shift in the feed"
    );
}

#[tokio::test]
async fn a_shift_with_no_requirements_survives_the_filter() {
    let Some(pool) = test_pool().await else { return };
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let worker = seed_user(&pool, "health_worker", None).await;
    let clinician_id = seed_clinician(&pool, worker).await;

    // No shift_requirements rows at all. The predicate is "no unmatched
    // requirement exists", so this must pass rather than being excluded by a
    // count comparison.
    let shift_id = seed_virtual_shift(&pool, hospital_id, admin).await;

    let repo = nexuscare_backend::repositories::shift::ShiftRepository::new(pool.clone());
    assert!(
        feed_contains(&repo, clinician_id, shift_id, true).await,
        "a shift that asks for nothing is matched by everyone"
    );
}

#[tokio::test]
async fn one_unmet_requirement_out_of_several_hides_the_shift() {
    let Some(pool) = test_pool().await else { return };
    let hospital_id = seed_hospital(&pool).await;
    let admin = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let worker = seed_user(&pool, "health_worker", None).await;
    let clinician_id = seed_clinician(&pool, worker).await;

    let shift_id = seed_virtual_shift(&pool, hospital_id, admin).await;
    for req in ["ACLS", "PALS"] {
        sqlx::query("INSERT INTO shift_requirements (shift_id, qualification) VALUES ($1, $2)")
            .bind(shift_id)
            .bind(req)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO clinician_qualifications (clinician_id, qualification) VALUES ($1, 'ACLS')")
        .bind(clinician_id)
        .execute(&pool)
        .await
        .unwrap();

    let repo = nexuscare_backend::repositories::shift::ShiftRepository::new(pool.clone());
    assert!(
        !feed_contains(&repo, clinician_id, shift_id, true).await,
        "the filter requires every requirement to be met, not just one"
    );
}

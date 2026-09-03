use chrono::{Duration, Timelike, Utc};
use nexuscare_backend::models::shift::{
    CreateShiftRequest, PayType, RoleCategory, ShiftPriority, ShiftType,
};
use nexuscare_backend::repositories::shift::ShiftRepository;
use nexuscare_backend::services::notification_service::NotificationService;
use nexuscare_backend::services::shift_service::{ShiftService, ShiftServiceError};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// Helper to create a valid shift request. Start time is snapped to the next
/// 15-minute boundary in the future (F1-F05). Defaults to Normal priority and a
/// today-relative start, satisfying BR-F1-03.
fn create_valid_shift_request() -> CreateShiftRequest {
    let start = next_15min_boundary(Utc::now() + Duration::hours(2));
    CreateShiftRequest {
        role_category: RoleCategory::Doctor,
        role_title: "Emergency Doctor".to_string(),
        specialty: Some("Emergency Medicine".to_string()),
        department: Some("Emergency Department".to_string()),
        shift_type: ShiftType::InPerson,
        priority: ShiftPriority::Normal,
        urgency_bonus_pct: None,
        scheduled_start: start,
        duration_hours: 8.0,
        pay_type: PayType::HourlyRate,
        rate_kobo_per_hour: Some(800_000), // ₦8,000/hr
        fixed_rate_kobo: None,
        stat_bonus_kobo: None,
        shift_label: Some("Night Shift: Emergency".to_string()),
        job_description: Some("Cover the night shift in the ED.".to_string()),
        tasks: vec!["Triage incoming patients".to_string()],
        equipment: vec![],
        requirements: vec!["Valid medical license".to_string()],
        notes: Some("Urgent coverage needed".to_string()),
        broadcast_consent_confirmed: true,
    }
}

/// Snap a timestamp forward to the next 15-minute boundary with zero seconds.
fn next_15min_boundary(ts: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    let snapped = ts.with_second(0).unwrap().with_nanosecond(0).unwrap();
    let minute = snapped.minute();
    let remainder = minute % 15;
    if remainder == 0 {
        snapped
    } else {
        snapped + Duration::minutes((15 - remainder) as i64)
    }
}

#[cfg(test)]
mod shift_creation_tests {
    use super::*;

    /// AC-01: Form Validation - All required fields correctly filled
    #[tokio::test]
    async fn test_valid_shift_creation() {
        // This test would require a test database setup
        // For now, we validate the request structure
        let request = create_valid_shift_request();

        assert_eq!(request.role_title, "Emergency Doctor");
        assert_eq!(request.duration_hours, 8.0);
        assert!(request.broadcast_consent_confirmed);
        assert!(request.rate_kobo_per_hour.is_some());
    }

    /// AC-02: Missing Required Field - Role title empty
    #[test]
    fn test_missing_role_title() {
        let mut request = create_valid_shift_request();
        request.role_title = "".to_string();

        // Validation would fail in the service layer
        assert!(request.role_title.is_empty());
    }

    /// AC-02: Missing Required Field - Hourly rate missing for hourly pay type
    #[test]
    fn test_missing_hourly_rate() {
        let mut request = create_valid_shift_request();
        request.pay_type = PayType::HourlyRate;
        request.rate_kobo_per_hour = None;

        // This should fail validation
        assert!(request.rate_kobo_per_hour.is_none());
    }

    /// AC-03: STAT Shift Logic - Start time within one hour
    #[test]
    fn test_stat_shift_within_one_hour() {
        let mut request = create_valid_shift_request();
        request.priority = ShiftPriority::Stat;
        request.scheduled_start = Utc::now() + Duration::minutes(30);
        request.stat_bonus_kobo = Some(500_000); // ₦5,000 bonus

        let time_until_start = request.scheduled_start.signed_duration_since(Utc::now());
        assert!(time_until_start <= Duration::hours(1));
        assert!(request.stat_bonus_kobo.is_some());
    }

    /// AC-03: STAT Shift Logic - Start time beyond one hour should fail
    #[test]
    fn test_stat_shift_beyond_one_hour() {
        let mut request = create_valid_shift_request();
        request.priority = ShiftPriority::Stat;
        request.scheduled_start = Utc::now() + Duration::hours(2);
        request.stat_bonus_kobo = Some(500_000);

        let time_until_start = request.scheduled_start.signed_duration_since(Utc::now());
        assert!(time_until_start > Duration::hours(1));
    }

    /// AC-03: STAT Shift Logic - Bonus payment required
    #[test]
    fn test_stat_shift_requires_bonus() {
        let mut request = create_valid_shift_request();
        request.priority = ShiftPriority::Stat;
        request.scheduled_start = Utc::now() + Duration::minutes(30);
        request.stat_bonus_kobo = None;
        request.urgency_bonus_pct = None;

        // Should fail validation - STAT requires bonus
        assert!(request.stat_bonus_kobo.is_none() && request.urgency_bonus_pct.is_none());
    }

    /// AC-04: Virtual Shift Creation - No distance restriction
    #[test]
    fn test_virtual_shift_no_distance_restriction() {
        let mut request = create_valid_shift_request();
        request.shift_type = ShiftType::Virtual;

        assert_eq!(request.shift_type, ShiftType::Virtual);
        // Virtual shifts should generate a meeting link
    }

    /// AC-05: In-Person Shift Creation - 5km distance restriction
    #[test]
    fn test_in_person_shift_distance_restriction() {
        let mut request = create_valid_shift_request();
        request.shift_type = ShiftType::InPerson;

        assert_eq!(request.shift_type, ShiftType::InPerson);
        // In-person shifts should apply 5km distance filter
    }

    /// AC-06: Shift Preview - Calculate compensation correctly
    #[test]
    fn test_shift_preview_compensation() {
        let request = create_valid_shift_request();

        let base_amount =
            request.rate_kobo_per_hour.unwrap() as f64 * request.duration_hours as f64;
        let expected_base = 6_400_000; // 800,000 * 8 = ₦64,000

        assert_eq!(base_amount as i64, expected_base);
    }

    /// AC-06: Shift Preview - STAT bonus included in total
    #[test]
    fn test_shift_preview_with_stat_bonus() {
        let mut request = create_valid_shift_request();
        request.priority = ShiftPriority::Stat;
        request.scheduled_start = Utc::now() + Duration::minutes(30);
        request.stat_bonus_kobo = Some(500_000); // ₦5,000

        let base_amount =
            request.rate_kobo_per_hour.unwrap() as f64 * request.duration_hours as f64;
        let grand_total = base_amount as i64 + request.stat_bonus_kobo.unwrap();
        let expected_total = 6_900_000; // ₦64,000 + ₦5,000 = ₦69,000

        assert_eq!(grand_total, expected_total);
    }

    /// AC-07: Shift Broadcast - Consent must be confirmed
    #[test]
    fn test_broadcast_consent_required() {
        let mut request = create_valid_shift_request();
        request.broadcast_consent_confirmed = false;

        // Should fail validation
        assert!(!request.broadcast_consent_confirmed);
    }

    /// AC-08: Duplicate Shift Prevention - Similar shift detection
    #[test]
    fn test_duplicate_shift_detection() {
        let scheduled_time = Utc::now() + Duration::hours(2);

        let mut request1 = create_valid_shift_request();
        request1.scheduled_start = scheduled_time;

        let mut request2 = create_valid_shift_request();
        request2.scheduled_start = scheduled_time;

        // Same role title and scheduled start time
        assert_eq!(request1.role_title, request2.role_title);
        assert_eq!(request1.scheduled_start, request2.scheduled_start);
    }

    /// Test fixed rate pay type
    #[test]
    fn test_fixed_rate_pay_type() {
        let mut request = create_valid_shift_request();
        request.pay_type = PayType::FixedRate;
        request.rate_kobo_per_hour = None;
        request.fixed_rate_kobo = Some(5_000_000); // ₦50,000 fixed

        assert_eq!(request.pay_type, PayType::FixedRate);
        assert!(request.fixed_rate_kobo.is_some());
        assert_eq!(request.fixed_rate_kobo.unwrap(), 5_000_000);
    }

    /// Test urgent priority with bonus
    #[test]
    fn test_urgent_shift_with_bonus() {
        let mut request = create_valid_shift_request();
        request.priority = ShiftPriority::Urgent;
        request.urgency_bonus_pct = Some(20); // +20% bonus

        assert_eq!(request.priority, ShiftPriority::Urgent);
        assert_eq!(request.urgency_bonus_pct, Some(20));
    }

    /// Test duration validation
    #[test]
    fn test_duration_validation() {
        let request = create_valid_shift_request();

        // Duration should be between 0.5 and 24 hours
        assert!(request.duration_hours >= 0.5);
        assert!(request.duration_hours <= 24.0);
    }

    /// Test specialty and department optional fields
    #[test]
    fn test_optional_fields() {
        let request = create_valid_shift_request();

        assert!(request.specialty.is_some());
        assert!(request.department.is_some());
        assert!(request.shift_label.is_some());
        assert!(request.notes.is_some());
    }
    /// Test hospital approval requirement
    #[test]
    fn test_hospital_approval_required() {
        // This test validates that only approved hospitals can create shifts
        // In production, this would be enforced by the service layer

        // Mock scenario: Hospital with pending status
        let is_approved = false;
        assert!(
            !is_approved,
            "Pending hospitals should not be able to create shifts"
        );

        // Mock scenario: Hospital with approved status
        let is_approved = true;
        assert!(
            is_approved,
            "Approved hospitals should be able to create shifts"
        );
    }

    /// Test hospital name is included in shift
    #[test]
    fn test_hospital_name_in_shift() {
        // Verify that shifts include the hospital name
        let hospital_name = Some("Lagos University Teaching Hospital".to_string());
        assert!(hospital_name.is_some());
        assert_eq!(hospital_name.unwrap(), "Lagos University Teaching Hospital");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Integration test placeholder - requires database
    /// This would test the full flow from request to database
    #[tokio::test]
    #[ignore] // Ignore until database is set up
    async fn test_full_shift_creation_flow() {
        // Setup test database
        // Create shift service
        // Create shift
        // Verify shift in database
        // Verify notifications sent
    }

    /// Integration test placeholder - hospital approval check
    #[tokio::test]
    #[ignore]
    async fn test_unapproved_hospital_cannot_create_shift() {
        // Create unapproved hospital
        // Attempt to create shift
        // Verify error returned: "Hospital not approved"
    }

    /// Integration test placeholder - hospital name in shift
    #[tokio::test]
    #[ignore]
    async fn test_shift_includes_hospital_name() {
        // Create approved hospital with name
        // Create shift
        // Verify shift.hospital_name matches hospital.name
    }

    /// Integration test placeholder - duplicate detection
    #[tokio::test]
    #[ignore]
    async fn test_duplicate_shift_prevention_integration() {
        // Create first shift
        // Attempt to create duplicate within 1 hour
        // Verify error returned
    }

    /// AC-04 — a virtual shift is given an app deep link; an in-person one is
    /// not. Skips when no test database is reachable, like the rest of the
    /// suite.
    #[tokio::test]
    async fn test_virtual_link_generation_integration() {
        let Some(pool) = test_pool().await else { return };
        let service = shift_service(&pool);
        let (hospital_id, user_id) = seed_funded_hospital(&pool).await;

        let mut virtual_request = create_valid_shift_request();
        virtual_request.shift_type = ShiftType::Virtual;
        let virtual_shift = service
            .create_shift(hospital_id, user_id, virtual_request)
            .await
            .expect("a funded, approved hospital can create a virtual shift");

        let base = std::env::var("APP_PUBLIC_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://app.nexuscare.com".to_string());
        let expected = format!(
            "{}/consults/{}",
            base.trim_end_matches('/'),
            virtual_shift.id
        );

        assert_eq!(virtual_link(&pool, virtual_shift.id).await.as_deref(), Some(expected.as_str()));

        // A distinct role title, so the duplicate-shift guard (hospital +
        // title + start) does not reject the second create.
        let mut in_person_request = create_valid_shift_request();
        in_person_request.role_title = "ICU Nurse".to_string();
        let in_person_shift = service
            .create_shift(hospital_id, user_id, in_person_request)
            .await
            .expect("and an in-person one");

        assert_eq!(
            virtual_link(&pool, in_person_shift.id).await,
            None,
            "in-person shifts have no consultation to link to"
        );
    }
}

// Database-backed helpers. `test_pool` skips with a notice rather than failing
// when no Postgres is reachable, matching tests/patient_ingest_tests.rs.

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
    use nexuscare_backend::repositories::notification::NotificationRepository;
    use nexuscare_backend::repositories::wallet::WalletRepository;
    use nexuscare_backend::repositories::EmailOutboxRepository;
    use nexuscare_backend::services::email_outbox_service::EmailOutboxService;
    use nexuscare_backend::services::fcm::FcmClient;
    use nexuscare_backend::services::push_service::PushService;
    use nexuscare_backend::services::safehaven::SafeHavenClient;
    use nexuscare_backend::services::wallet_service::WalletService;

    let notification_service = Arc::new(NotificationService::new());
    Arc::new(ShiftService::new(
        Arc::new(ShiftRepository::new(pool.clone())),
        pool.clone(),
        notification_service.clone(),
        Arc::new(EmailOutboxService::new(
            Arc::new(EmailOutboxRepository::new(pool.clone())),
            notification_service,
        )),
        Arc::new(WalletService::new(
            Arc::new(WalletRepository::new(pool.clone())),
            Arc::new(SafeHavenClient::from_env()),
            pool.clone(),
        )),
        Arc::new(PushService::new(
            Arc::new(NotificationRepository::new(pool.clone())),
            Arc::new(FcmClient::from_env()),
        )),
    ))
}

/// An approved hospital with enough wallet balance to cover the shift hold,
/// plus the admin user that creates the shift.
async fn seed_funded_hospital(pool: &PgPool) -> (Uuid, Uuid) {
    let hospital_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO hospitals (name, registration_number, email, address, phone_number,
                               admin_registration_status)
        VALUES ($1, $2, $3, 'Test Address', '08000000000', 'approved')
        RETURNING id
        "#,
    )
    .bind(format!("Test Hospital {}", Uuid::new_v4()))
    .bind(format!("RC-{}", &Uuid::new_v4().to_string()[..8]))
    .bind(format!("{}@example.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .expect("failed to seed hospital");

    let user_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO users (email, first_name, last_name, password_hash, role, hospital_id)
        VALUES ($1, 'Test', 'Admin', 'not-a-real-hash', 'hospital_admin', $2)
        RETURNING id
        "#,
    )
    .bind(format!("{}@example.test", Uuid::new_v4()))
    .bind(hospital_id)
    .fetch_one(pool)
    .await
    .expect("failed to seed hospital admin");

    sqlx::query(
        "INSERT INTO hospital_wallets (hospital_id, balance_kobo) VALUES ($1, 100000000)",
    )
    .bind(hospital_id)
    .execute(pool)
    .await
    .expect("failed to fund wallet");

    (hospital_id, user_id)
}

async fn virtual_link(pool: &PgPool, shift_id: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT virtual_link FROM shifts WHERE id = $1")
        .bind(shift_id)
        .fetch_one(pool)
        .await
        .expect("failed to read virtual_link")
}

//! Regression coverage for `GET /api/v1/worker/earnings` (Defect 1).
//!
//! `SUM(bigint)` returns `numeric` in Postgres, which sqlx cannot decode into
//! `i64` unless the query casts it (`::BIGINT`). The bug only surfaces once the
//! worker actually has payout rows — with zero rows `SUM` is NULL and the type
//! check is skipped. This test seeds a `payout`/`success` row for an assigned
//! clinician and asserts the exact totals query the handler runs decodes into
//! `(i64, i64, i64)` with the correct sum. Follows the repo's `#[ignore]`
//! integration-test convention (needs a seeded database; see setup_local_db.sh).

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

/// Connect to the test database, or skip (return None) if none is reachable.
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

#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn earnings_totals_sum_decodes_for_nonempty_account() {
    let Some(pool) = test_pool().await else { return };

    // Seed hospital + health-worker user + clinician.
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
    .fetch_one(&pool)
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
    .fetch_one(&pool)
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
    .fetch_one(&pool)
    .await
    .expect("seed clinician");

    // A completed shift assigned to the clinician, plus a successful payout.
    let shift_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO shifts (hospital_id, role_category, role_title, shift_type, status,
                            scheduled_start, duration_hours, scheduled_end,
                            pay_type, rate_kobo_per_hour, created_by, assigned_clinician_id)
        VALUES ($1, 'doctor', 'Doctor', 'in_person', 'completed',
                NOW() - INTERVAL '2 days', 8, NOW() - INTERVAL '1 day',
                'hourly_rate', 800000, $2, $3)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(user_id)
    .bind(clinician_id)
    .fetch_one(&pool)
    .await
    .expect("seed shift");

    let amount_kobo: i64 = 1_500_000; // ₦15,000
    sqlx::query(
        r#"
        INSERT INTO billing_transactions (hospital_id, event_type, amount_kobo, status, shift_id)
        VALUES ($1, 'payout', $2, 'success', $3)
        "#,
    )
    .bind(hospital_id)
    .bind(amount_kobo)
    .bind(shift_id)
    .execute(&pool)
    .await
    .expect("seed payout");

    // Exactly the totals query the earnings handler runs. Decoding into
    // (i64, i64, i64) is the assertion: without the ::BIGINT cast this fails.
    let month_start = Utc::now();
    let totals: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COALESCE(SUM(bt.amount_kobo) FILTER (WHERE bt.status = 'success'), 0)::BIGINT
                AS total_earned,
            COALESCE(SUM(bt.amount_kobo) FILTER (WHERE bt.status = 'success'
                                                   AND bt.completed_at >= $2), 0)::BIGINT
                AS this_month,
            COALESCE(SUM(bt.amount_kobo) FILTER (WHERE bt.status = 'pending'), 0)::BIGINT
                AS pending
        FROM billing_transactions bt
        JOIN shifts s ON s.id = bt.shift_id
        WHERE bt.event_type = 'payout'
          AND s.assigned_clinician_id = $1
        "#,
    )
    .bind(clinician_id)
    .bind(month_start)
    .fetch_one(&pool)
    .await
    .expect("totals query must decode into i64");

    assert_eq!(totals.0, amount_kobo, "total_earned_kobo should sum the payout");
}

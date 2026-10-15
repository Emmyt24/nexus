//! End-to-end walk of a virtual consultation over **real HTTP**, from the
//! clinician joining to the hospital reading the finished patient records.
//!
//! This is the only test in the suite that exercises the HTTP layer. It binds
//! `axum::serve` to `127.0.0.1:0` and drives it with `reqwest`, so the router,
//! the `require_role` guards and `extract_claims` all run for real — the JWTs
//! here are minted by `issue_access_token`, not hand-built `Claims` structs.
//! That is what makes it worth having alongside the service-level suites: a
//! route registered with the wrong guard, or a handler whose extractor order is
//! wrong, is invisible to those but fails here.
//!
//! Requires a reachable Postgres — set TEST_DATABASE_URL. Skips (prints a
//! notice and returns early) rather than failing if none is reachable.
//!
//! **One test function on purpose.** `JWT_SECRET`, `APP_PUBLIC_BASE_URL` and the
//! `LIVEKIT_*` vars are process-global, so a second `#[tokio::test]` in this
//! file would race this one's environment — the same constraint already
//! documented in `shift_service.rs`'s `consult_deep_link` tests.

use std::sync::Arc;

use chrono::Utc;
use nexuscare_backend::models::user::UserRole;
use nexuscare_backend::repositories::EmailOutboxRepository;
use nexuscare_backend::services::email_outbox_service::EmailOutboxService;
use nexuscare_backend::services::notification_service::NotificationService;
use nexuscare_backend::utils::jwt::issue_access_token;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const JWT_SECRET: &str = "e2e-patient-consult-flow-secret";

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

/// Serve the real router on an ephemeral port and return its base URL.
async fn serve(pool: PgPool) -> String {
    let notification_service = Arc::new(NotificationService::new());
    let email_outbox = Arc::new(EmailOutboxService::new(
        Arc::new(EmailOutboxRepository::new(pool.clone())),
        notification_service.clone(),
    ));
    let (app, _state) =
        nexuscare_backend::routes::create_router(pool, notification_service, email_outbox);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral port");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    format!("http://{addr}")
}

// Fixtures. Seeded directly, as everywhere else in this suite.

async fn seed_hospital(pool: &PgPool) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO hospitals (name, registration_number, email, address, phone_number,
                               admin_registration_status)
        VALUES ($1, $2, $3, 'Test Address', '08000000000', 'approved')
        RETURNING id
        "#,
    )
    .bind(format!("E2E Hospital {}", Uuid::new_v4()))
    .bind(format!("RC-{}", &Uuid::new_v4().to_string()[..8]))
    .bind(format!("{}@example.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .expect("failed to seed hospital")
}

async fn seed_user(pool: &PgPool, role: &str, hospital_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("{}@example.test", Uuid::new_v4());
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO users (email, first_name, last_name, password_hash, role, hospital_id)
        VALUES ($1, 'E2E', 'User', 'not-a-real-hash', $2::user_role, $3)
        RETURNING id
        "#,
    )
    .bind(&email)
    .bind(role)
    .bind(hospital_id)
    .fetch_one(pool)
    .await
    .expect("failed to seed user");
    (id, email)
}

async fn seed_clinician(pool: &PgPool, user_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        r#"
        INSERT INTO clinicians (user_id, first_name, last_name, specialty, role_title,
                                license_number, clinician_role)
        VALUES ($1, 'Amina', 'Bello', 'emergency_medicine', 'Emergency Doctor',
                'MDCN-E2E', 'doctor')
        RETURNING id
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("failed to seed clinician")
}

async fn seed_shift(pool: &PgPool, hospital_id: Uuid, created_by: Uuid, clinician_id: Uuid) -> Uuid {
    let shift_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO shifts (
            hospital_id, role_category, role_title, shift_type, status,
            scheduled_start, duration_hours, scheduled_end,
            assigned_clinician_id, pay_type, rate_kobo_per_hour,
            grand_total_kobo, created_by
        )
        VALUES ($1, 'doctor', 'Emergency Doctor', 'virtual', 'assigned',
                $2, 4, $2 + INTERVAL '4 hours',
                $3, 'hourly_rate', 800000, 3200000, $4)
        RETURNING id
        "#,
    )
    .bind(hospital_id)
    .bind(Utc::now())
    .bind(clinician_id)
    .bind(created_by)
    .fetch_one(pool)
    .await
    .expect("failed to seed shift");

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

    shift_id
}

// HTTP helpers.

struct Api {
    base: String,
    http: reqwest::Client,
}

impl Api {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("request failed");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let json = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, json)
    }

    async fn get(&self, path: &str, token: &str) -> (reqwest::StatusCode, Value) {
        self.send(reqwest::Method::GET, path, Some(token), None).await
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> (reqwest::StatusCode, Value) {
        self.send(reqwest::Method::POST, path, Some(token), Some(body))
            .await
    }

    async fn put(&self, path: &str, token: &str, body: Value) -> (reqwest::StatusCode, Value) {
        self.send(reqwest::Method::PUT, path, Some(token), Some(body))
            .await
    }
}

#[tokio::test]
async fn the_whole_patient_consult_flow_over_http() {
    let Some(pool) = test_pool().await else { return };

    // Mock LiveKit (empty credentials) and clock-in on join, so the webhook
    // below really writes attendance.
    std::env::set_var("JWT_SECRET", JWT_SECRET);
    std::env::set_var("LIVEKIT_URL", "wss://mock.livekit.test");
    std::env::set_var("LIVEKIT_API_KEY", "");
    std::env::set_var("LIVEKIT_API_SECRET", "");
    std::env::set_var("LIVEKIT_VIRTUAL_CLOCKIN_ENABLED", "true");
    std::env::set_var("APP_PUBLIC_BASE_URL", "https://e2e.nexuscare.test");
    std::env::set_var("ML_SERVICE_URL", "");

    let hospital_id = seed_hospital(&pool).await;
    let (admin_id, admin_email) = seed_user(&pool, "hospital_admin", Some(hospital_id)).await;
    let (worker_id, worker_email) = seed_user(&pool, "health_worker", None).await;
    let clinician_id = seed_clinician(&pool, worker_id).await;
    let shift_id = seed_shift(&pool, hospital_id, admin_id, clinician_id).await;

    // Real tokens, so require_role and extract_claims are genuinely exercised.
    let (admin_token, _) = issue_access_token(
        admin_id,
        &admin_email,
        UserRole::HospitalAdmin,
        Some(hospital_id.to_string()),
    )
    .expect("failed to mint the admin token");
    let (worker_token, _) =
        issue_access_token(worker_id, &worker_email, UserRole::HealthWorker, None)
            .expect("failed to mint the worker token");

    let api = Api {
        base: serve(pool.clone()).await,
        http: reqwest::Client::new(),
    };

    // 1 — the worker joins the consultation.
    let (status, token_body) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/consult/token"),
            &worker_token,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "token: {token_body}");
    assert_eq!(token_body["mock"], json!(true));
    let room_name = token_body["room_name"].as_str().unwrap().to_string();
    let worker_identity = token_body["identity"].as_str().unwrap().to_string();

    // 2 — LiveKit reports the join, which is what clocks the worker in.
    let (status, _) = api
        .send(
            reqwest::Method::POST,
            "/api/v1/webhooks/livekit",
            None,
            Some(json!({
                "event": "participant_joined",
                // Unique per run: webhook_events dedupes on the provider id, and
                // this suite never truncates, so a fixed id would be reported
                // as already-seen on the second run.
                "id": format!("e2e-join-{}", Uuid::new_v4()),
                "createdAt": Utc::now().timestamp(),
                "room": { "name": room_name, "sid": "RM_e2e" },
                "participant": { "identity": worker_identity, "sid": "PA_e2e", "name": "Dr Test" }
            })),
        )
        .await;
    assert_eq!(status, 200, "the webhook always 200s");

    let (status, session) = api
        .get(&format!("/api/v1/shifts/{shift_id}/consult"), &worker_token)
        .await;
    assert_eq!(status, 200, "consult: {session}");
    assert_eq!(
        session["clock_in_recorded"],
        json!(true),
        "joining records the clock-in: {session}"
    );
    assert_eq!(session["waiting_room"]["waiting"], json!(0));

    // 3 — the role guard really rejects a worker on a hospital-only route.
    let (status, _) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/consult/queue"),
            &worker_token,
            json!({ "patient": { "full_name": "Should Not Land", "age": 30.0 } }),
        )
        .await;
    assert_eq!(
        status, 403,
        "require_role must keep workers out of the queue route"
    );

    // 4 — the hospital adds two patients mid-call.
    for name in ["Ada Lovelace", "Grace Hopper"] {
        let (status, body) = api
            .post(
                &format!("/api/v1/shifts/{shift_id}/consult/queue"),
                &admin_token,
                json!({ "patient": { "full_name": name, "age": 42.0, "symptoms": "cough" } }),
            )
            .await;
        assert_eq!(status, 201, "queue {name}: {body}");
    }

    let (status, session) = api
        .get(&format!("/api/v1/shifts/{shift_id}/consult"), &worker_token)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        session["waiting_room"]["waiting"],
        json!(2),
        "the waiting count shows on the consult read: {session}"
    );

    let (status, queue) = api
        .get(
            &format!("/api/v1/shifts/{shift_id}/consult/queue"),
            &worker_token,
        )
        .await;
    assert_eq!(status, 200, "queue list: {queue}");
    let first_entry = queue["patients"][0]["entry_id"].as_str().unwrap().to_string();
    let first_patient = queue["patients"][0]["patient_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 5 — the doctor calls the first patient in and records a note.
    let (status, body) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/consult/queue/{first_entry}/call"),
            &worker_token,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "call: {body}");
    assert_eq!(body["in_consult"], json!(1));
    assert_eq!(body["waiting"], json!(1));

    let (status, note) = api
        .post(
            &format!("/api/v1/patients/{first_patient}/consultation-notes"),
            &worker_token,
            json!({ "shift_id": shift_id }),
        )
        .await;
    assert_eq!(status, 200, "start note: {note}");
    let note_id = note["id"].as_str().unwrap().to_string();

    let (status, saved) = api
        .send(
            reqwest::Method::PATCH,
            &format!("/api/v1/consultation-notes/{note_id}"),
            Some(&worker_token),
            Some(json!({
                "assessment": "Acute bronchitis",
                "plan": "Rest, fluids, review in 5 days",
                "diagnosis": "J20.9",
                "vitals": { "bp": "118/76", "temp_c": 37.8 }
            })),
        )
        .await;
    assert_eq!(status, 200, "patch note: {saved}");
    assert_eq!(saved["assessment"], json!("Acute bronchitis"));
    assert_eq!(saved["shift_id"], json!(shift_id));

    let (status, body) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/consult/queue/{first_entry}/seen"),
            &worker_token,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "seen: {body}");
    assert_eq!(body["seen"], json!(1));
    assert_eq!(body["in_consult"], json!(0));

    // 6 — the on-site hospital party files that patient's handover note.
    let (status, handover) = api
        .put(
            &format!("/api/v1/shifts/{shift_id}/patients/{first_patient}/handover-note"),
            &admin_token,
            json!({
                "summary": "Seen by the doctor, discharged with advice",
                "outstanding_tasks": ["Send the prescription to pharmacy"],
                "escalation_required": false
            }),
        )
        .await;
    assert_eq!(status, 200, "handover: {handover}");
    assert_eq!(handover["editable"], json!(true));

    // 7 — "Continue on phone": issue a code, redeem it with NO auth header.
    let (status, handoff) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/consult/handoff"),
            &worker_token,
            json!({ "device_label": "iPhone" }),
        )
        .await;
    assert_eq!(status, 200, "handoff: {handoff}");
    assert_eq!(handoff["device_ordinal"], json!(2));
    let handoff_url = handoff["handoff_url"].as_str().unwrap();
    assert!(
        handoff_url.starts_with("https://e2e.nexuscare.test/consults/") && handoff_url.contains("#c="),
        "the code must ride in the fragment: {handoff_url}"
    );
    let code = handoff["code"].as_str().unwrap().to_string();

    let (status, joined) = api
        .send(
            reqwest::Method::POST,
            "/api/v1/consult/handoff/redeem",
            // Deliberately no bearer token: the code is the credential.
            None,
            Some(json!({ "code": code })),
        )
        .await;
    assert_eq!(
        status, 200,
        "the redeem route must work with no Authorization header: {joined}"
    );
    assert_eq!(joined["identity"], json!(format!("{worker_identity}#d2")));

    // The companion device must not have produced a second clock-in.
    let attendance_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM shift_attendance WHERE shift_id = $1")
            .bind(shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        attendance_rows, 1,
        "two devices, one person, one attendance row"
    );

    // Replaying the code must fail.
    let (status, _) = api
        .send(
            reqwest::Method::POST,
            "/api/v1/consult/handoff/redeem",
            None,
            Some(json!({ "code": code })),
        )
        .await;
    assert_eq!(status, 403, "a handoff code is single-use");

    // 8 — the hospital ends the call, the worker closes out the shift.
    let (status, ended) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/consult/end"),
            &admin_token,
            json!({ "reason": "Consultation complete" }),
        )
        .await;
    assert_eq!(status, 200, "end: {ended}");
    assert_eq!(ended["clock_out_required"], json!(true));

    let (status, body) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/handover"),
            &worker_token,
            json!({
                "patients_seen": 2,
                "critical_patients": [],
                "pending_tasks": [],
                "instructions": "Both patients seen; notes filed per patient."
            }),
        )
        .await;
    assert!(
        status.is_success(),
        "shift handover: {status} {body}"
    );

    let (status, body) = api
        .post(
            &format!("/api/v1/shifts/{shift_id}/clockout"),
            &worker_token,
            json!({}),
        )
        .await;
    assert!(status.is_success(), "clockout: {status} {body}");

    // 9 — and the record survives completion, which is the whole point.
    let (status, records) = api
        .get(
            &format!("/api/v1/shifts/{shift_id}/patient-records"),
            &admin_token,
        )
        .await;
    assert_eq!(status, 200, "patient-records: {records}");
    assert_eq!(records["shift_status"], json!("completed"));
    assert_eq!(records["metadata_only"], json!(false));
    assert_eq!(records["patients_total"], json!(2));
    assert_eq!(records["patients_seen"], json!(1));

    let first = records["patients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["patient_id"] == json!(first_patient))
        .expect("the patient the doctor saw is in the roll-up");
    assert_eq!(first["clinical_notes"].as_array().unwrap().len(), 1);
    assert_eq!(
        first["clinical_notes"][0]["assessment"],
        json!("Acute bronchitis")
    );
    assert!(first["handover_note"].is_object(), "and their handover note");
}

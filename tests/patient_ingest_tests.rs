//! Integration tests for the patient intake -> ml-service -> prediction
//! pipeline. Requires a reachable Postgres — set TEST_DATABASE_URL (falls
//! back to a local `nexuscare_test` database). Skips (prints a notice and
//! returns early) rather than failing the suite if no database is reachable,
//! consistent with this repo not having CI-provisioned Postgres today.

use std::sync::Arc;

use nexuscare_backend::models::patient::NewPatientRequest;
use nexuscare_backend::models::patient_prediction::PipelineEvent;
use nexuscare_backend::repositories::{PatientPredictionRepository, PatientRepository};
use nexuscare_backend::services::{MlClient, PatientPredictionService};
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

/// Inserts a minimal hospital + user row so patients' FK constraints are
/// satisfied, and returns (hospital_id, user_id).
async fn seed_hospital_and_user(pool: &PgPool) -> (Uuid, Uuid) {
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
    .expect("failed to seed hospital");

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
    .expect("failed to seed user");

    (hospital_id, user_id)
}

fn sample_patient_request() -> NewPatientRequest {
    NewPatientRequest {
        full_name: "Integration Test Patient".to_string(),
        age: 45.0,
        gender: "Female".to_string(),
        blood_group: "O+".to_string(),
        genotype: "AA".to_string(),
        height_cm: 165.0,
        weight_kg: 68.0,
        symptoms: "Cough, Fever".to_string(),
        existing_conditions: "None".to_string(),
        disease_type: None,
        severity_level: "Moderate".to_string(),
        weather_condition: "Dry".to_string(),
        smoking_status: false,
        alcohol_consumption: false,
        exercise_habits: "Weekly".to_string(),
        diet_type: "Mixed".to_string(),
        water_source: "Tap".to_string(),
        patient_category: "Adult".to_string(),
        predictive_risk_score: None,
    }
}

/// Ingest creates a patient row and queues a `pending` prediction.
#[tokio::test]
async fn ingest_patient_queues_pending_prediction() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let (hospital_id, user_id) = seed_hospital_and_user(&pool).await;

    let patient_repo = Arc::new(PatientRepository::new(pool.clone()));
    let prediction_repo = Arc::new(PatientPredictionRepository::new(pool.clone()));
    let ml_client = Arc::new(MlClient::new(String::new())); // mock mode
    let (tx, _rx) = tokio::sync::broadcast::channel::<PipelineEvent>(16);

    let service = PatientPredictionService::new(
        pool.clone(),
        patient_repo,
        prediction_repo,
        ml_client,
        Arc::new(tx),
    );

    let (patient, prediction) = service
        .ingest_patient(hospital_id, user_id, sample_patient_request())
        .await
        .expect("ingest_patient failed");

    assert_eq!(patient.hospital_id, hospital_id);
    assert_eq!(patient.full_name, "Integration Test Patient");
    assert_eq!(prediction.status, "pending");
    assert_eq!(prediction.patient_id, patient.id);
}

/// The worker's batch processor picks up a pending row, calls ml-service
/// (mock mode), and transitions it to `completed`, broadcasting an SSE event.
#[tokio::test]
async fn worker_completes_pending_prediction_via_mock_ml_client() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let (hospital_id, user_id) = seed_hospital_and_user(&pool).await;

    let patient_repo = Arc::new(PatientRepository::new(pool.clone()));
    let prediction_repo = Arc::new(PatientPredictionRepository::new(pool.clone()));
    let ml_client = Arc::new(MlClient::new(String::new())); // mock mode — no real ml-service needed
    let (tx, mut rx) = tokio::sync::broadcast::channel::<PipelineEvent>(16);

    let service = PatientPredictionService::new(
        pool.clone(),
        patient_repo,
        prediction_repo,
        ml_client,
        Arc::new(tx),
    );

    let (patient, prediction) = service
        .ingest_patient(hospital_id, user_id, sample_patient_request())
        .await
        .expect("ingest_patient failed");

    // >=1 rather than ==1: other tests in this file share the same database
    // and may leave their own pending rows for this batch to pick up too.
    // The assertions below confirm THIS test's prediction specifically
    // completed, which is what actually matters.
    let processed = service
        .process_pending_batch(10, 3)
        .await
        .expect("process_pending_batch failed");
    assert!(processed >= 1);

    let latest = service
        .latest_for_patient(patient.id)
        .await
        .expect("latest_for_patient failed")
        .expect("expected a prediction to exist");

    assert_eq!(latest.id, prediction.id);
    assert_eq!(latest.status, "completed");
    assert!(latest.diagnosis_condition.is_some());
    assert!(latest.risk_level.is_some());

    // The SSE broadcast should carry the same completed prediction. The
    // batch may also process leftover pending rows from other test runs
    // sharing this database (nothing truncates between runs), so scan for
    // the event addressed to *our* patient rather than assuming it's first,
    // capped at `processed` iterations so a genuine miss still fails loudly.
    let mut found = false;
    for _ in 0..processed {
        if let PipelineEvent::PredictionCompleted {
            hospital_id: evt_hospital,
            patient_id: evt_patient,
            prediction: evt_prediction,
        } = rx.recv().await.expect("expected a PipelineEvent")
        {
            if evt_patient == patient.id {
                assert_eq!(evt_hospital, hospital_id);
                assert_eq!(evt_prediction.status, "completed");
                found = true;
                break;
            }
        }
    }
    assert!(
        found,
        "expected a PredictionCompleted event for patient {}",
        patient.id
    );
}

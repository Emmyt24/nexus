// ! Orchestrates the patient -> ml-service -> SSE pipeline. Mirrors
// email_outbox_service.rs's Service + Worker split: `PatientPredictionService`
// holds the one-shot `process_pending_batch` business logic, and the thin
// `PatientPredictionWorker` wraps it in a poll loop (see src/schedulers/*.rs
// for the same shape).

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::models::patient::{NewPatientRequest, Patient};
use crate::models::patient_prediction::{PatientPrediction, PipelineEvent, PredictionResponse};
use crate::repositories::patient::{PatientRepository, RepositoryError as PatientRepoError};
use crate::repositories::patient_prediction::PatientPredictionRepository;
use crate::services::ml_client::{MlClient, MlPredictionRequest};

#[derive(Debug, thiserror::Error)]
pub enum PatientPredictionError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Patient repository error: {0}")]
    PatientRepo(#[from] PatientRepoError),
}

pub struct PatientPredictionService {
    pool: PgPool,
    patient_repo: Arc<PatientRepository>,
    prediction_repo: Arc<PatientPredictionRepository>,
    ml_client: Arc<MlClient>,
    event_tx: Arc<broadcast::Sender<PipelineEvent>>,
}

impl PatientPredictionService {
    pub fn new(
        pool: PgPool,
        patient_repo: Arc<PatientRepository>,
        prediction_repo: Arc<PatientPredictionRepository>,
        ml_client: Arc<MlClient>,
        event_tx: Arc<broadcast::Sender<PipelineEvent>>,
    ) -> Self {
        Self {
            pool,
            patient_repo,
            prediction_repo,
            ml_client,
            event_tx,
        }
    }

    /// Inserts the patient + a `pending` prediction row in one transaction
    /// (so a failure never leaves an orphaned patient with no queued
    /// prediction) and returns immediately — `PatientPredictionWorker`
    /// picks the row up on its next poll tick.
    pub async fn ingest_patient(
        &self,
        hospital_id: Uuid,
        created_by: Uuid,
        req: NewPatientRequest,
    ) -> Result<(Patient, PatientPrediction), PatientPredictionError> {
        let mut tx = self.pool.begin().await?;

        let patient = self
            .patient_repo
            .create(&mut tx, hospital_id, created_by, req)
            .await?;
        let prediction = self.prediction_repo.create_pending(&mut tx, patient.id).await?;

        tx.commit().await?;

        Ok((patient, prediction))
    }

    /// Claims up to `batch_size` pending predictions and attempts each
    /// against ml-service. Returns the number that completed successfully
    /// this tick. Failures under `max_attempts` are left `pending` for the
    /// next tick; once attempts are exhausted the row is marked `failed` and
    /// a `PredictionFailed` event is broadcast.
    pub async fn process_pending_batch(
        &self,
        batch_size: i64,
        max_attempts: i32,
    ) -> Result<usize, PatientPredictionError> {
        let pending = self.prediction_repo.fetch_pending(batch_size).await?;
        let mut processed = 0usize;

        for prediction in pending {
            let attempts = self.prediction_repo.mark_processing(prediction.id).await?;

            let patient = match self.patient_repo.find_by_id(prediction.patient_id).await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    // Shouldn't happen — patients are never deleted and the
                    // FK guarantees the row existed at insert time — but
                    // fail closed rather than loop forever. No hospital_id
                    // is knowable here, so no SSE event is emitted.
                    let error = format!("patient {} not found", prediction.patient_id);
                    tracing::error!("{error}");
                    self.prediction_repo.mark_failed(prediction.id, &error).await?;
                    continue;
                }
                Err(e) => {
                    tracing::error!("Failed to load patient {}: {}", prediction.patient_id, e);
                    self.prediction_repo
                        .reschedule(prediction.id, &e.to_string())
                        .await?;
                    continue;
                }
            };

            let ml_request = MlPredictionRequest::from(&patient);
            match self.ml_client.predict_full(&ml_request).await {
                Ok(result) => {
                    let updated = self
                        .prediction_repo
                        .mark_completed(prediction.id, &result)
                        .await?;
                    processed += 1;
                    self.emit(PipelineEvent::PredictionCompleted {
                        hospital_id: patient.hospital_id,
                        patient_id: patient.id,
                        prediction: PredictionResponse::from(updated),
                    });
                }
                Err(err) => {
                    let message = err.to_string();
                    if attempts >= max_attempts {
                        self.prediction_repo
                            .mark_failed(prediction.id, &message)
                            .await?;
                        self.emit(PipelineEvent::PredictionFailed {
                            hospital_id: patient.hospital_id,
                            patient_id: patient.id,
                            prediction_id: prediction.id,
                            error: message,
                        });
                    } else {
                        tracing::warn!(
                            "ml-service call failed for prediction {} (attempt {}/{}): {}",
                            prediction.id,
                            attempts,
                            max_attempts,
                            message
                        );
                        self.prediction_repo
                            .reschedule(prediction.id, &message)
                            .await?;
                    }
                }
            }
        }

        Ok(processed)
    }

    /// No-op if there are no active SSE subscribers — `send` only errors
    /// when the channel has zero receivers, which just means nobody's
    /// listening right now.
    fn emit(&self, event: PipelineEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Used by `GET /api/v1/patients/{id}` as a poll fallback for clients
    /// not using SSE.
    pub async fn latest_for_patient(
        &self,
        patient_id: Uuid,
    ) -> Result<Option<PredictionResponse>, PatientPredictionError> {
        let prediction = self.prediction_repo.find_by_patient_id(patient_id).await?;
        Ok(prediction.map(PredictionResponse::from))
    }
}

pub struct PatientPredictionWorker {
    service: Arc<PatientPredictionService>,
    batch_size: i64,
    max_attempts: i32,
    poll_secs: u64,
}

impl PatientPredictionWorker {
    pub fn new(service: Arc<PatientPredictionService>) -> Self {
        let batch_size = std::env::var("PATIENT_PREDICTION_BATCH_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);

        let max_attempts = std::env::var("PATIENT_PREDICTION_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        let poll_secs = std::env::var("PATIENT_PREDICTION_POLL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);

        Self {
            service,
            batch_size,
            max_attempts,
            poll_secs,
        }
    }

    pub async fn run(self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(self.poll_secs));
        loop {
            interval.tick().await;
            match self
                .service
                .process_pending_batch(self.batch_size, self.max_attempts)
                .await
            {
                Ok(count) if count > 0 => {
                    tracing::info!("Patient prediction worker completed {} prediction(s)", count);
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::error!("Patient prediction worker tick failed: {}", err);
                }
            }
        }
    }
}

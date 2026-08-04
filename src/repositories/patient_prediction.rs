use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::patient_prediction::PatientPrediction;
use crate::services::ml_client::MlPredictionResponse;

const PREDICTION_COLUMNS: &str = r#"
    id, patient_id, status, attempts, last_error,
    diagnosis_condition, diagnosis_confidence, diagnosis_probabilities,
    risk_level, risk_score, deterioration_probability, risk_probabilities,
    drug_recommendation, recommendation_confidence, recommendations, urgency,
    route_to, department, alert_priority,
    raw_response, created_at, updated_at, completed_at
"#;

pub struct PatientPredictionRepository {
    pool: PgPool,
}

impl PatientPredictionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Queue a prediction row in `pending` state, within the same
    /// transaction as the patient insert.
    pub async fn create_pending(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        patient_id: Uuid,
    ) -> Result<PatientPrediction, sqlx::Error> {
        let query = format!(
            "INSERT INTO patient_predictions (patient_id) VALUES ($1) RETURNING {PREDICTION_COLUMNS}"
        );
        sqlx::query_as::<_, PatientPrediction>(&query)
            .bind(patient_id)
            .fetch_one(&mut **tx)
            .await
    }

    /// Rows the worker should attempt next, oldest first.
    pub async fn fetch_pending(&self, limit: i64) -> Result<Vec<PatientPrediction>, sqlx::Error> {
        let query = format!(
            "SELECT {PREDICTION_COLUMNS} FROM patient_predictions
             WHERE status = 'pending' ORDER BY created_at ASC LIMIT $1"
        );
        sqlx::query_as::<_, PatientPrediction>(&query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
    }

    /// Claim a row before calling ml-service; returns the attempt count
    /// after incrementing, so the caller can decide whether this was the
    /// final allowed attempt.
    pub async fn mark_processing(&self, id: Uuid) -> Result<i32, sqlx::Error> {
        sqlx::query_scalar(
            r#"
            UPDATE patient_predictions
            SET status = 'processing', attempts = attempts + 1, updated_at = NOW()
            WHERE id = $1
            RETURNING attempts
            "#,
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn find_by_patient_id(
        &self,
        patient_id: Uuid,
    ) -> Result<Option<PatientPrediction>, sqlx::Error> {
        let query = format!(
            "SELECT {PREDICTION_COLUMNS} FROM patient_predictions
             WHERE patient_id = $1 ORDER BY created_at DESC LIMIT 1"
        );
        sqlx::query_as::<_, PatientPrediction>(&query)
            .bind(patient_id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn mark_completed(
        &self,
        id: Uuid,
        result: &MlPredictionResponse,
    ) -> Result<PatientPrediction, sqlx::Error> {
        let query = format!(
            r#"
            UPDATE patient_predictions
            SET status = 'completed',
                diagnosis_condition = $2,
                diagnosis_confidence = $3,
                diagnosis_probabilities = $4,
                risk_level = $5,
                risk_score = $6,
                deterioration_probability = $7,
                risk_probabilities = $8,
                drug_recommendation = $9,
                recommendation_confidence = $10,
                recommendations = $11,
                urgency = $12,
                route_to = $13,
                department = $14,
                alert_priority = $15,
                raw_response = $16,
                completed_at = NOW(),
                updated_at = NOW()
            WHERE id = $1
            RETURNING {PREDICTION_COLUMNS}
            "#
        );

        sqlx::query_as::<_, PatientPrediction>(&query)
            .bind(id)
            .bind(&result.diagnosis.probable_condition)
            .bind(result.diagnosis.confidence)
            .bind(serde_json::to_value(&result.diagnosis.all_probabilities).ok())
            .bind(&result.risk.risk_level)
            .bind(result.risk.risk_score)
            .bind(result.risk.deterioration_probability)
            .bind(serde_json::to_value(&result.risk.all_probabilities).ok())
            .bind(&result.recommendation.drug_recommendation)
            .bind(result.recommendation.confidence)
            .bind(serde_json::to_value(&result.recommendation.recommendations).ok())
            .bind(&result.recommendation.urgency)
            .bind(&result.routing.route_to)
            .bind(&result.routing.department)
            .bind(result.routing.alert_priority)
            .bind(serde_json::to_value(result).ok())
            .fetch_one(&self.pool)
            .await
    }

    pub async fn mark_failed(&self, id: Uuid, error: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE patient_predictions
            SET status = 'failed', last_error = $2, updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Leaves the row `pending` for the next poll tick, recording the error
    /// so `last_error` is visible even mid-retry.
    pub async fn reschedule(&self, id: Uuid, error: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE patient_predictions
            SET status = 'pending', last_error = $2, updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

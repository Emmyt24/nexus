use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// Maps to the `patient_predictions` table — one row per prediction attempt,
/// doubling as the async job queue `PatientPredictionWorker` polls (mirrors
/// `email_outbox`'s pending/processing/... lifecycle).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PatientPrediction {
    pub id: Uuid,
    pub patient_id: Uuid,
    pub status: String,
    pub attempts: i32,
    pub last_error: Option<String>,

    pub diagnosis_condition: Option<String>,
    pub diagnosis_confidence: Option<f32>,
    pub diagnosis_probabilities: Option<Value>,

    pub risk_level: Option<String>,
    pub risk_score: Option<f32>,
    pub deterioration_probability: Option<f32>,
    pub risk_probabilities: Option<Value>,

    pub drug_recommendation: Option<String>,
    pub recommendation_confidence: Option<f32>,
    pub recommendations: Option<Value>,
    pub urgency: Option<String>,

    pub route_to: Option<String>,
    pub department: Option<String>,
    pub alert_priority: Option<i16>,

    pub raw_response: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Response shape for `GET /api/v1/patients/{id}` and the SSE payload.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PredictionResponse {
    pub id: Uuid,
    pub patient_id: Uuid,
    pub status: String,
    pub diagnosis_condition: Option<String>,
    pub diagnosis_confidence: Option<f32>,
    pub diagnosis_probabilities: Option<Value>,
    pub risk_level: Option<String>,
    pub risk_score: Option<f32>,
    pub deterioration_probability: Option<f32>,
    pub risk_probabilities: Option<Value>,
    pub drug_recommendation: Option<String>,
    pub recommendation_confidence: Option<f32>,
    pub recommendations: Option<Value>,
    pub urgency: Option<String>,
    pub route_to: Option<String>,
    pub department: Option<String>,
    pub alert_priority: Option<i16>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl From<PatientPrediction> for PredictionResponse {
    fn from(p: PatientPrediction) -> Self {
        Self {
            id: p.id,
            patient_id: p.patient_id,
            status: p.status,
            diagnosis_condition: p.diagnosis_condition,
            diagnosis_confidence: p.diagnosis_confidence,
            diagnosis_probabilities: p.diagnosis_probabilities,
            risk_level: p.risk_level,
            risk_score: p.risk_score,
            deterioration_probability: p.deterioration_probability,
            risk_probabilities: p.risk_probabilities,
            drug_recommendation: p.drug_recommendation,
            recommendation_confidence: p.recommendation_confidence,
            recommendations: p.recommendations,
            urgency: p.urgency,
            route_to: p.route_to,
            department: p.department,
            alert_priority: p.alert_priority,
            last_error: p.last_error,
            created_at: p.created_at,
            completed_at: p.completed_at,
        }
    }
}

/// SSE broadcast payload — pushed onto `AppState::pipeline_events` by the
/// worker after each prediction attempt, filtered per-connection by
/// `hospital_id` in the `pipeline_events` handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PipelineEvent {
    PredictionCompleted {
        hospital_id: Uuid,
        patient_id: Uuid,
        prediction: PredictionResponse,
    },
    PredictionFailed {
        hospital_id: Uuid,
        patient_id: Uuid,
        prediction_id: Uuid,
        error: String,
    },
}

impl PipelineEvent {
    pub fn hospital_id(&self) -> Uuid {
        match self {
            PipelineEvent::PredictionCompleted { hospital_id, .. } => *hospital_id,
            PipelineEvent::PredictionFailed { hospital_id, .. } => *hospital_id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            PipelineEvent::PredictionCompleted { .. } => "prediction_completed",
            PipelineEvent::PredictionFailed { .. } => "prediction_failed",
        }
    }
}

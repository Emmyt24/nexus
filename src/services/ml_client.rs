// ! HTTP client for the NexusCare ML service (ml-service/, FastAPI). An empty
// base_url flips the client into mock mode, mirroring SafeHavenClient, so
// local dev/tests don't need the Python service running.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::models::patient::Patient;

#[derive(Debug, thiserror::Error)]
pub enum MlClientError {
    #[error("HTTP request to ml-service failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("ml-service returned HTTP {0}: {1}")]
    BadStatus(u16, String),
}

/// Mirrors ml-service/main.py's `PatientFeatures` Pydantic model field-for-
/// field (snake_case on both sides, no renaming needed).
#[derive(Debug, Clone, Serialize)]
pub struct MlPredictionRequest {
    pub patient_id: String,
    pub symptoms: String,
    pub existing_conditions: String,
    pub blood_group: String,
    pub genotype: String,
    pub age: f32,
    pub gender: String,
    pub height_cm: f32,
    pub weight_kg: f32,
    pub disease_type: Option<String>,
    pub severity_level: String,
    pub weather_condition: String,
    pub smoking_status: bool,
    pub alcohol_consumption: bool,
    pub exercise_habits: String,
    pub diet_type: String,
    pub water_source: String,
    pub patient_category: String,
    pub predictive_risk_score: Option<f32>,
}

impl From<&Patient> for MlPredictionRequest {
    fn from(p: &Patient) -> Self {
        Self {
            patient_id: p.id.to_string(),
            symptoms: p.symptoms.clone(),
            existing_conditions: p.existing_conditions.clone(),
            blood_group: p.blood_group.clone(),
            genotype: p.genotype.clone(),
            age: p.age,
            gender: p.gender.clone(),
            height_cm: p.height_cm,
            weight_kg: p.weight_kg,
            disease_type: p.disease_type.clone(),
            severity_level: p.severity_level.clone(),
            weather_condition: p.weather_condition.clone(),
            smoking_status: p.smoking_status,
            alcohol_consumption: p.alcohol_consumption,
            exercise_habits: p.exercise_habits.clone(),
            diet_type: p.diet_type.clone(),
            water_source: p.water_source.clone(),
            patient_category: p.patient_category.clone(),
            predictive_risk_score: p.predictive_risk_score,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosisResult {
    pub probable_condition: String,
    pub confidence: f32,
    pub all_probabilities: std::collections::HashMap<String, f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskResult {
    pub risk_level: String,
    pub risk_score: f32,
    pub deterioration_probability: f32,
    pub all_probabilities: std::collections::HashMap<String, f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendationResult {
    pub drug_recommendation: String,
    pub confidence: f32,
    pub recommendations: Vec<String>,
    pub urgency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingResult {
    pub route_to: String,
    pub department: String,
    pub alert_priority: i16,
}

/// Mirrors `POST /predict/full`'s response shape exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlPredictionResponse {
    pub patient_id: String,
    pub diagnosis: DiagnosisResult,
    pub risk: RiskResult,
    pub recommendation: RecommendationResult,
    pub routing: RoutingResult,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MlHealthResponse {
    pub status: String,
    pub models_loaded: bool,
}

#[derive(Debug, Clone)]
pub struct MlClient {
    http: Client,
    base_url: String,
}

impl MlClient {
    pub fn new(base_url: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build ml-service HTTP client");
        Self { http, base_url }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var("ML_SERVICE_URL").unwrap_or_default())
    }

    /// An empty `base_url` flips the client into mock mode — used for local
    /// dev/tests when the ml-service container isn't running.
    pub fn is_mock(&self) -> bool {
        self.base_url.trim().is_empty()
    }

    pub async fn health(&self) -> Result<MlHealthResponse, MlClientError> {
        if self.is_mock() {
            return Ok(MlHealthResponse {
                status: "ok".to_string(),
                models_loaded: true,
            });
        }

        let url = format!("{}/health", self.base_url);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(MlClientError::BadStatus(status, text));
        }
        Ok(resp.json().await?)
    }

    pub async fn predict_full(
        &self,
        req: &MlPredictionRequest,
    ) -> Result<MlPredictionResponse, MlClientError> {
        if self.is_mock() {
            tracing::info!(
                "[ML-CLIENT MOCK] predict_full patient_id={}",
                req.patient_id
            );
            return Ok(mock_prediction(req));
        }

        let url = format!("{}/predict/full", self.base_url);
        let resp = self.http.post(&url).json(req).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(MlClientError::BadStatus(status, text));
        }

        Ok(resp.json().await?)
    }
}

fn mock_prediction(req: &MlPredictionRequest) -> MlPredictionResponse {
    let mut diag_probs = std::collections::HashMap::new();
    diag_probs.insert("Infectious".to_string(), 0.7);
    diag_probs.insert("Chronic".to_string(), 0.15);
    diag_probs.insert("Genetic".to_string(), 0.1);
    diag_probs.insert("MentalHealth".to_string(), 0.05);

    let mut risk_probs = std::collections::HashMap::new();
    risk_probs.insert("Low".to_string(), 0.7);
    risk_probs.insert("Medium".to_string(), 0.2);
    risk_probs.insert("High".to_string(), 0.1);

    MlPredictionResponse {
        patient_id: req.patient_id.clone(),
        diagnosis: DiagnosisResult {
            probable_condition: "Infectious".to_string(),
            confidence: 0.7,
            all_probabilities: diag_probs,
        },
        risk: RiskResult {
            risk_level: "Low".to_string(),
            risk_score: 0.7,
            deterioration_probability: 0.1,
            all_probabilities: risk_probs,
        },
        recommendation: RecommendationResult {
            drug_recommendation: "Consult specialist".to_string(),
            confidence: 0.5,
            recommendations: vec!["Recommended treatment: Consult specialist".to_string()],
            urgency: "routine".to_string(),
        },
        routing: RoutingResult {
            route_to: "gp".to_string(),
            department: "General Medicine".to_string(),
            alert_priority: 3,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_patient() -> Patient {
        Patient {
            id: uuid::Uuid::new_v4(),
            hospital_id: uuid::Uuid::new_v4(),
            created_by: uuid::Uuid::new_v4(),
            full_name: "Test Patient".to_string(),
            age: 40.0,
            gender: "Female".to_string(),
            blood_group: "O+".to_string(),
            genotype: "AA".to_string(),
            height_cm: 165.0,
            weight_kg: 60.0,
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn mock_client_returns_canned_prediction() {
        let client = MlClient::new(String::new());
        assert!(client.is_mock());

        let patient = sample_patient();
        let req = MlPredictionRequest::from(&patient);
        let resp = client.predict_full(&req).await.unwrap();

        assert_eq!(resp.patient_id, patient.id.to_string());
        assert!(!resp.diagnosis.probable_condition.is_empty());
    }

    #[tokio::test]
    async fn mock_client_health_reports_loaded() {
        let client = MlClient::new(String::new());
        let health = client.health().await.unwrap();
        assert!(health.models_loaded);
    }
}

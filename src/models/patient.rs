use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

/// Core patient intake record — maps to the `patients` table. Field set
/// mirrors ml-service's `PatientFeatures` schema (ml-service/main.py) so the
/// row can be sent to `/predict/full` with no translation.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Patient {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub created_by: Uuid,
    pub full_name: String,
    pub age: f32,
    pub gender: String,
    pub blood_group: String,
    pub genotype: String,
    pub height_cm: f32,
    pub weight_kg: f32,
    pub symptoms: String,
    pub existing_conditions: String,
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
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload for `POST /api/v1/ingest/patient`. `hospital_id`/`created_by`
/// aren't accepted from the client — they come from the caller's JWT claims.
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct NewPatientRequest {
    #[validate(length(min = 1, max = 255, message = "Full name is required"))]
    pub full_name: String,

    #[validate(range(min = 0.0, max = 120.0, message = "Age must be between 0 and 120"))]
    pub age: f32,

    #[serde(default = "default_gender")]
    pub gender: String,

    #[serde(default = "default_blood_group")]
    pub blood_group: String,

    #[serde(default = "default_genotype")]
    pub genotype: String,

    #[serde(default = "default_height_cm")]
    pub height_cm: f32,

    #[serde(default = "default_weight_kg")]
    pub weight_kg: f32,

    #[serde(default)]
    pub symptoms: String,

    #[serde(default = "default_existing_conditions")]
    pub existing_conditions: String,

    /// Optional override consumed by the recommendation + routing stages
    /// only — NOT fed to the diagnosis model, and not auto-chained from its
    /// prediction. Matches ml-service's actual (slightly surprising)
    /// behavior; see ml-service/main.py's `_predict_recommendation`.
    pub disease_type: Option<String>,

    #[serde(default = "default_severity_level")]
    pub severity_level: String,

    #[serde(default = "default_weather_condition")]
    pub weather_condition: String,

    #[serde(default)]
    pub smoking_status: bool,

    #[serde(default)]
    pub alcohol_consumption: bool,

    #[serde(default = "default_exercise_habits")]
    pub exercise_habits: String,

    #[serde(default = "default_diet_type")]
    pub diet_type: String,

    #[serde(default = "default_water_source")]
    pub water_source: String,

    #[serde(default = "default_patient_category")]
    pub patient_category: String,

    pub predictive_risk_score: Option<f32>,
}

fn default_gender() -> String {
    "Male".to_string()
}
fn default_blood_group() -> String {
    "O+".to_string()
}
fn default_genotype() -> String {
    "AA".to_string()
}
fn default_height_cm() -> f32 {
    170.0
}
fn default_weight_kg() -> f32 {
    70.0
}
fn default_existing_conditions() -> String {
    "None".to_string()
}
fn default_severity_level() -> String {
    "Mild".to_string()
}
fn default_weather_condition() -> String {
    "Dry".to_string()
}
fn default_exercise_habits() -> String {
    "Weekly".to_string()
}
fn default_diet_type() -> String {
    "Mixed".to_string()
}
fn default_water_source() -> String {
    "Tap".to_string()
}
fn default_patient_category() -> String {
    "Adult".to_string()
}

/// Response shape for patient endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PatientResponse {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub full_name: String,
    pub age: f32,
    pub gender: String,
    pub symptoms: String,
    pub existing_conditions: String,
    pub severity_level: String,
    pub created_at: DateTime<Utc>,
}

impl From<Patient> for PatientResponse {
    fn from(p: Patient) -> Self {
        Self {
            id: p.id,
            hospital_id: p.hospital_id,
            full_name: p.full_name,
            age: p.age,
            gender: p.gender,
            symptoms: p.symptoms,
            existing_conditions: p.existing_conditions,
            severity_level: p.severity_level,
            created_at: p.created_at,
        }
    }
}

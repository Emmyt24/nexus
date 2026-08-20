use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::patient::{NewPatientRequest, Patient};

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),

    #[error("Patient not found: {0}")]
    NotFound(Uuid),
}

pub struct PatientRepository {
    pool: PgPool,
}

impl PatientRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a patient intake row within the caller's transaction (the
    /// handler also creates the pending prediction row in the same
    /// transaction, so a failure never leaves an orphaned patient).
    pub async fn create(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        created_by: Uuid,
        req: NewPatientRequest,
    ) -> Result<Patient, RepositoryError> {
        let patient = sqlx::query_as::<_, Patient>(
            r#"
            INSERT INTO patients (
                hospital_id, created_by, full_name, age, gender, blood_group, genotype,
                height_cm, weight_kg, symptoms, existing_conditions, disease_type,
                severity_level, weather_condition, smoking_status, alcohol_consumption,
                exercise_habits, diet_type, water_source, patient_category, predictive_risk_score
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
            RETURNING
                id, hospital_id, created_by, full_name, age, gender, blood_group, genotype,
                height_cm, weight_kg, symptoms, existing_conditions, disease_type,
                severity_level, weather_condition, smoking_status, alcohol_consumption,
                exercise_habits, diet_type, water_source, patient_category, predictive_risk_score,
                created_at, updated_at
            "#,
        )
        .bind(hospital_id)
        .bind(created_by)
        .bind(&req.full_name)
        .bind(req.age)
        .bind(&req.gender)
        .bind(&req.blood_group)
        .bind(&req.genotype)
        .bind(req.height_cm)
        .bind(req.weight_kg)
        .bind(&req.symptoms)
        .bind(&req.existing_conditions)
        .bind(&req.disease_type)
        .bind(&req.severity_level)
        .bind(&req.weather_condition)
        .bind(req.smoking_status)
        .bind(req.alcohol_consumption)
        .bind(&req.exercise_habits)
        .bind(&req.diet_type)
        .bind(&req.water_source)
        .bind(&req.patient_category)
        .bind(req.predictive_risk_score)
        .fetch_one(&mut **tx)
        .await?;

        Ok(patient)
    }

    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<Patient>, RepositoryError> {
        let patient = sqlx::query_as::<_, Patient>(
            r#"
            SELECT
                id, hospital_id, created_by, full_name, age, gender, blood_group, genotype,
                height_cm, weight_kg, symptoms, existing_conditions, disease_type,
                severity_level, weather_condition, smoking_status, alcohol_consumption,
                exercise_habits, diet_type, water_source, patient_category, predictive_risk_score,
                created_at, updated_at
            FROM patients
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(patient)
    }
}

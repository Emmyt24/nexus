-- Patients table
-- Intake data submitted by a HealthWorker/HospitalAdmin for ML triage.
-- Field set mirrors ml-service's PatientFeatures schema (ml-service/main.py)
-- so no translation guesswork happens when building the prediction request.

CREATE TABLE patients (
    id                     UUID          PRIMARY KEY DEFAULT gen_random_uuid(),
    hospital_id            UUID          NOT NULL REFERENCES hospitals (id),
    created_by             UUID          NOT NULL REFERENCES users (id),
    full_name              VARCHAR(255)  NOT NULL,
    age                    REAL          NOT NULL,
    gender                 VARCHAR(20)   NOT NULL DEFAULT 'Male',
    blood_group            VARCHAR(5)    NOT NULL DEFAULT 'O+',
    genotype               VARCHAR(5)    NOT NULL DEFAULT 'AA',
    height_cm              REAL          NOT NULL DEFAULT 170,
    weight_kg              REAL          NOT NULL DEFAULT 70,
    symptoms               TEXT          NOT NULL DEFAULT '',
    existing_conditions    TEXT          NOT NULL DEFAULT 'None',
    disease_type           VARCHAR(50),
    severity_level         VARCHAR(20)   NOT NULL DEFAULT 'Mild',
    weather_condition      VARCHAR(20)   NOT NULL DEFAULT 'Dry',
    smoking_status          BOOLEAN      NOT NULL DEFAULT FALSE,
    alcohol_consumption     BOOLEAN      NOT NULL DEFAULT FALSE,
    exercise_habits        VARCHAR(20)   NOT NULL DEFAULT 'Weekly',
    diet_type               VARCHAR(20)  NOT NULL DEFAULT 'Mixed',
    water_source            VARCHAR(20)  NOT NULL DEFAULT 'Tap',
    patient_category        VARCHAR(20)  NOT NULL DEFAULT 'Adult',
    predictive_risk_score   REAL,
    created_at              TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at              TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_patients_hospital_id ON patients (hospital_id);
CREATE INDEX idx_patients_created_at  ON patients (created_at DESC);

CREATE TRIGGER trg_patients_updated_at
    BEFORE UPDATE ON patients
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

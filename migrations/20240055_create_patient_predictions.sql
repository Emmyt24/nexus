-- Patient predictions
-- One row per prediction attempt. Doubles as the async outbox/job queue that
-- PatientPredictionWorker polls (mirrors the email_outbox pattern in
-- 20240019_email_outbox.sql) — status starts 'pending', the worker claims it
-- via mark_processing, then calls ml-service and records the result.

CREATE TABLE patient_predictions (
    id                         UUID          PRIMARY KEY DEFAULT gen_random_uuid(),
    patient_id                 UUID          NOT NULL REFERENCES patients (id),
    status                     VARCHAR(20)   NOT NULL DEFAULT 'pending',
    attempts                   INTEGER       NOT NULL DEFAULT 0,
    last_error                 TEXT,

    diagnosis_condition        VARCHAR(50),
    diagnosis_confidence       REAL,
    diagnosis_probabilities    JSONB,

    risk_level                 VARCHAR(20),
    risk_score                 REAL,
    deterioration_probability  REAL,
    risk_probabilities         JSONB,

    drug_recommendation        VARCHAR(100),
    recommendation_confidence  REAL,
    recommendations            JSONB,
    urgency                    VARCHAR(20),

    route_to                   VARCHAR(20),
    department                 VARCHAR(100),
    alert_priority              SMALLINT,

    raw_response                JSONB,
    created_at                  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at                  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    completed_at                 TIMESTAMPTZ
);

CREATE INDEX idx_patient_predictions_status     ON patient_predictions (status, created_at);
CREATE INDEX idx_patient_predictions_patient_id ON patient_predictions (patient_id);

CREATE TRIGGER trg_patient_predictions_updated_at
    BEFORE UPDATE ON patient_predictions
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

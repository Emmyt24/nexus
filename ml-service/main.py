"""
NexusCare ML Service — FastAPI
Serves 4 trained models for the Rust NestJS backend.

Endpoints:
  POST /predict/diagnosis
  POST /predict/risk
  POST /predict/recommendation
  POST /predict/routing
  POST /predict/full          ← all 4 in one call
  POST /retrain               ← triggers background retraining
  GET  /health
  GET  /models/info
"""
import json
import os
import asyncio
from contextlib import asynccontextmanager
from typing import Optional

import joblib
import numpy as np
from dotenv import load_dotenv
from fastapi import FastAPI, HTTPException, BackgroundTasks, Header, Depends
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel, field_validator

load_dotenv()

# Comma-separated list of origins allowed to call this API from a browser, e.g.
# "https://admin.nexuscare.example.com". Server-to-server calls (the Rust
# backend) aren't browsers and are unaffected by CORS either way — this only
# gates JS running in someone else's page. Leave unset to block all browser
# origins by default rather than the previous wildcard "*".
ALLOWED_ORIGINS = [o.strip() for o in os.getenv("ALLOWED_ORIGINS", "").split(",") if o.strip()]

# Shared secret required to trigger retraining / data export. These endpoints
# shell out to trusted subprocesses and touch the DB, so they shouldn't be
# reachable by anyone who can just curl the service.
RETRAIN_API_KEY = os.getenv("ML_RETRAIN_API_KEY")


def require_retrain_key(x_api_key: Optional[str] = Header(None, alias="X-API-Key")):
    if RETRAIN_API_KEY and x_api_key != RETRAIN_API_KEY:
        raise HTTPException(status_code=401, detail="Missing or invalid X-API-Key")

# ─── Model registry ───────────────────────────────────────────────────────────

class ModelRegistry:
    diagnosis_model = None
    risk_model = None
    recommendation_model = None
    drug_le = None
    encoders: dict = {}
    routing_rules: dict = {}
    loaded = False

    @classmethod
    def load(cls):
        try:
            cls.diagnosis_model     = joblib.load("models/diagnosis_model.pkl")
            cls.risk_model          = joblib.load("models/risk_model.pkl")
            cls.recommendation_model = joblib.load("models/recommendation_model.pkl")
            cls.drug_le             = joblib.load("models/drug_label_encoder.pkl")
            cls.encoders            = joblib.load("models/encoders.pkl")
            with open("models/routing_rules.json") as f:
                cls.routing_rules   = json.load(f)
            cls.loaded = True
            print("✓ All models loaded")
        except FileNotFoundError as e:
            print(f"⚠ Models not found ({e}). Run train_models.py first.")
            cls.loaded = False


@asynccontextmanager
async def lifespan(app: FastAPI):
    ModelRegistry.load()
    if not ALLOWED_ORIGINS:
        print("⚠ ALLOWED_ORIGINS not set — no browser origins are permitted (server-to-server calls are unaffected).")
    if not RETRAIN_API_KEY:
        print("⚠ ML_RETRAIN_API_KEY not set — /retrain and /export-training-data are UNAUTHENTICATED. Set it before deploying publicly.")
    yield


app = FastAPI(
    title="NexusCare ML Service",
    version="1.0.0",
    description="Hospital ML pipeline — 4 model inference service",
    lifespan=lifespan,
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=ALLOWED_ORIGINS,
    allow_methods=["GET", "POST"],
    allow_headers=["*"],
)


# ─── Input schemas ─────────────────────────────────────────────────────────────

class PatientFeatures(BaseModel):
    patient_id: str
    symptoms: Optional[str] = ""
    existing_conditions: Optional[str] = "None"
    blood_group: Optional[str] = "O+"
    genotype: Optional[str] = "AA"
    age: Optional[float] = 30.0
    gender: Optional[str] = "Male"
    height_cm: Optional[float] = 170.0
    weight_kg: Optional[float] = 70.0
    disease_type: Optional[str] = None
    severity_level: Optional[str] = "Mild"
    weather_condition: Optional[str] = "Dry"
    smoking_status: Optional[bool] = False
    alcohol_consumption: Optional[bool] = False
    exercise_habits: Optional[str] = "Weekly"
    diet_type: Optional[str] = "Mixed"
    water_source: Optional[str] = "Tap"
    patient_category: Optional[str] = "Adult"
    predictive_risk_score: Optional[float] = None

    @field_validator("age", mode="before")
    @classmethod
    def clamp_age(cls, v):
        if v is None: return 30.0
        return max(0.0, min(float(v), 120.0))


# ─── Preprocessing ─────────────────────────────────────────────────────────────

def safe_encode(le, value: str, fallback: int = 0) -> int:
    """Encode a value with the LabelEncoder; return fallback if unseen."""
    try:
        return int(le.transform([value])[0])
    except (ValueError, KeyError):
        return fallback


def preprocess(data: PatientFeatures) -> dict:
    enc = ModelRegistry.encoders
    scaler = enc["scaler"]

    # Normalise numeric
    raw_scaled = scaler.transform([[
        data.age or 30,
        data.height_cm or 170,
        data.weight_kg or 70,
        data.predictive_risk_score or 0.0,
    ]])
    age_norm, height_norm, weight_norm, risk_norm = raw_scaled[0]

    # Encode categoricals
    blood_enc   = safe_encode(enc["blood_group"], data.blood_group or "O+")
    genotype_enc = safe_encode(enc["genotype"], data.genotype or "AA")
    gender_enc  = safe_encode(enc["gender"], data.gender or "Male")
    disease_enc = safe_encode(enc["disease_type"], data.disease_type or "Infectious")
    severity_enc = safe_encode(enc["severity_level"], data.severity_level or "Mild")
    weather_enc = safe_encode(enc["weather_condition"], data.weather_condition or "Dry")
    category_enc = safe_encode(enc["patient_category"], data.patient_category or "Adult")
    exercise_enc = safe_encode(enc["exercise_habits"], data.exercise_habits or "Weekly")
    diet_enc    = safe_encode(enc["diet_type"], data.diet_type or "Mixed")
    water_enc   = safe_encode(enc["water_source"], data.water_source or "Tap")

    severity_ordinal = {"Mild": 0, "Moderate": 1, "Severe": 2, "Critical": 3}.get(
        data.severity_level or "Mild", 0
    )

    # TF-IDF text features
    sym_vec = enc["tfidf_symptoms"].transform([data.symptoms or ""])
    cond_vec = enc["tfidf_conditions"].transform([data.existing_conditions or "None"])

    return {
        "symptoms_vec": sym_vec.toarray()[0],
        "conditions_vec": cond_vec.toarray()[0],
        "symptoms_cols": enc["symptoms_cols"],
        "conditions_cols": enc["conditions_cols"],
        "blood_enc": blood_enc,
        "genotype_enc": genotype_enc,
        "gender_enc": gender_enc,
        "disease_enc": disease_enc,
        "severity_enc": severity_enc,
        "severity_ordinal": severity_ordinal,
        "weather_enc": weather_enc,
        "category_enc": category_enc,
        "exercise_enc": exercise_enc,
        "diet_enc": diet_enc,
        "water_enc": water_enc,
        "age_norm": age_norm,
        "height_norm": height_norm,
        "weight_norm": weight_norm,
        "risk_norm": risk_norm,
        "smoking": int(data.smoking_status or False),
        "alcohol": int(data.alcohol_consumption or False),
    }


def _require_models():
    if not ModelRegistry.loaded:
        raise HTTPException(
            status_code=503,
            detail="Models not loaded. Run train_models.py then restart the service."
        )


# ─── Prediction helpers ────────────────────────────────────────────────────────

def _predict_diagnosis(f: dict) -> dict:
    enc = ModelRegistry.encoders
    X = np.array(
        list(f["symptoms_vec"])
        + list(f["conditions_vec"])
        + [f["blood_enc"], f["genotype_enc"], f["gender_enc"],
           f["age_norm"], f["smoking"], f["alcohol"]]
    ).reshape(1, -1)

    proba = ModelRegistry.diagnosis_model.predict_proba(X)[0]
    idx = int(np.argmax(proba))
    return {
        "probable_condition": str(enc["disease_type"].classes_[idx]),
        "confidence": round(float(proba[idx]), 4),
        "all_probabilities": {
            str(cls): round(float(p), 4)
            for cls, p in zip(enc["disease_type"].classes_, proba)
        },
    }


def _predict_risk(f: dict) -> dict:
    enc = ModelRegistry.encoders
    X = np.array(
        [f["age_norm"], f["genotype_enc"], f["severity_ordinal"],
         f["smoking"], f["alcohol"], f["blood_enc"],
         f["height_norm"], f["weight_norm"]]
        + list(f["conditions_vec"])
    ).reshape(1, -1)

    proba = ModelRegistry.risk_model.predict_proba(X)[0]
    idx = int(np.argmax(proba))
    risk_level = str(enc["mortality_risk"].classes_[idx])

    # Map class probabilities to named risks
    risk_proba = {
        str(cls): round(float(p), 4)
        for cls, p in zip(enc["mortality_risk"].classes_, proba)
    }
    high_prob = risk_proba.get("High", 0.0)

    return {
        "risk_level": risk_level,
        "risk_score": round(float(max(proba)), 4),
        "deterioration_probability": round(high_prob, 4),
        "all_probabilities": risk_proba,
    }


def _predict_recommendation(f: dict, disease_type: Optional[str]) -> dict:
    enc = ModelRegistry.encoders
    disease_enc = safe_encode(enc["disease_type"], disease_type or "Infectious")

    X = np.array([
        disease_enc, f["risk_norm"], f["smoking"], f["alcohol"],
        f["exercise_enc"], f["diet_enc"], f["water_enc"],
        f["weather_enc"], f["genotype_enc"],
    ]).reshape(1, -1)

    proba = ModelRegistry.recommendation_model.predict_proba(X)[0]
    idx = int(np.argmax(proba))
    drug = str(ModelRegistry.drug_le.classes_[idx])
    confidence = round(float(proba[idx]), 4)

    # Build lifestyle recommendations on top of drug
    recs = [f"Recommended treatment: {drug}"]
    if f["smoking"]:
        recs.append("Stop smoking — significantly reduces cardiovascular risk")
    if f["alcohol"]:
        recs.append("Reduce alcohol consumption")
    if f["exercise_enc"] == safe_encode(enc["exercise_habits"], "None"):
        recs.append("Begin light exercise routine — 30 min walk 3x/week")

    risk_score_raw = f["risk_norm"]
    urgency = "emergency" if risk_score_raw > 0.7 else ("urgent" if risk_score_raw > 0.4 else "routine")

    return {
        "drug_recommendation": drug,
        "confidence": confidence,
        "recommendations": recs,
        "urgency": urgency,
    }


def _predict_routing(data: PatientFeatures) -> dict:
    rules = ModelRegistry.routing_rules

    severity = data.severity_level or "Mild"
    priority = rules["severity_to_priority"].get(severity, 3)

    # Category override takes precedence
    dept = rules["category_override"].get(data.patient_category or "")
    if not dept:
        dept = rules["disease_to_department"].get(data.disease_type or "", "General Medicine")

    route = rules["priority_to_route"].get(str(priority), "gp")

    return {
        "route_to": route,
        "department": dept,
        "alert_priority": priority,
    }


# ─── Endpoints ─────────────────────────────────────────────────────────────────

@app.get("/health")
def health():
    return {
        "status": "ok",
        "models_loaded": ModelRegistry.loaded,
        "models": {
            "diagnosis": ModelRegistry.diagnosis_model is not None,
            "risk": ModelRegistry.risk_model is not None,
            "recommendation": ModelRegistry.recommendation_model is not None,
            "routing": bool(ModelRegistry.routing_rules),
        }
    }


@app.get("/models/info")
def models_info():
    _require_models()
    enc = ModelRegistry.encoders
    return {
        "disease_classes": list(enc["disease_type"].classes_),
        "mortality_risk_classes": list(enc["mortality_risk"].classes_),
        "drug_classes": list(ModelRegistry.drug_le.classes_),
        "symptoms_vocab_size": len(enc["symptoms_cols"]),
        "conditions_vocab_size": len(enc["conditions_cols"]),
    }


@app.post("/predict/diagnosis")
def predict_diagnosis(data: PatientFeatures):
    _require_models()
    f = preprocess(data)
    return {"patient_id": data.patient_id, **_predict_diagnosis(f)}


@app.post("/predict/risk")
def predict_risk(data: PatientFeatures):
    _require_models()
    f = preprocess(data)
    return {"patient_id": data.patient_id, **_predict_risk(f)}


@app.post("/predict/recommendation")
def predict_recommendation(data: PatientFeatures):
    _require_models()
    f = preprocess(data)
    return {"patient_id": data.patient_id, **_predict_recommendation(f, data.disease_type)}


@app.post("/predict/routing")
def predict_routing(data: PatientFeatures):
    _require_models()
    return {"patient_id": data.patient_id, **_predict_routing(data)}


@app.post("/predict/full")
def predict_full(data: PatientFeatures):
    """Run all 4 models in one call — used by the Rust ml_service.rs."""
    _require_models()
    f = preprocess(data)
    return {
        "patient_id": data.patient_id,
        "diagnosis": _predict_diagnosis(f),
        "risk": _predict_risk(f),
        "recommendation": _predict_recommendation(f, data.disease_type),
        "routing": _predict_routing(data),
    }


@app.post("/export-training-data", dependencies=[Depends(require_retrain_key)])
async def export_training_data(background_tasks: BackgroundTasks):
    """Export patient_training_data table to data/patients_export.csv."""
    def _run_export():
        import subprocess
        result = subprocess.run(
            ["python", "seed_database.py", "--export"],
            capture_output=True, text=True, cwd=os.getcwd(), env={**os.environ}
        )
        if result.returncode == 0:
            print("✓ Export complete")
        else:
            print(f"✗ Export failed:\n{result.stderr}")

    background_tasks.add_task(_run_export)
    return {"status": "export started", "output": "data/patients_export.csv"}


@app.post("/retrain", dependencies=[Depends(require_retrain_key)])
async def retrain(background_tasks: BackgroundTasks):
    """
    Trigger model retraining in the background.
    Called weekly by PipelineScheduler (future cron job).
    In production: export latest Gold data from PostgreSQL, retrain, evaluate,
    swap if better. For now, reruns train_models.py on latest data/patients_training.csv.
    """
    def _run_retrain():
        import subprocess
        env = {**os.environ}
        result = subprocess.run(
            ["python", "train_models.py", "--from-db"],
            capture_output=True, text=True, cwd=os.getcwd(), env=env
        )
        if result.returncode == 0:
            ModelRegistry.load()
            print("✓ Retrain complete — models hot-swapped")
        else:
            print(f"✗ Retrain failed:\n{result.stderr}")

    background_tasks.add_task(_run_retrain)
    return {"status": "retraining started", "note": "Results will be hot-swapped on completion"}

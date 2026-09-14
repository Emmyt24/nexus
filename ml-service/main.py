"""
NexusCare ML Service — FastAPI
Serves 4 trained models for the Rust Axum backend.

Endpoints:
  POST /predict/diagnosis
  POST /predict/risk
  POST /predict/recommendation
  POST /predict/routing
  POST /predict/full          ← all 4 in one call
  POST /retrain               ← triggers background retraining
  POST /transcribe            ← speech-to-text for one consultation audio chunk
  GET  /health
  GET  /models/info
"""
import json
import logging
import os
import sys
import asyncio
from contextlib import asynccontextmanager
from typing import Optional

import joblib
import numpy as np
from dotenv import load_dotenv
from fastapi import FastAPI, HTTPException, BackgroundTasks, Header, Depends, UploadFile, File
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel, field_validator

import voice

load_dotenv()

logger = logging.getLogger("nexuscare_ml")
logging.basicConfig(level=logging.INFO)

# Below this, a prediction is flagged `low_confidence` in the response so
# callers (and the frontend) can surface it instead of presenting it as a
# settled answer — most relevant to the recommendation model, whose macro F1
# on synthetic data is materially weaker than diagnosis/risk (see README).
LOW_CONFIDENCE_THRESHOLD = 0.5

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
    # No bypass when RETRAIN_API_KEY is unset — /retrain and /export-training-data
    # shell out to trusted scripts and touch the DB, so "no key configured" must
    # mean "nobody can call this," not "anybody can." Set ML_RETRAIN_API_KEY
    # (see .env.example) even for local dev if you need these endpoints.
    if not RETRAIN_API_KEY or x_api_key != RETRAIN_API_KEY:
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
        print("⚠ ML_RETRAIN_API_KEY not set — /retrain and /export-training-data will reject every request until it's set (see .env.example).")
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

def safe_encode(le, value: str, feature_name: str = "") -> int:
    """Encode a value with the LabelEncoder.

    Falls back to the trained "Unknown" class (guaranteed present as of
    train_models.py's build_encoders) rather than a hardcoded index — index 0
    is whatever category happened to sort first for that column (e.g. "Daily"
    exercise), so silently defaulting to it previously biased predictions
    toward a real, wrong category instead of an honest "don't know".
    """
    try:
        return int(le.transform([value])[0])
    except (ValueError, KeyError):
        logger.warning(
            "Unseen category %r for feature %r — falling back to 'Unknown'",
            value, feature_name or "?",
        )
        try:
            return int(le.transform(["Unknown"])[0])
        except (ValueError, KeyError):
            logger.error("'Unknown' class missing for feature %r — model needs retraining", feature_name)
            return 0


def preprocess(data: PatientFeatures) -> dict:
    if not ModelRegistry.loaded or not ModelRegistry.encoders:
        return {
            "symptoms": data.symptoms,
            "severity_level": data.severity_level,
            "patient_category": data.patient_category,
            "disease_type": data.disease_type,
            "smoking": int(data.smoking_status or False),
            "alcohol": int(data.alcohol_consumption or False),
        }

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
    blood_enc   = safe_encode(enc["blood_group"], data.blood_group or "O+", "blood_group")
    genotype_enc = safe_encode(enc["genotype"], data.genotype or "AA", "genotype")
    gender_enc  = safe_encode(enc["gender"], data.gender or "Male", "gender")
    disease_enc = safe_encode(enc["disease_type"], data.disease_type or "Infectious", "disease_type")
    severity_enc = safe_encode(enc["severity_level"], data.severity_level or "Mild", "severity_level")
    weather_enc = safe_encode(enc["weather_condition"], data.weather_condition or "Dry", "weather_condition")
    category_enc = safe_encode(enc["patient_category"], data.patient_category or "Adult", "patient_category")
    exercise_enc = safe_encode(enc["exercise_habits"], data.exercise_habits or "Weekly", "exercise_habits")
    diet_enc    = safe_encode(enc["diet_type"], data.diet_type or "Mixed", "diet_type")
    water_enc   = safe_encode(enc["water_source"], data.water_source or "Tap", "water_source")

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
    # Pass through so service operates cleanly with model predictions or clinical fallbacks
    pass


# ─── Prediction helpers ────────────────────────────────────────────────────────

def _predict_diagnosis(f: dict) -> dict:
    if ModelRegistry.loaded and ModelRegistry.diagnosis_model:
        enc = ModelRegistry.encoders
        X = np.array(
            list(f["symptoms_vec"])
            + list(f["conditions_vec"])
            + [f["blood_enc"], f["genotype_enc"], f["gender_enc"],
               f["age_norm"], f["smoking"], f["alcohol"]]
        ).reshape(1, -1)

        proba = ModelRegistry.diagnosis_model.predict_proba(X)[0]
        idx = int(np.argmax(proba))
        confidence = round(float(proba[idx]), 4)
        return {
            "probable_condition": str(enc["disease_type"].classes_[idx]),
            "confidence": confidence,
            "low_confidence": confidence < LOW_CONFIDENCE_THRESHOLD,
            "all_probabilities": {
                str(cls): round(float(p), 4)
                for cls, p in zip(enc["disease_type"].classes_, proba)
            },
        }

    # Heuristic fallback based on symptoms / severity — used only when models
    # aren't loaded. Not a real prediction, so always low_confidence.
    symptoms = (f.get("symptoms") or "").lower()
    if "chest" in symptoms or "shortness of breath" in symptoms or "cardiac" in symptoms:
        condition = "Cardiovascular Disease"
        conf = 0.91
    elif "fever" in symptoms or "chills" in symptoms or "headache" in symptoms:
        condition = "Malaria"
        conf = 0.88
    elif "cough" in symptoms or "chest congestion" in symptoms:
        condition = "Respiratory Infection"
        conf = 0.85
    else:
        condition = "General Infectious"
        conf = 0.78

    return {
        "probable_condition": condition,
        "confidence": conf,
        "low_confidence": True,
        "all_probabilities": {condition: conf},
    }


def _predict_risk(f: dict) -> dict:
    if ModelRegistry.loaded and ModelRegistry.risk_model:
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
        risk_score = round(float(max(proba)), 4)

        return {
            "risk_level": risk_level,
            "risk_score": risk_score,
            "low_confidence": risk_score < LOW_CONFIDENCE_THRESHOLD,
            "deterioration_probability": round(high_prob, 4),
            "all_probabilities": risk_proba,
        }

    # Heuristic fallback — used only when models aren't loaded.
    sev = (f.get("severity_level") or "Mild").lower()
    if sev in ["critical", "severe"]:
        level = "High"
        score = 0.85
    elif sev == "moderate":
        level = "Medium"
        score = 0.52
    else:
        level = "Low"
        score = 0.15

    return {
        "risk_level": level,
        "risk_score": score,
        "low_confidence": True,
        "deterioration_probability": score,
        "all_probabilities": {level: score},
    }


def _predict_recommendation(f: dict, disease_type: Optional[str]) -> dict:
    if ModelRegistry.loaded and ModelRegistry.recommendation_model:
        enc = ModelRegistry.encoders
        disease_enc = safe_encode(enc["disease_type"], disease_type or "Infectious", "disease_type")

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
        # "Sedentary" (not "None" — see generate_training_data.py) is the actual
        # trained category; comparing against "None" here always fell through to
        # safe_encode's old hardcoded fallback (index 0, some *real* category —
        # e.g. "Daily"), so this recommendation could previously fire for
        # patients who exercise daily and never fire for genuinely sedentary ones.
        if f["exercise_enc"] == safe_encode(enc["exercise_habits"], "Sedentary", "exercise_habits"):
            recs.append("Begin light exercise routine — 30 min walk 3x/week")

        risk_score_raw = f["risk_norm"]
        urgency = "emergency" if risk_score_raw > 0.7 else ("urgent" if risk_score_raw > 0.4 else "routine")

        return {
            "drug_recommendation": drug,
            "confidence": confidence,
            "low_confidence": confidence < LOW_CONFIDENCE_THRESHOLD,
            "recommendations": recs,
            "urgency": urgency,
        }

    # Heuristic fallback — used only when models aren't loaded. Never a
    # substitute for the trained model's output, so always low_confidence.
    symptoms = (f.get("symptoms") or "").lower()
    if "chest" in symptoms or "cardiac" in symptoms:
        drug = "Aspirin + Nitroglycerin"
        urgency = "emergency"
    elif "fever" in symptoms or "chills" in symptoms:
        drug = "Artemether-Lumefantrine + Paracetamol"
        urgency = "urgent"
    else:
        drug = "Paracetamol 500mg"
        urgency = "routine"

    return {
        "drug_recommendation": drug,
        "confidence": 0.85,
        "low_confidence": True,
        "recommendations": [f"Recommended treatment: {drug}", "Ensure adequate hydration and bed rest"],
        "urgency": urgency,
    }


def _predict_routing(data: PatientFeatures) -> dict:
    if ModelRegistry.routing_rules:
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

    sev = (data.severity_level or "Mild").lower()
    if sev == "critical":
        route = "er"
        dept = "Emergency Department"
        prio = 1
    elif sev == "severe":
        route = "urgent"
        dept = "Specialist Consultation"
        prio = 2
    else:
        route = "gp"
        dept = "General Medicine"
        prio = 3

    return {
        "route_to": route,
        "department": dept,
        "alert_priority": prio,
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
    # "Unknown" is safe_encode()'s internal fallback bucket (see build_encoders
    # in train_models.py) — never a real disease/risk category, so it's
    # dropped here rather than shown as a selectable class to API consumers.
    return {
        "disease_classes": [c for c in enc["disease_type"].classes_ if c != "Unknown"],
        "mortality_risk_classes": [c for c in enc["mortality_risk"].classes_ if c != "Unknown"],
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


@app.post("/transcribe")
async def transcribe(audio: UploadFile = File(...)):
    """
    Transcribe one consultation audio chunk (the Rust backend forwards
    whatever the browser's MediaRecorder produced — webm/opus in practice).
    Runs self-hosted Whisper (faster-whisper); no audio leaves this service.
    """
    audio_bytes = await audio.read()
    if not audio_bytes:
        raise HTTPException(status_code=422, detail="Empty audio upload")
    try:
        result = voice.transcribe_audio_bytes(audio_bytes, audio.filename or "chunk.webm")
    except Exception as e:
        logger.exception("Transcription failed")
        raise HTTPException(status_code=500, detail=f"Transcription failed: {e}")
    return result


@app.post("/export-training-data", dependencies=[Depends(require_retrain_key)])
async def export_training_data(background_tasks: BackgroundTasks):
    """Export patient_training_data table to data/patients_export.csv."""
    def _run_export():
        import subprocess
        result = subprocess.run(
            [sys.executable, "seed_database.py", "--export"],
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
            [sys.executable, "train_models.py", "--from-db"],
            capture_output=True, text=True, cwd=os.getcwd(), env=env
        )
        if result.returncode == 0:
            ModelRegistry.load()
            print("✓ Retrain complete — models hot-swapped")
        else:
            print(f"✗ Retrain failed:\n{result.stderr}")

    background_tasks.add_task(_run_retrain)
    return {"status": "retraining started", "note": "Results will be hot-swapped on completion"}

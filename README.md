# NexusCare

Hospital management platform with a built-in ML pipeline for patient risk assessment, diagnosis, drug recommendation, and clinical routing.

---

## Stack

| Layer | Tech |
|---|---|
| Backend API | Rust · Axum · SQLx |
| Database | PostgreSQL |
| ML Service | Python · FastAPI · scikit-learn |
| Auth | JWT + Argon2 |
| Payments | Paystack |

---

## How Data Flows

```
POST /api/v1/ingest/patient
        │
        ▼
   [Bronze]  Raw payload written as-is to PostgreSQL + JSON blob
        │
        ▼
   [Silver]  Symptoms/conditions normalised, nulls filled, category inferred
        │
        ▼
   [Gold]    Risk score, mortality risk, outbreak flag computed and saved to DB
        │
        ▼
   [ML]      Enriched patient record → POST /predict/full (Python, port 8001)
             ├─ Model 1: Diagnosis        (GradientBoosting)
             ├─ Model 2: Risk             (RandomForest + SMOTE)
             ├─ Model 3: Drug             (DecisionTree)
             └─ Model 4: Routing          (rule-based JSON)
             └─ Result persisted back to patients table
        │
        ▼
   SSE stream → GET /api/v1/pipeline/events
```

The pipeline runs in a background task — the ingest call returns in <100ms. ML results are pushed to all connected subscribers via Server-Sent Events.

### Training data flow

The ML models are trained from the `patient_training_data` table in PostgreSQL (seeded from the synthetic CSV on first setup). As real patients accumulate, `POST /retrain` re-trains directly from that table and hot-swaps the models with no restart.

```
data/patients_training.csv
        │  (once, via seed_database.py --seed)
        ▼
patient_training_data  ←──── real patients merge in over time
        │  (python train_models.py --from-db)
        ▼
models/*.pkl  (hot-swapped on retrain)
```

Export the table back to CSV at any time:
```bash
python seed_database.py --export          # → data/patients_export.csv
# or via API:
POST /export-training-data
```

### Consuming ML output

```
GET /api/v1/pipeline/events?patient_id=P001&role=nurse
```

Events: `patient:assessment` · `alert:high-risk` · `alert:outbreak` · `pipeline:status` · `pipeline:error`

```json
{
  "patient_id": "P001",
  "diagnosis":       { "probable_condition": "Infectious", "confidence": 0.91 },
  "risk":            { "risk_level": "High", "risk_score": 0.97, "deterioration_probability": 0.82 },
  "recommendation":  { "drug_recommendation": "Amoxicillin 500mg", "urgency": "emergency" },
  "routing":         { "route_to": "emergency", "department": "Infectious Disease", "alert_priority": 1 }
}
```

---

## Quick Start

### 1. Backend
```bash
cp .env.example .env   # fill DATABASE_URL, JWT_SECRET, ML_SERVICE_URL
cargo run              # http://localhost:8080 — Swagger at /api/docs
```

`cargo run` auto-starts ml-service for you (training synthetic models on
first run if none exist) as long as `ML_SERVICE_URL` points at localhost —
see `src/services/ml_service_launcher.rs`. Skip step 2 entirely unless you
need to run ml-service yourself (e.g. under a debugger), in which case set
`ML_SERVICE_AUTOSTART=false` first so the two don't fight over port 8001.

### 2. ML Service (optional — only if not letting `cargo run` handle it)
```bash
cd ml-service
cp .env.example .env   # fill DATABASE_URL
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt

# First time only
python generate_training_data.py   # generate synthetic CSV
python seed_database.py --seed     # load CSV into PostgreSQL
python train_models.py --from-db   # train models from DB

uvicorn main:app --host 0.0.0.0 --port 8001
```

---

## Key Endpoints

| Method | Path | Description |
|---|---|---|
| `POST` | `/api/v1/ingest/patient` | Ingest patient → triggers full pipeline |
| `GET` | `/api/v1/pipeline/events` | SSE stream — real-time ML results |
| `GET` | `/api/v1/patients/{id}/assessment` | Pull latest ML assessment for a patient |
| `POST` | `/api/v1/pipeline/re-assess/{id}` | Re-run pipeline for existing patient |
| `GET` | `/api/v1/ml/health` | ML service health (proxied) |
| `POST` | `/retrain` *(ML service)* | Retrain models from DB, hot-swap |
| `POST` | `/export-training-data` *(ML service)* | Export training table to CSV |
| `POST` | `/api/v1/auth/login` | Login |
| `GET` | `/health` | Backend health |
| `GET` | `/api/v1/patients` | List recent patients for the caller's hospital |
| `POST` | `/api/v1/patients/{patient_id}/consultation-notes` | Start a voice-recorded consultation note |
| `POST` | `/api/v1/consultation-notes/{id}/audio-chunk` | Upload + transcribe one audio chunk (Whisper) |

Voice-recorded consultation notes: `docs/CONSULTATION_NOTES.md`.
Full reference: `docs/COMPLETE_API_DOCUMENTATION.md` · Swagger: `/api/docs`

> Note: some entries in this table (`/api/v1/patients/{id}/assessment`,
> `/api/v1/pipeline/re-assess/{id}`, `/api/v1/ml/health`, `/api/v1/auth/login`)
> predate the routes that actually exist in `src/routes/app_routes.rs` today
> — this table has drifted from the code in places unrelated to this change.
> The rows added above are accurate as of this PR; the rest could use an
> audit against the real router.

---

## Project Structure

```
nexus/
├── src/                  # Rust backend
│   ├── handlers/         # Axum route handlers
│   ├── services/         # Pipeline, ML, auth, billing…
│   ├── repositories/     # SQLx DB queries
│   └── models/           # Domain types
├── ml-service/
│   ├── main.py           # FastAPI — serves 4 models
│   ├── train_models.py   # Train from DB (--from-db) or CSV
│   ├── seed_database.py  # CSV ↔ PostgreSQL seeding & export
│   ├── generate_training_data.py
│   └── models/           # Trained .pkl files
├── migrations/           # SQL migration files
└── docs/                 # Extended documentation
```

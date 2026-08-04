# NexusCare ML Service

FastAPI microservice that trains and serves the 4 clinical decision-support
models used by NexusCare: diagnosis, mortality risk, drug recommendation, and
department routing.

## Models

| Model | Algorithm | Predicts | Notes |
|---|---|---|---|
| Diagnosis | `GradientBoostingClassifier` | `disease_type` (Chronic / Genetic / Infectious / MentalHealth) | Trained on TF-IDF of `symptoms` + `existing_conditions` plus demographics/genotype. |
| Mortality risk | `RandomForestClassifier` + SMOTE | `mortality_risk` (Low / Medium / High) | Class-balanced via SMOTE oversampling of the minority "High" class. |
| Recommendation | `DecisionTreeClassifier` | `drug_recommendation` | Phase 1 only — swap for a data-driven recommender once real prescriptions accumulate. Currently the weakest model (macro F1 ≈ 0.3–0.4). |
| Routing | Rule-based JSON (`models/routing_rules.json`) | department + alert priority | Deterministic disease/severity → department/route matrix, not a trained model. |

All four are exposed together via `POST /predict/full`, or individually via
`POST /predict/{diagnosis,risk,recommendation,routing}`.

## Data

Training data comes from `data/patients_training.csv`, either generated
synthetically (`generate_training_data.py`, 1500 rows) or exported from the
`patient_training_data` Postgres table (`train_models.py --from-db`).

The synthetic generator injects deliberate label noise (`SYMPTOM_NOISE_RATE`,
`CONDITION_NOISE_RATE` in `generate_training_data.py`) so that `symptoms` and
`existing_conditions` aren't a perfect 1:1 lookup for `disease_type` — without
it, the diagnosis model memorizes the vocabulary instead of learning a
pattern (an earlier version of this generator produced a meaningless F1 =
1.00 for exactly this reason).

## Running locally

```bash
pip install -r requirements.txt
python generate_training_data.py   # or: python train_models.py --from-db
python train_models.py
uvicorn main:app --host 0.0.0.0 --port 8001 --reload
```

```bash
curl http://localhost:8001/health
curl -X POST http://localhost:8001/predict/full \
  -H "Content-Type: application/json" \
  -d '{"patient_id":"P999","age":70,"symptoms":"Pyrexia, Cough","existing_conditions":"Hypertension","genotype":"SS","severity_level":"Severe"}'
```

## Running with Docker

```bash
docker build -t nexuscare-ml-service .
docker run -p 8001:8001 \
  -e ML_RETRAIN_API_KEY=$(openssl rand -hex 32) \
  -e ALLOWED_ORIGINS=https://your-admin-dashboard.example.com \
  nexuscare-ml-service
```

The image trains a model set from the bundled synthetic CSV *during the
build*, so the container is servable immediately — no live Postgres
connection required to boot. See **[DEPLOYMENT.md](DEPLOYMENT.md)** for
Fly.io / Railway deploy steps, the full environment variable reference, and
retraining in production.

## Security

- `ALLOWED_ORIGINS` (comma-separated) controls browser CORS access; unset
  blocks all browser origins. Server-to-server calls (the Rust backend) are
  unaffected by CORS either way.
- `ML_RETRAIN_API_KEY` gates `POST /retrain` and `POST /export-training-data`
  via an `X-API-Key` header — both endpoints shell out to trusted scripts and
  touch the database, so they shouldn't be publicly callable. If unset, the
  service logs a startup warning and runs those two endpoints unauthenticated
  (fine for local dev only).

## Known limitations

- Trained on synthetic data until real labeled patient records accumulate in
  `patient_training_data`.
- No probability calibration, drift detection, or explainability endpoint.
- `LabelEncoder` fallback (`safe_encode` in `main.py`) returns `0` for any
  category unseen at training time, which can silently bias predictions if
  real-world data introduces many new categories.
- Recommendation model's accuracy is materially weaker than the other two
  trained models — treat its output as a rough prior, not a suggestion to
  surface directly to clinicians without review.

## Files

| File | Purpose |
|---|---|
| `main.py` | FastAPI app — model registry, preprocessing, prediction endpoints. |
| `train_models.py` | Trains all 4 models and saves artifacts to `models/`. |
| `generate_training_data.py` | Synthetic training data generator. |
| `seed_database.py` | Loads/exports `data/patients_training.csv` ↔ Postgres. |
| `test_against_dataset.py` | Evaluates trained models against a real hospital dataset export. |
| `Dockerfile`, `fly.toml`, `.dockerignore` | Container build and Fly.io deploy config. |
| `DEPLOYMENT.md` | Deployment guide (Fly.io, Railway, env vars, retraining). |

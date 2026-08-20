# Deploying the NexusCare ML Service

FastAPI service that serves 4 models (diagnosis, mortality risk, drug
recommendation, routing) to the Rust backend. Stateless — the only persistent
state is the `models/*.pkl` / `.json` artifacts baked into the image at build
time.

## Image

`Dockerfile` installs `requirements.txt`, then runs `generate_training_data.py`
+ `train_models.py` **during the build**, so the image ships with a trained
model set and boots immediately — no live Postgres connection needed to start
serving. `ModelRegistry.load()` in `main.py` will run with `loaded=False` and
every `/predict/*` call will 503 if these artifacts are ever missing.

Build and run locally:

```bash
docker build -t nexuscare-ml-service .
docker run -p 8001:8001 \
  -e ALLOWED_ORIGINS=https://your-admin-dashboard.example.com \
  -e ML_RETRAIN_API_KEY=$(openssl rand -hex 32) \
  nexuscare-ml-service
curl http://localhost:8001/health
```

## Required environment variables

| Variable | Required | Purpose |
|---|---|---|
| `PORT` | no (default 8001) | Port `uvicorn` binds to. Railway overrides this automatically. |
| `DATABASE_URL` | only for `--from-db` training / `/retrain` / `/export-training-data` | Postgres connection to the `patient_training_data` Gold table. |
| `ALLOWED_ORIGINS` | recommended | Comma-separated browser origins allowed to call the API cross-origin. Leave unset to block all browser origins (server-to-server calls, e.g. from the Rust backend, are unaffected by CORS either way). |
| `ML_RETRAIN_API_KEY` | recommended | Shared secret required in the `X-API-Key` header to call `POST /retrain` and `POST /export-training-data`. If unset, those two endpoints run **unauthenticated** (a startup warning is logged) — fine for local dev, not for a public deployment. Generate with `openssl rand -hex 32`. |

## Deploy to Fly.io

```bash
cd ml-service
fly launch --no-deploy        # detects fly.toml, confirm/create the app name
fly secrets set ML_RETRAIN_API_KEY=$(openssl rand -hex 32)
fly secrets set ALLOWED_ORIGINS=https://your-admin-dashboard.example.com
fly secrets set DATABASE_URL=postgresql://...   # only if using --from-db retrain
fly deploy
```

`fly.toml` runs a health check against `GET /health` and keeps at least one
machine warm (`min_machines_running = 1`) so `/predict/*` never cold-starts
into a 503.

## Deploy to Railway

```bash
cd ml-service
railway init
railway up
```

Railway auto-detects the `Dockerfile` — no extra config file needed. Set the
same variables (`ML_RETRAIN_API_KEY`, `ALLOWED_ORIGINS`, `DATABASE_URL` if
needed) in the Railway dashboard or via `railway variables set KEY=value`.
Railway injects its own `$PORT`, which the image's `CMD` already respects.

## Wiring up the Rust backend

Point the backend's `ML_SERVICE_URL` (already present in the root
`.env.example`) at the deployed URL, e.g. `https://nexuscare-ml-service.fly.dev`.
No API key is required for `/predict/*` — only `/retrain` and
`/export-training-data` are gated, since those are the endpoints that mutate
model state / touch the DB.

## Retraining in production

`POST /retrain` (with `X-API-Key`) always calls `train_models.py --from-db`,
which requires `DATABASE_URL` to be set and reachable. It retrains
synchronously in a background task on the same process/machine serving
traffic — for the current model sizes (~1500–a few thousand rows) this
finishes in seconds, but if the Gold table grows substantially, move retraining
to a separate job/machine rather than the serving container.

## Known limitations to keep in mind before trusting this in production

- Models are trained on synthetic data (`generate_training_data.py`, 1500
  rows) until real labeled patient data accumulates in `patient_training_data`.
- No probability calibration, drift detection, or explainability endpoint yet.
- `LabelEncoder` fallback returns `0` for any category unseen at training time
  (`safe_encode` in `main.py`) — can silently bias predictions if the real
  data introduces many categories the synthetic set never saw.

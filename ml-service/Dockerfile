FROM python:3.11-slim

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir --default-timeout=120 --retries 10 -r requirements.txt

COPY . .

# Bake a trained model set into the image from the bundled synthetic CSV so the
# service is immediately functional on boot, even with no DATABASE_URL / Gold data.
# POST /retrain (which needs DATABASE_URL) can replace these with real-data models later.
RUN python generate_training_data.py && python train_models.py

EXPOSE 8001

CMD ["sh", "-c", "uvicorn main:app --host 0.0.0.0 --port ${PORT:-8001}"]

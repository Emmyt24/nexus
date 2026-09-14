"""
Speech-to-text for consultation recordings — self-hosted Whisper via
faster-whisper, so patient audio never leaves this service to a third-party
API. Model is lazy-loaded on first request (not at service startup) so
`cargo run`'s ml-service auto-start doesn't pay Whisper's load time/memory on
every boot for services that never use voice notes.

Env vars (all optional, see .env.example):
  WHISPER_MODEL_SIZE   tiny|base|small|medium|large-v3 (default: base)
  WHISPER_DEVICE       cpu|cuda (default: cpu)
  WHISPER_COMPUTE_TYPE int8|float16|float32 (default: int8 — fastest on CPU)
"""
import logging
import os
import tempfile
import threading

logger = logging.getLogger("nexuscare_ml.voice")

_model = None
_model_lock = threading.Lock()


def _get_model():
    global _model
    if _model is not None:
        return _model
    with _model_lock:
        if _model is None:
            # huggingface_hub's newer "xet" transfer backend can hang
            # indefinitely (not fail — hang) on networks that allow
            # huggingface.co but block its separate CDN/CAS endpoints (common
            # behind corporate proxies/firewalls). Plain HTTPS download is
            # slower but fails visibly instead of hanging the request thread.
            # Override by setting HF_HUB_DISABLE_XET=0 if xet works for you.
            os.environ.setdefault("HF_HUB_DISABLE_XET", "1")
            from faster_whisper import WhisperModel
            size = os.getenv("WHISPER_MODEL_SIZE", "base")
            device = os.getenv("WHISPER_DEVICE", "cpu")
            compute_type = os.getenv("WHISPER_COMPUTE_TYPE", "int8")
            logger.info(
                "Loading Whisper model (size=%s, device=%s, compute_type=%s) — "
                "first call only, downloads the model on first-ever run",
                size, device, compute_type,
            )
            _model = WhisperModel(size, device=device, compute_type=compute_type)
    return _model


def transcribe_audio_bytes(audio_bytes: bytes, filename_hint: str = "chunk.webm") -> dict:
    """Transcribe one audio chunk. Returns
    {"text": str, "language": str, "language_probability": float, "duration_seconds": float}.

    Raises on decode/model failure — the caller (POST /transcribe) turns that
    into a 500/422 rather than silently returning empty text, so a broken
    chunk is visible in the consultation_transcript_segments row instead of
    being indistinguishable from genuine silence.
    """
    model = _get_model()
    suffix = os.path.splitext(filename_hint)[1] or ".webm"
    with tempfile.NamedTemporaryFile(suffix=suffix) as tmp:
        tmp.write(audio_bytes)
        tmp.flush()
        segments, info = model.transcribe(tmp.name, beam_size=5, vad_filter=True)
        text = " ".join(seg.text.strip() for seg in segments).strip()

    return {
        "text": text,
        "language": info.language,
        "language_probability": round(float(info.language_probability), 4),
        "duration_seconds": round(float(info.duration), 2),
    }

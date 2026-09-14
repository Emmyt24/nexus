//! Auto-starts the local ml-service (FastAPI/uvicorn) alongside `cargo run`,
//! purely as a local-dev convenience so a fresh checkout works with one
//! command instead of two terminals. Only fires when `ML_SERVICE_URL` points
//! at localhost — a deployed environment manages ml-service as its own
//! process/container and points the URL elsewhere, so this stays inert there.
//!
//! Opt out with `ML_SERVICE_AUTOSTART=false` if you're running ml-service
//! yourself (e.g. under a debugger, or with `--reload`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub struct MlServiceHandle {
    child: Option<Child>,
}

impl MlServiceHandle {
    /// Best-effort: any failure along the way just logs a warning and
    /// returns a handle with no child — it never blocks or fails `cargo run`.
    pub async fn maybe_spawn(ml_service_url: &str) -> Self {
        if autostart_disabled() {
            tracing::info!("ML_SERVICE_AUTOSTART=false — not auto-starting ml-service");
            return Self { child: None };
        }

        let Some(port) = localhost_port(ml_service_url) else {
            tracing::info!(
                "ML_SERVICE_URL ({ml_service_url}) isn't localhost — assuming ml-service \
                 is managed separately, not auto-starting"
            );
            return Self { child: None };
        };

        let ml_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ml-service");
        if !ml_dir.is_dir() {
            tracing::warn!(
                "No ml-service/ directory at {} — skipping auto-start",
                ml_dir.display()
            );
            return Self { child: None };
        }

        let Some(python) = venv_python(&ml_dir) else {
            tracing::warn!(
                "ml-service has no .venv at {} — skipping auto-start. Set it up with:\n  \
                 cd ml-service && python3 -m venv .venv && .venv/bin/pip install -r requirements.txt",
                ml_dir.display()
            );
            return Self { child: None };
        };

        if !models_exist(&ml_dir) {
            tracing::info!("ml-service has no trained models yet — training now (first run only)...");
            if let Err(e) = train_models(&python, &ml_dir).await {
                tracing::warn!(
                    "Auto-training ml-service models failed ({e:#}) — skipping auto-start. \
                     Run `cd ml-service && .venv/bin/python train_models.py` manually and restart."
                );
                return Self { child: None };
            }
        }

        match spawn_uvicorn(&python, &ml_dir, port) {
            Ok(child) => {
                tracing::info!("ml-service auto-starting on port {port} (pid {:?})", child.id());
                let handle = Self { child: Some(child) };
                handle.wait_until_ready(ml_service_url).await;
                handle
            }
            Err(e) => {
                tracing::warn!("Failed to spawn ml-service: {e:#}");
                Self { child: None }
            }
        }
    }

    /// Polls /health for a few seconds so the patient-prediction worker's
    /// first requests don't all hit connection-refused during uvicorn's
    /// cold start. Not fatal if it never comes up in time — the worker
    /// retries on its own schedule regardless.
    async fn wait_until_ready(&self, ml_service_url: &str) {
        if self.child.is_none() {
            return;
        }
        let client = reqwest::Client::new();
        let health_url = format!("{}/health", ml_service_url.trim_end_matches('/'));
        for _ in 0..20 {
            if let Ok(resp) = client.get(&health_url).timeout(Duration::from_secs(1)).send().await {
                if resp.status().is_success() {
                    tracing::info!("ml-service is up ({health_url})");
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        tracing::warn!("ml-service didn't respond to {health_url} within 10s — continuing anyway");
    }

    pub async fn shutdown(mut self) {
        if let Some(mut child) = self.child.take() {
            tracing::info!("Stopping auto-started ml-service (pid {:?})", child.id());
            let _ = child.kill().await;
        }
    }
}

fn autostart_disabled() -> bool {
    matches!(
        std::env::var("ML_SERVICE_AUTOSTART").as_deref(),
        Ok("false") | Ok("0") | Ok("no")
    )
}

/// Returns the port if `url` is a localhost/127.0.0.1/0.0.0.0 URL, else None.
/// Deliberately not pulling in the `url` crate for this one check — just
/// strips the scheme and picks apart `host[:port]`.
fn localhost_port(url: &str) -> Option<u16> {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, p.parse().ok()),
        None => (authority, None),
    };
    if matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0") {
        Some(port.unwrap_or(80))
    } else {
        None
    }
}

fn venv_python(ml_dir: &Path) -> Option<PathBuf> {
    let candidate = ml_dir.join(".venv").join("bin").join("python");
    candidate.is_file().then_some(candidate)
}

fn models_exist(ml_dir: &Path) -> bool {
    [
        "diagnosis_model.pkl",
        "risk_model.pkl",
        "recommendation_model.pkl",
        "encoders.pkl",
    ]
    .iter()
    .all(|f| ml_dir.join("models").join(f).is_file())
}

async fn train_models(python: &Path, ml_dir: &Path) -> anyhow::Result<()> {
    if !ml_dir.join("data").join("patients_training.csv").is_file() {
        run_python_script(python, ml_dir, "generate_training_data.py", &[]).await?;
    }
    run_python_script(python, ml_dir, "train_models.py", &[]).await
}

async fn run_python_script(
    python: &Path,
    ml_dir: &Path,
    script: &str,
    args: &[&str],
) -> anyhow::Result<()> {
    let output = Command::new(python)
        .arg(script)
        .args(args)
        .current_dir(ml_dir)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "{script} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn spawn_uvicorn(python: &Path, ml_dir: &Path, port: u16) -> std::io::Result<Child> {
    let mut child = Command::new(python)
        .args([
            "-m",
            "uvicorn",
            "main:app",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .current_dir(ml_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // Forward ml-service's own logs through tracing, prefixed so they're
    // distinguishable from the Rust backend's own log lines.
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(forward_lines(stdout, "ml-service"));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(forward_lines(stderr, "ml-service"));
    }

    Ok(child)
}

async fn forward_lines(reader: impl tokio::io::AsyncRead + Unpin, prefix: &'static str) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!("[{prefix}] {line}");
    }
}

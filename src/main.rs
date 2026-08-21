use anyhow::Context;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use nexuscare_backend::repositories::EmailOutboxRepository;
use nexuscare_backend::routes;
use nexuscare_backend::schedulers::{
    BroadcastScheduler, HandoverAutoApprovalScheduler, OfferExpiryScheduler, PayoutScheduler,
    VideoSessionReconciler,
};
use nexuscare_backend::services::{
    EmailOutboxService, EmailOutboxWorker, NotificationService, PatientPredictionWorker,
};
use nexuscare_backend::utils::AppConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file (prefer project root, fallback to CWD)
    let manifest_env = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
    if let Err(err) = dotenvy::from_path_override(&manifest_env) {
        if let Err(err2) = dotenvy::from_filename_override(".env") {
            tracing::warn!(
                "Failed to load .env from {}: {}; also failed from CWD: {}",
                manifest_env.display(),
                err,
                err2
            );
        }
    }

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nexuscare_backend=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration
    let cfg = AppConfig::from_env().context("Failed to load configuration")?;
    // and don't pin idle connections open (so Neon can autosuspend).
    let mut connect_opts =
        PgConnectOptions::from_str(&cfg.database.url).context("invalid DATABASE_URL")?;
    let host = connect_opts.get_host().to_string();
    if host != "localhost" && host != "127.0.0.1" {
        connect_opts = connect_opts.ssl_mode(PgSslMode::Require);
    }
    let pool = PgPoolOptions::new()
        .max_connections(cfg.database.max_connections)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(connect_opts)
        .await
        .context("Failed to connect to PostgreSQL")?;

    // Run migrations
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("Failed to run database migrations")?;

    tracing::info!("Database migrations applied successfully");

    // Bootstrap the first super admin from env so the admin API has an initial
    // owner; every later admin (including more super admins) is made via the API.
    seed_super_admin(&pool).await?;

    let notification_service = Arc::new(NotificationService::new());
    let email_outbox_repo = Arc::new(EmailOutboxRepository::new(pool.clone()));
    let email_outbox_service = Arc::new(EmailOutboxService::new(
        email_outbox_repo,
        notification_service.clone(),
    ));

    let worker = EmailOutboxWorker::new(email_outbox_service.clone());
    tokio::spawn(worker.run());

    // Build the application router (also returns the AppState so we can
    let (app, state) =
        routes::create_router(pool.clone(), notification_service, email_outbox_service);

    // re-broadcast cadence sweep (STAT every 15 min, Urgent every
    let broadcast_scheduler = BroadcastScheduler::new(state.shift_service.clone());
    tokio::spawn(broadcast_scheduler.run());

    // offer-expiry sweep. Marks offers past their
    let offer_expiry_scheduler = OfferExpiryScheduler::new(state.shift_service.clone());
    tokio::spawn(offer_expiry_scheduler.run());

    // handover auto-approval sweep. Approves handovers
    let handover_scheduler = HandoverAutoApprovalScheduler::new(state.shift_service.clone());
    tokio::spawn(handover_scheduler.run());

    // SafeHaven payout pipeline: pays out approved handovers every minute
    let payout_scheduler = PayoutScheduler::new(state.payout_service.clone());
    tokio::spawn(payout_scheduler.run());

    // Video consultations: recovers lost LiveKit webhooks and, once a handover
    // exists, clocks the worker out. Spawned only here — never in
    // create_router, which is how PatientPredictionWorker ended up running
    // twice.
    let video_reconciler = VideoSessionReconciler::new(state.video_service.clone());
    tokio::spawn(video_reconciler.run());

    // Patient ML pipeline: polls patient_predictions for pending rows, calls
    // ml-service, and broadcasts results over SSE (GET /api/v1/pipeline/events).
    let patient_prediction_worker =
        PatientPredictionWorker::new(state.patient_prediction_service.clone());
    tokio::spawn(patient_prediction_worker.run());

    let addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port)
        .parse()
        .context("Invalid server address")?;

    tracing::info!("NexusCare backend listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Idempotently seed the initial super admin from SUPER_ADMIN_EMAIL and
/// SUPER_ADMIN_PASSWORD. No-op when the env vars are unset; on conflict it
/// promotes the existing user to super_admin and refreshes the password.
async fn seed_super_admin(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    // Both env vars are required; skip seeding when either is missing.
    let (email, password) =
        match (std::env::var("SUPER_ADMIN_EMAIL"), std::env::var("SUPER_ADMIN_PASSWORD")) {
            (Ok(e), Ok(p)) if !e.trim().is_empty() && !p.is_empty() => (e, p),
            _ => {
                tracing::info!("SUPER_ADMIN_EMAIL/PASSWORD not set; skipping super-admin seed");
                return Ok(());
            }
        };

    // Hash with the same argon2 helper used by password login.
    let email = email.trim().to_lowercase();
    let password_hash = nexuscare_backend::services::auth_service::hash_password(&password)
        .map_err(|e| anyhow::anyhow!("failed to hash super-admin password: {e}"))?;

    // Upsert: create the row, or promote/refresh an existing user by email.
    sqlx::query(
        "INSERT INTO users (first_name, last_name, email, role, password_hash, is_active)
         VALUES ('Super', 'Admin', $1, 'super_admin', $2, TRUE)
         ON CONFLICT (email)
         DO UPDATE SET role = 'super_admin', password_hash = $2, is_active = TRUE",
    )
    .bind(&email)
    .bind(&password_hash)
    .execute(pool)
    .await
    .context("Failed to seed super admin")?;

    tracing::info!("Super admin seeded/updated for {}", email);
    Ok(())
}

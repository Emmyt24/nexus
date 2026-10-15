//! Platform-wide configuration a hospital or clinician is allowed to read.
//!
//! This exists because `GET /admin/settings` is gated on
//! `Permission::ManageSettings`, which only `super_admin` holds — so a hospital
//! had no way to discover the platform fee before committing money to a shift.
//! Rather than widen that permission and expose the other eleven settings
//! (broadcast radius, dispute windows, recording caps), this surfaces only the
//! fee.
//!
//! Thin by convention: one service-free read, no authorization beyond the
//! route-level `require_role` guard. There is nothing tenant-specific here —
//! the fee is global, so there is no `hospital_id` to check.

use axum::{extract::State, Json};

use crate::routes::AppState;
use crate::utils::money::Naira;
use crate::services::payout_service::try_read_platform_fee_config;
use crate::utils::errors::AppResult;

/// The platform fee, as a hospital needs to see it before creating a shift.
///
/// Amounts are **naira**, like every other amount this API returns. The ledger
/// keeps kobo; `utils::money::Naira` converts once, here at the boundary.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PlatformFeeResponse {
    /// `flat` or `percent`. Decides which field below applies.
    pub mode: String,
    /// Flat fee in naira. Applies when `mode == "flat"`.
    pub platform_fee_flat_naira: Naira,
    /// Percentage of gross. Applies when `mode == "percent"`.
    pub platform_fee_percent: f64,
    /// Cap in naira on a percentage fee; `null` means uncapped. Ignored in flat
    /// mode.
    pub platform_fee_cap_naira: Option<Naira>,
    /// The fee is deducted from the clinician's pay, not added to the hospital's
    /// bill (FRS BR-F1-42). Stated on the wire so a client does not have to
    /// guess which side bears it.
    pub deducted_from: &'static str,
}

/// GET /api/v1/platform/fee
#[utoipa::path(
    get,
    path = "/api/v1/platform/fee",
    responses(
        (status = 200, description = "Current platform fee configuration", body = PlatformFeeResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Role may not read platform configuration", body = crate::handlers::shifts::ErrorResponse),
        (status = 500, description = "Fee configuration could not be read", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "platform",
    summary = "Read the platform fee",
    description = "The fee the platform retains from a completed shift. All amounts are in \
                   NAIRA, as everywhere in this API. The fee is deducted from the clinician's \
                   payout, so a hospital still escrows the full shift total. \
                   `POST /api/v1/shifts/preview` returns the resolved figure for a specific shift.",
    security(("bearerAuth" = []))
)]
pub async fn get_platform_fee(State(state): State<AppState>) -> AppResult<Json<PlatformFeeResponse>> {
    // The fallible read. A hospital is about to price a shift against this, so
    // answering 200 with a guessed `percent, 10%` because one query failed is
    // worse than answering 500 — the lenient fallback exists for the payout
    // pipeline, which retries.
    let cfg = try_read_platform_fee_config(&state.pool).await?;
    Ok(Json(PlatformFeeResponse {
        mode: cfg.mode.as_str().to_string(),
        platform_fee_flat_naira: Naira::from_kobo(cfg.flat_kobo),
        platform_fee_percent: cfg.percent,
        platform_fee_cap_naira: cfg.cap_kobo.map(Naira::from_kobo),
        deducted_from: "clinician_payout",
    }))
}

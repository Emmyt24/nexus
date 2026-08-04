use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::{extract::Query, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::handlers::registration::{
    ErrorResponse as RegistrationErrorResponse, ListHospitalsQuery,
};
use crate::models::admin::*;
use crate::models::clinician::ClinicianAdminSummary;
use crate::models::registration::RegistrationStatus;
use crate::routes::AppState;
use crate::services::registration_service::HospitalListResponse;
use crate::utils::errors::AppError;
use crate::utils::extract_claims;

/// Parse the `sub` claim (the admin's user id) from the bearer token.
fn admin_user_id(headers: &HeaderMap) -> Result<Uuid, AppError> {
    let claims = extract_claims(headers)?;
    Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid subject in token".into()))
}

// ===========================================================================
// §2 Dashboard metrics
// ===========================================================================

/// GET /api/v1/admin/metrics/dashboard
#[utoipa::path(get, path = "/api/v1/admin/metrics/dashboard", tag = "admin",
    responses((status = 200, description = "Dashboard KPIs", body = DashboardMetrics)))]
pub async fn metrics_dashboard(
    State(state): State<AppState>,
) -> Result<Json<DashboardMetrics>, AppError> {
    Ok(Json(state.admin_service.dashboard().await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct VolumeQuery {
    pub days: Option<i64>,
}

/// GET /api/v1/admin/metrics/shifts/volume
#[utoipa::path(get, path = "/api/v1/admin/metrics/shifts/volume", tag = "admin",
    params(("days" = Option<i64>, Query, description = "Look-back window in days (default 30)")),
    responses((status = 200, description = "Shift volume trend", body = [ShiftVolumePoint])))]
pub async fn metrics_shift_volume(
    State(state): State<AppState>,
    Query(q): Query<VolumeQuery>,
) -> Result<Json<Vec<ShiftVolumePoint>>, AppError> {
    Ok(Json(
        state.admin_service.shift_volume(q.days.unwrap_or(30)).await?,
    ))
}

/// GET /api/v1/admin/metrics/geographic
#[utoipa::path(get, path = "/api/v1/admin/metrics/geographic", tag = "admin",
    responses((status = 200, description = "Geographic distribution", body = [GeoDistributionPoint])))]
pub async fn metrics_geographic(
    State(state): State<AppState>,
) -> Result<Json<Vec<GeoDistributionPoint>>, AppError> {
    Ok(Json(state.admin_service.geographic().await?))
}

/// GET /api/v1/admin/metrics/workers/performance
#[utoipa::path(get, path = "/api/v1/admin/metrics/workers/performance", tag = "admin",
    responses((status = 200, description = "Worker performance", body = WorkerPerformance)))]
pub async fn metrics_worker_performance(
    State(state): State<AppState>,
) -> Result<Json<WorkerPerformance>, AppError> {
    Ok(Json(state.admin_service.worker_performance().await?))
}

/// GET /api/v1/admin/metrics/revenue
#[utoipa::path(get, path = "/api/v1/admin/metrics/revenue", tag = "admin",
    responses((status = 200, description = "Revenue breakdown", body = RevenueBreakdown)))]
pub async fn metrics_revenue(
    State(state): State<AppState>,
) -> Result<Json<RevenueBreakdown>, AppError> {
    Ok(Json(state.admin_service.revenue().await?))
}

/// GET /api/v1/admin/metrics/ai/usage
#[utoipa::path(get, path = "/api/v1/admin/metrics/ai/usage", tag = "admin",
    responses((status = 200, description = "AI usage metrics", body = AiUsageMetrics)))]
pub async fn metrics_ai_usage(
    State(state): State<AppState>,
) -> Result<Json<AiUsageMetrics>, AppError> {
    Ok(Json(state.admin_service.ai_usage().await?))
}

// ===========================================================================
// §5.3 Pending verifications
// ===========================================================================

/// GET /api/v1/admin/hospitals/pending
#[utoipa::path(get, path = "/api/v1/admin/hospitals/pending", tag = "admin",
    params(("page" = Option<i64>, Query, description = "Page (default 1)"),
           ("page_size" = Option<i64>, Query, description = "Page size (default 20)")),
    responses((status = 200, description = "Pending hospital verifications", body = HospitalListResponse)))]
pub async fn hospitals_pending(
    State(state): State<AppState>,
    Query(params): Query<ListHospitalsQuery>,
) -> Result<Json<HospitalListResponse>, AppError> {
    let page = params.page.unwrap_or(1);
    let page_size = params.page_size.unwrap_or(20);
    let resp = state
        .registration_service
        .list_hospitals(Some(RegistrationStatus::Pending), page, page_size)
        .await
        .map_err(|e| AppError::InternalServerError(e.to_string()))?;
    Ok(Json(resp))
}

/// GET /api/v1/admin/workers/pending
#[utoipa::path(get, path = "/api/v1/admin/workers/pending", tag = "admin",
    params(("page" = Option<i64>, Query, description = "Page (default 1)"),
           ("page_size" = Option<i64>, Query, description = "Page size (default 20)")),
    responses((status = 200, description = "Pending worker verifications", body = ClinicianListResponse)))]
pub async fn workers_pending(
    State(state): State<AppState>,
    Query(params): Query<ListCliniciansQuery>,
) -> Result<Json<ClinicianListResponse>, AppError> {
    // Reuse the completed-clinician listing; pending-identity filtering can be
    // layered on once identity status is surfaced on the clinician summary.
    let page = params.page.unwrap_or(1).max(1);
    let page_size = params.page_size.unwrap_or(20).clamp(1, 100);
    let offset = (page - 1) * page_size;

    let clinicians = state
        .clinician_repo
        .list_completed_clinicians(page_size, offset)
        .await
        .map_err(|e| AppError::InternalServerError(e.to_string()))?;
    let total = state
        .clinician_repo
        .count_completed_clinicians()
        .await
        .map_err(|e| AppError::InternalServerError(e.to_string()))?;
    let total_pages = (total as f64 / page_size as f64).ceil() as i64;

    Ok(Json(ClinicianListResponse {
        clinicians,
        pagination: PaginationMetadata {
            current_page: page,
            page_size,
            total_items: total,
            total_pages,
            has_next: page < total_pages,
            has_previous: page > 1,
        },
    }))
}

// ===========================================================================
// §4.3 Disputes
// ===========================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct ListDisputesQuery {
    pub status: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// GET /api/v1/admin/disputes
#[utoipa::path(get, path = "/api/v1/admin/disputes", tag = "admin",
    params(("status" = Option<String>, Query, description = "open | resolved | closed"),
           ("page" = Option<i64>, Query, description = "Page (default 1)"),
           ("page_size" = Option<i64>, Query, description = "Page size (default 20)")),
    responses((status = 200, description = "Disputes", body = [Dispute])))]
pub async fn list_disputes(
    State(state): State<AppState>,
    Query(q): Query<ListDisputesQuery>,
) -> Result<Json<Vec<Dispute>>, AppError> {
    let disputes = state
        .admin_service
        .list_disputes(q.status, q.page.unwrap_or(1), q.page_size.unwrap_or(20))
        .await?;
    Ok(Json(disputes))
}

/// POST /api/v1/admin/disputes/{id}/resolve
#[utoipa::path(post, path = "/api/v1/admin/disputes/{id}/resolve", tag = "admin",
    params(("id" = Uuid, Path, description = "Dispute id")),
    request_body = ResolveDisputeRequest,
    responses((status = 200, description = "Resolved dispute", body = Dispute),
              (status = 404, description = "Dispute not found")))]
pub async fn resolve_dispute(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<ResolveDisputeRequest>,
) -> Result<Json<Dispute>, AppError> {
    let admin_id = admin_user_id(&headers)?;
    let dispute = state.admin_service.resolve_dispute(id, req, admin_id).await?;
    Ok(Json(dispute))
}

// ===========================================================================
// §5.6 Payments
// ===========================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct FailedPaymentsQuery {
    pub limit: Option<i64>,
}

/// GET /api/v1/admin/payments/failed
#[utoipa::path(get, path = "/api/v1/admin/payments/failed", tag = "admin",
    params(("limit" = Option<i64>, Query, description = "Max rows (default 50)")),
    responses((status = 200, description = "Failed payments", body = [FailedPayment])))]
pub async fn payments_failed(
    State(state): State<AppState>,
    Query(q): Query<FailedPaymentsQuery>,
) -> Result<Json<Vec<FailedPayment>>, AppError> {
    Ok(Json(
        state.admin_service.failed_payments(q.limit.unwrap_or(50)).await?,
    ))
}

// ===========================================================================
// §7 Reports
// ===========================================================================

/// POST /api/v1/admin/reports/generate
#[utoipa::path(post, path = "/api/v1/admin/reports/generate", tag = "admin",
    request_body = GenerateReportRequest,
    responses((status = 200, description = "Report queued", body = GenerateReportResponse)))]
pub async fn reports_generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<GenerateReportRequest>,
) -> Result<Json<GenerateReportResponse>, AppError> {
    // The financial report is restricted to super/finance beyond the route guard.
    let role = extract_claims(&headers)?.role;
    Ok(Json(state.admin_service.generate_report(req, role).await?))
}

// ===========================================================================
// §8 Settings
// ===========================================================================

/// GET /api/v1/admin/settings
#[utoipa::path(get, path = "/api/v1/admin/settings", tag = "admin",
    responses((status = 200, description = "Platform settings", body = PlatformSettings)))]
pub async fn get_settings(
    State(state): State<AppState>,
) -> Result<Json<PlatformSettings>, AppError> {
    Ok(Json(state.admin_service.get_settings().await?))
}

/// PUT /api/v1/admin/settings
#[utoipa::path(put, path = "/api/v1/admin/settings", tag = "admin",
    request_body = UpdatePlatformSettings,
    responses((status = 200, description = "Updated settings", body = PlatformSettings)))]
pub async fn update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdatePlatformSettings>,
) -> Result<Json<PlatformSettings>, AppError> {
    let admin_id = admin_user_id(&headers)?;
    Ok(Json(state.admin_service.update_settings(req, admin_id).await?))
}

// ===========================================================================
// §1.2 Hospital suspend / unsuspend
// ===========================================================================

/// POST /api/v1/admin/hospitals/{id}/suspend
#[utoipa::path(post, path = "/api/v1/admin/hospitals/{id}/suspend", tag = "admin",
    params(("id" = Uuid, Path, description = "Hospital id")),
    request_body = ReasonRequest,
    responses((status = 200, description = "Suspended", body = AdminActionResponse),
              (status = 404, description = "Hospital not found")))]
pub async fn suspend_hospital(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<ReasonRequest>,
) -> Result<Json<AdminActionResponse>, AppError> {
    let admin_id = admin_user_id(&headers)?;
    Ok(Json(
        state.admin_service.suspend_hospital(id, admin_id, req.reason).await?,
    ))
}

/// POST /api/v1/admin/hospitals/{id}/unsuspend
#[utoipa::path(post, path = "/api/v1/admin/hospitals/{id}/unsuspend", tag = "admin",
    params(("id" = Uuid, Path, description = "Hospital id")),
    responses((status = 200, description = "Reinstated", body = AdminActionResponse),
              (status = 404, description = "Hospital not found")))]
pub async fn unsuspend_hospital(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AdminActionResponse>, AppError> {
    Ok(Json(state.admin_service.unsuspend_hospital(id).await?))
}

// ===========================================================================
// §2 Worker verify / reject / suspend
// ===========================================================================

/// POST /api/v1/admin/workers/{id}/verify
#[utoipa::path(post, path = "/api/v1/admin/workers/{id}/verify", tag = "admin",
    params(("id" = Uuid, Path, description = "Clinician id")),
    request_body = VerifyWorkerRequest,
    responses((status = 200, description = "Verified", body = AdminActionResponse),
              (status = 404, description = "Worker not found")))]
pub async fn verify_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<VerifyWorkerRequest>,
) -> Result<Json<AdminActionResponse>, AppError> {
    let admin_id = admin_user_id(&headers)?;
    Ok(Json(
        state.admin_service.set_worker_verified(id, true, admin_id, req.notes).await?,
    ))
}

/// POST /api/v1/admin/workers/{id}/reject
#[utoipa::path(post, path = "/api/v1/admin/workers/{id}/reject", tag = "admin",
    params(("id" = Uuid, Path, description = "Clinician id")),
    request_body = ReasonRequest,
    responses((status = 200, description = "Rejected", body = AdminActionResponse),
              (status = 404, description = "Worker not found")))]
pub async fn reject_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<ReasonRequest>,
) -> Result<Json<AdminActionResponse>, AppError> {
    let admin_id = admin_user_id(&headers)?;
    Ok(Json(
        state.admin_service.set_worker_verified(id, false, admin_id, req.reason).await?,
    ))
}

/// POST /api/v1/admin/workers/{id}/suspend
#[utoipa::path(post, path = "/api/v1/admin/workers/{id}/suspend", tag = "admin",
    params(("id" = Uuid, Path, description = "Clinician id")),
    request_body = ReasonRequest,
    responses((status = 200, description = "Suspended", body = AdminActionResponse),
              (status = 404, description = "Worker not found")))]
pub async fn suspend_worker(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ReasonRequest>,
) -> Result<Json<AdminActionResponse>, AppError> {
    Ok(Json(
        state.admin_service.set_worker_active(id, false, req.reason).await?,
    ))
}

/// POST /api/v1/admin/workers/{id}/unsuspend
#[utoipa::path(post, path = "/api/v1/admin/workers/{id}/unsuspend", tag = "admin",
    params(("id" = Uuid, Path, description = "Clinician id")),
    responses((status = 200, description = "Reinstated", body = AdminActionResponse),
              (status = 404, description = "Worker not found")))]
pub async fn unsuspend_worker(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AdminActionResponse>, AppError> {
    Ok(Json(state.admin_service.set_worker_active(id, true, None).await?))
}

// ===========================================================================
// §3 Platform-wide shifts
// ===========================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct AdminShiftsQuery {
    pub status: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// GET /api/v1/admin/shifts
#[utoipa::path(get, path = "/api/v1/admin/shifts", tag = "admin",
    params(("status" = Option<String>, Query, description = "Filter by shift status"),
           ("page" = Option<i64>, Query, description = "Page (default 1)"),
           ("page_size" = Option<i64>, Query, description = "Page size (default 20)")),
    responses((status = 200, description = "Shifts", body = [AdminShiftRow])))]
pub async fn list_admin_shifts(
    State(state): State<AppState>,
    Query(q): Query<AdminShiftsQuery>,
) -> Result<Json<Vec<AdminShiftRow>>, AppError> {
    let shifts = state
        .admin_service
        .list_shifts(q.status, q.page.unwrap_or(1), q.page_size.unwrap_or(20))
        .await?;
    Ok(Json(shifts))
}

/// POST /api/v1/admin/shifts/{id}/cancel
#[utoipa::path(post, path = "/api/v1/admin/shifts/{id}/cancel", tag = "admin",
    params(("id" = Uuid, Path, description = "Shift id")),
    responses((status = 200, description = "Cancelled", body = AdminActionResponse),
              (status = 404, description = "Shift not found")))]
pub async fn cancel_admin_shift(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AdminActionResponse>, AppError> {
    Ok(Json(state.admin_service.cancel_shift(id).await?))
}

// ===========================================================================
// §5.6 Manual payout
// ===========================================================================

/// POST /api/v1/admin/payments/manual
#[utoipa::path(post, path = "/api/v1/admin/payments/manual", tag = "admin",
    request_body = ManualPayoutRequest,
    responses((status = 200, description = "Payout processed", body = AdminActionResponse),
              (status = 404, description = "No payable shift found")))]
pub async fn manual_payout(
    State(state): State<AppState>,
    Json(req): Json<ManualPayoutRequest>,
) -> Result<Json<AdminActionResponse>, AppError> {
    // Reuse the payout retry path to (re)process this shift's payout.
    let processed = state
        .payout_service
        .retry_payout(req.shift_id)
        .await
        .map_err(|e| AppError::InternalServerError(e.to_string()))?;
    if !processed {
        return Err(AppError::NotFound(
            "No pending or failed payout found for this shift".into(),
        ));
    }
    Ok(Json(AdminActionResponse {
        message: "Payout processed".into(),
    }))
}

// ===========================================================================
// §1 Admin management (super admin only)
// ===========================================================================

/// POST /api/v1/admin/admins
#[utoipa::path(post, path = "/api/v1/admin/admins", tag = "admin",
    request_body = CreateAdminRequest,
    responses((status = 200, description = "Admin created", body = AdminSummary),
              (status = 400, description = "Invalid role")))]
pub async fn create_admin(
    State(state): State<AppState>,
    Json(req): Json<CreateAdminRequest>,
) -> Result<Json<AdminSummary>, AppError> {
    Ok(Json(state.admin_service.create_admin(req).await?))
}

/// GET /api/v1/admin/admins
#[utoipa::path(get, path = "/api/v1/admin/admins", tag = "admin",
    responses((status = 200, description = "Admin users", body = [AdminSummary])))]
pub async fn list_admins(
    State(state): State<AppState>,
) -> Result<Json<Vec<AdminSummary>>, AppError> {
    Ok(Json(state.admin_service.list_admins().await?))
}

/// PATCH /api/v1/admin/admins/{id}
#[utoipa::path(patch, path = "/api/v1/admin/admins/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Admin user id")),
    request_body = UpdateAdminRequest,
    responses((status = 200, description = "Admin updated", body = AdminSummary),
              (status = 404, description = "Admin not found")))]
pub async fn update_admin(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateAdminRequest>,
) -> Result<Json<AdminSummary>, AppError> {
    Ok(Json(state.admin_service.update_admin(id, req).await?))
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ClinicianListResponse {
    pub clinicians: Vec<ClinicianAdminSummary>,
    pub pagination: PaginationMetadata,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PaginationMetadata {
    pub current_page: i64,
    pub page_size: i64,
    pub total_items: i64,
    pub total_pages: i64,
    pub has_next: bool,
    pub has_previous: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ListCliniciansQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// GET /api/v1/admin/hospitals
#[utoipa::path(
    get,
    path = "/api/v1/admin/hospitals",
    tag = "admin",
    params(
        ("status" = Option<String>, Query, description = "Filter by status: pending, approved, rejected"),
        ("page" = Option<i64>, Query, description = "Page number (default: 1)"),
        ("page_size" = Option<i64>, Query, description = "Items per page (default: 20, max: 100)")
    ),
    responses(
        (status = 200, description = "Hospitals retrieved successfully", body = HospitalListResponse),
        (status = 400, description = "Invalid status", body = RegistrationErrorResponse),
        (status = 500, description = "Internal server error", body = RegistrationErrorResponse)
    )
)]
pub async fn list_hospitals_admin(
    State(state): State<AppState>,
    Query(params): Query<ListHospitalsQuery>,
) -> Result<Json<HospitalListResponse>, (StatusCode, Json<RegistrationErrorResponse>)> {
    let status_filter = if let Some(status_str) = params.status {
        match status_str.to_lowercase().as_str() {
            "pending" => Some(RegistrationStatus::Pending),
            "approved" => Some(RegistrationStatus::Approved),
            "rejected" => Some(RegistrationStatus::Rejected),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(RegistrationErrorResponse {
                        code: "INVALID_STATUS".to_string(),
                        message: "Status must be one of: pending, approved, rejected".to_string(),
                    }),
                ));
            }
        }
    } else {
        None
    };

    let page = params.page.unwrap_or(1);
    let page_size = params.page_size.unwrap_or(20);

    match state
        .registration_service
        .list_hospitals(status_filter, page, page_size)
        .await
    {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(RegistrationErrorResponse {
                code: "ERROR".to_string(),
                message: e.to_string(),
            }),
        )),
    }
}

/// GET /api/v1/admin/clinicians
#[utoipa::path(
    get,
    path = "/api/v1/admin/clinicians",
    tag = "admin",
    params(
        ("page" = Option<i64>, Query, description = "Page number (default: 1)"),
        ("page_size" = Option<i64>, Query, description = "Items per page (default: 20, max: 100)")
    ),
    responses(
        (status = 200, description = "Clinicians retrieved successfully", body = ClinicianListResponse),
        (status = 500, description = "Internal server error", body = RegistrationErrorResponse)
    )
)]
pub async fn list_clinicians_admin(
    State(state): State<AppState>,
    Query(params): Query<ListCliniciansQuery>,
) -> Result<Json<ClinicianListResponse>, (StatusCode, Json<RegistrationErrorResponse>)> {
    let page = params.page.unwrap_or(1).max(1);
    let page_size = params.page_size.unwrap_or(20).clamp(1, 100);
    let offset = (page - 1) * page_size;

    let clinicians = state
        .clinician_repo
        .list_completed_clinicians(page_size, offset)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(RegistrationErrorResponse {
                    code: "ERROR".to_string(),
                    message: e.to_string(),
                }),
            )
        })?;

    let total = state
        .clinician_repo
        .count_completed_clinicians()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(RegistrationErrorResponse {
                    code: "ERROR".to_string(),
                    message: e.to_string(),
                }),
            )
        })?;

    let total_pages = (total as f64 / page_size as f64).ceil() as i64;

    Ok(Json(ClinicianListResponse {
        clinicians,
        pagination: PaginationMetadata {
            current_page: page,
            page_size,
            total_items: total,
            total_pages,
            has_next: page < total_pages,
            has_previous: page > 1,
        },
    }))
}

//! Admin dashboard DTOs — metrics, disputes, settings, payments.
//! Backs the Admin Section (§11) endpoints under `/api/v1/admin/*`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// §2 Dashboard KPI metrics
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DashboardMetrics {
    pub total_hospitals: i64,
    pub active_workers: i64,
    pub active_shifts: i64,
    pub shifts_completed_30d: i64,
    /// Platform fee revenue over the last 30 days, in kobo.
    pub platform_revenue_30d_kobo: i64,
    pub active_disputes: i64,
    pub pending_hospital_verifications: i64,
    pub pending_worker_verifications: i64,
    pub failed_payments: i64,
}

// ---------------------------------------------------------------------------
// §3.1 Shift volume trend
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct ShiftVolumePoint {
    /// Day bucket (UTC midnight).
    pub day: DateTime<Utc>,
    pub created: i64,
    pub filled: i64,
    pub completed: i64,
}

// ---------------------------------------------------------------------------
// §3.2 Geographic distribution
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct GeoDistributionPoint {
    pub hospital_id: Uuid,
    pub hospital_name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub shifts_30d: i64,
}

// ---------------------------------------------------------------------------
// §3.3 Worker performance / rating distribution
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct RatingBucket {
    /// Star value, 1..=5.
    pub score: i16,
    pub worker_count: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct WorkerPerformance {
    pub distribution: Vec<RatingBucket>,
    pub top_performers: Vec<TopPerformer>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct TopPerformer {
    pub worker_id: Uuid,
    pub full_name: Option<String>,
    pub shifts_completed: i64,
    pub avg_rating: Option<f64>,
    pub earnings_kobo: i64,
}

// ---------------------------------------------------------------------------
// §3.4 Revenue breakdown
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct RevenueSlice {
    pub label: String,
    pub revenue_kobo: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RevenueBreakdown {
    pub total_revenue_kobo: i64,
    pub by_priority: Vec<RevenueSlice>,
    pub by_status: Vec<RevenueSlice>,
}

// ---------------------------------------------------------------------------
// §3.5 AI usage metrics (no backing table yet — returns zeros until Feature 2
// note storage lands; kept in the surface so the frontend can bind to it).
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AiUsageMetrics {
    pub total_recordings: i64,
    pub total_notes_generated: i64,
    pub avg_note_generation_seconds: f64,
    pub translation_accuracy_percent: f64,
    pub language_breakdown: Vec<LanguageCount>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct LanguageCount {
    pub language: String,
    pub recordings: i64,
}

// ---------------------------------------------------------------------------
// §5.6 Failed payments
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct FailedPayment {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub hospital_name: Option<String>,
    pub amount_kobo: i64,
    pub currency: String,
    pub provider_reference: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// §4.3 / §5.4 Disputes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct Dispute {
    pub id: Uuid,
    pub shift_id: Uuid,
    pub hospital_id: Uuid,
    pub hospital_name: Option<String>,
    pub worker_id: Option<Uuid>,
    pub filed_by: String,
    pub reason: String,
    pub status: String,
    pub priority: String,
    pub amount_kobo: Option<i64>,
    pub resolution: Option<String>,
    pub resolution_amount_kobo: Option<i64>,
    pub admin_notes: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ResolveDisputeRequest {
    /// One of: full_payment, partial_refund, no_payment, escalate.
    pub resolution: String,
    /// Required when resolution = partial_refund (kobo).
    pub resolution_amount_kobo: Option<i64>,
    pub admin_notes: Option<String>,
}

// ---------------------------------------------------------------------------
// §8 Platform settings
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct PlatformSettings {
    pub platform_fee_percent: f64,
    pub worker_broadcast_radius_km: f64,
    pub stat_bonus_percent: f64,
    pub urgent_bonus_percent: f64,
    pub clock_in_grace_minutes: i32,
    pub auto_clock_out_hours: i32,
    pub handover_edit_window_hours: i32,
    pub dispute_filing_window_hours: i32,
    pub max_active_shifts_per_hospital: i32,
    pub min_hourly_rate_kobo: i64,
    pub max_recording_minutes: i32,
    pub updated_at: DateTime<Utc>,
}

/// Partial update — every field optional; only provided fields are changed.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePlatformSettings {
    pub platform_fee_percent: Option<f64>,
    pub worker_broadcast_radius_km: Option<f64>,
    pub stat_bonus_percent: Option<f64>,
    pub urgent_bonus_percent: Option<f64>,
    pub clock_in_grace_minutes: Option<i32>,
    pub auto_clock_out_hours: Option<i32>,
    pub handover_edit_window_hours: Option<i32>,
    pub dispute_filing_window_hours: Option<i32>,
    pub max_active_shifts_per_hospital: Option<i32>,
    pub min_hourly_rate_kobo: Option<i64>,
    pub max_recording_minutes: Option<i32>,
}

// ---------------------------------------------------------------------------
// §7 Reports
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct GenerateReportRequest {
    /// e.g. platform_performance, financial, worker_performance.
    pub report_type: String,
    /// e.g. last_7_days, last_30_days, last_90_days.
    pub date_range: Option<String>,
    /// pdf, csv, excel.
    pub format: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GenerateReportResponse {
    pub report_id: Uuid,
    pub report_type: String,
    pub status: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// §1.2 Suspend / verify actions (hospitals + workers)
// ---------------------------------------------------------------------------

/// Reason body for suspend and worker-reject actions.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReasonRequest {
    pub reason: Option<String>,
}

/// Optional notes captured when an admin verifies a worker's license.
#[derive(Debug, Deserialize, ToSchema)]
pub struct VerifyWorkerRequest {
    pub notes: Option<String>,
}

/// Generic acknowledgement for admin mutations that don't return an entity.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminActionResponse {
    pub message: String,
}

// ---------------------------------------------------------------------------
// §3 Admin platform-wide shifts
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct AdminShiftRow {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub hospital_name: Option<String>,
    pub status: String,
    pub priority: String,
    pub scheduled_start: DateTime<Utc>,
    pub scheduled_end: DateTime<Utc>,
    pub assigned_clinician_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// §5.6 Manual payout
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct ManualPayoutRequest {
    /// Shift whose pending/failed payout should be (re)processed.
    pub shift_id: Uuid,
}

// ---------------------------------------------------------------------------
// §1 Admin management (super admin only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAdminRequest {
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub phone: Option<String>,
    /// One of: operations_admin, verification_admin, finance_admin, super_admin.
    pub role: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateAdminRequest {
    pub role: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, FromRow)]
pub struct AdminSummary {
    pub id: Uuid,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub role: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

// ! Worker earnings endpoint (, ).

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::routes::AppState;
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EarningsSummary {
    pub total_earned_kobo: i64,
    pub this_month_kobo: i64,
    pub pending_kobo: i64,
    pub transactions: Vec<EarningsTransaction>,
    pub total_transactions: i64,
    pub page: i64,
    pub page_size: i64,
}

#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct EarningsTransaction {
    pub id: Uuid,
    pub shift_id: Option<Uuid>,
    pub amount_kobo: i64,
    pub status: String,
    pub hospital_name: Option<String>,
    pub role_title: Option<String>,
    pub scheduled_start: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct EarningsQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// GET /api/v1/worker/earnings
#[utoipa::path(
    get,
    path = "/api/v1/worker/earnings",
    params(EarningsQuery),
    responses(
        (status = 200, description = "Earnings summary", body = EarningsSummary),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse)
    ),
    tag = "earnings",
    summary = "Worker earnings (totals + paginated transactions)",
    description = "Matches FRS §3.8.7. Returns total earned, this month, pending, and a paginated list of billing_transactions joined to the relevant shift and hospital."
)]
pub async fn get_earnings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EarningsQuery>,
) -> AppResult<Json<EarningsSummary>> {
    let claims = extract_claims(&headers)?;
    let user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    // Resolve clinician_id from the authenticated user.
    let clinician_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM clinicians WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::Database)?;
    let clinician_id = clinician_id
        .ok_or_else(|| AppError::Forbidden("Caller has no clinician profile".to_string()))?;

    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(25).clamp(1, 100);
    let offset = (page - 1) * page_size;

    // Totals — use SUM with FILTER for the conditional aggregations. Note
    let now = Utc::now();
    let month_start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(now);

    let totals_row: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COALESCE(SUM(bt.amount_kobo) FILTER (WHERE bt.status = 'success'), 0)::BIGINT
                AS total_earned,
            COALESCE(SUM(bt.amount_kobo) FILTER (WHERE bt.status = 'success'
                                                   AND bt.completed_at >= $2), 0)::BIGINT
                AS this_month,
            COALESCE(SUM(bt.amount_kobo) FILTER (WHERE bt.status = 'pending'), 0)::BIGINT
                AS pending
        FROM billing_transactions bt
        JOIN shifts s ON s.id = bt.shift_id
        WHERE bt.event_type = 'payout'
          AND s.assigned_clinician_id = $1
        "#,
    )
    .bind(clinician_id)
    .bind(month_start)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let total_earned_kobo = totals_row.0;
    let this_month_kobo = totals_row.1;
    let pending_kobo = totals_row.2;

    // Transaction history (newest first).
    let transactions = sqlx::query_as::<_, EarningsTransaction>(
        r#"
        SELECT bt.id,
               bt.shift_id,
               bt.amount_kobo,
               bt.status::text   AS status,
               h.name            AS hospital_name,
               s.role_title      AS role_title,
               s.scheduled_start AS scheduled_start,
               bt.completed_at,
               bt.created_at
        FROM billing_transactions bt
        LEFT JOIN shifts s    ON s.id = bt.shift_id
        LEFT JOIN hospitals h ON h.id = s.hospital_id
        WHERE bt.event_type = 'payout'
          AND s.assigned_clinician_id = $1
        ORDER BY bt.created_at DESC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(clinician_id)
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let total_transactions: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM billing_transactions bt
        LEFT JOIN shifts s ON s.id = bt.shift_id
        WHERE bt.event_type = 'payout'
          AND s.assigned_clinician_id = $1
        "#,
    )
    .bind(clinician_id)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;

    Ok(Json(EarningsSummary {
        total_earned_kobo,
        this_month_kobo,
        pending_kobo,
        transactions,
        total_transactions,
        page,
        page_size,
    }))
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

//! HTTP surface for the hospital-scoped search.
//!
//! Thin by convention: validate, extract the claims, make one service call, map
//! the error. The tenant is resolved from `Claims` inside `SearchService` —
//! there is deliberately no `hospital_id` parameter to pass, because on a search
//! endpoint that argument would be a cross-tenant read with a legitimate role
//! attached, which the route guard cannot detect.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};

use crate::models::search::HospitalSearchResults;
use crate::routes::AppState;
use crate::utils::{errors::AppResult, extract_claims};

/// Query parameters for `GET /api/v1/search`.
#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct HospitalSearchQuery {
    /// What to search for. At least 2 characters; `%` and `_` are matched
    /// literally rather than as wildcards.
    pub q: String,
    /// Comma-separated subset of `shift,clinician,patient,handover`. All four
    /// when omitted. An unrecognised name is a 422, not a silent no-op.
    pub types: Option<String>,
    /// Per-bucket cap, 1-50. Defaults to 20.
    pub limit: Option<i64>,
}

/// GET /api/v1/search
#[utoipa::path(
    get,
    path = "/api/v1/search",
    params(HospitalSearchQuery),
    responses(
        (status = 200, description = "Matches within the caller's hospital", body = HospitalSearchResults),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Role has no hospital to search, or may not search", body = crate::handlers::shifts::ErrorResponse),
        (status = 422, description = "Query too short, or an unknown type", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "search",
    summary = "Search within your hospital",
    description = "Searches shifts, clinicians, patients and handover notes belonging to the \
                   caller's own hospital, grouped by entity. Scoped by the `hospital_id` in the \
                   bearer token, never by a parameter. Clinicians are limited to those with a \
                   working relationship to this hospital (assigned, applied, or expressed \
                   interest) and never include an email or licence number. Patients match on \
                   name only and are returned only to hospital administrators. Platform staff \
                   should use `GET /api/v1/admin/search` instead.",
    security(("bearerAuth" = []))
)]
pub async fn hospital_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HospitalSearchQuery>,
) -> AppResult<Json<HospitalSearchResults>> {
    let claims = extract_claims(&headers)?;
    let results = state
        .search_service
        .search(&claims, &q.q, q.types.as_deref(), q.limit)
        .await?;
    Ok(Json(results))
}

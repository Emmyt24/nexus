use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

use crate::{
    models::shift::{
        AcceptShiftRequest, ClockinApprovalDecisionRequest, ClockinApprovalRequest, ClockinRequest,
        ClockinResponse, ClockoutResponse, CreateShiftRequest, DeclineShiftRequest,
        EditRatingRequest, HandoverResponse, HandoverRevisionRequest, MyApplicationEntry,
        NearbyShiftCard, RankedInterestedClinician, RateHospitalRequest, RateWorkerRequest,
        RatingResponse, Shift, ShiftApplication, ShiftApplicationRequest, ShiftApplicationsQuery,
        ShiftDetailResponse,
        ShiftAssignRequest, ShiftCancelRequest, ShiftInterestRequest, ShiftListQuery,
        ShiftOfferRequest, ShiftOfferResponse, ShiftRescheduleRequest, SubmitHandoverRequest,
    },
    routes::AppState,
    services::shift_service::{self, ShiftServiceError},
    utils::{
        errors::{AppError, AppResult},
        extract_claims,
    },
};

/// Response for shift preview
#[derive(Debug, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct ShiftPreviewResponse {
    pub role_title: String,
    pub specialty: Option<String>,
    pub department: Option<String>,
    pub shift_type: String,
    pub priority: String,
    pub scheduled_start: String,
    pub duration_hours: f32,
    pub base_amount_kobo: i64,
    pub stat_bonus_kobo: i64,
    pub grand_total_kobo: i64,
    pub virtual_link: Option<String>,
    pub estimated_matches: i32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct PaginationMetadata {
    pub current_page: i64,
    pub page_size: i64,
    pub total_items: i64,
    pub total_pages: i64,
    pub has_next: bool,
    pub has_previous: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct ShiftListResponse {
    pub shifts: Vec<Shift>,
    pub pagination: PaginationMetadata,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct ShiftApplicationsResponse {
    pub applications: Vec<ShiftApplication>,
    pub pagination: PaginationMetadata,
}

impl From<shift_service::ShiftPreview> for ShiftPreviewResponse {
    fn from(preview: shift_service::ShiftPreview) -> Self {
        Self {
            role_title: preview.role_title,
            specialty: preview.specialty,
            department: preview.department,
            shift_type: format!("{:?}", preview.shift_type),
            priority: format!("{:?}", preview.priority),
            scheduled_start: preview.scheduled_start.to_rfc3339(),
            duration_hours: preview.duration_hours,
            base_amount_kobo: preview.base_amount_kobo,
            stat_bonus_kobo: preview.stat_bonus_kobo,
            grand_total_kobo: preview.grand_total_kobo,
            virtual_link: preview.virtual_link,
            estimated_matches: preview.estimated_matches,
        }
    }
}

/// POST /api/v1/shifts
#[utoipa::path(
    post,
    path = "/api/v1/shifts",
    request_body = CreateShiftRequest,
    responses(
        (status = 201, description = "Shift created successfully", body = Shift),
        (status = 422, description = "Validation error", body = ErrorResponse),
        (status = 409, description = "Duplicate shift exists", body = ErrorResponse),
        (status = 403, description = "Hospital not approved to create shifts", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Create a new shift",
    description = "Create a new shift posting for hospital staff. Only approved hospitals can create shifts. Validates all required fields, checks for duplicates, generates virtual links for virtual shifts, and broadcasts notifications to eligible workers."
)]
pub async fn create_shift(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateShiftRequest>,
) -> AppResult<(StatusCode, Json<Shift>)> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let claims = extract_claims(&headers)?;
    let created_by = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;
    let hospital_id = claims
        .hospital_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| {
            AppError::Forbidden("No hospital associated with this account".to_string())
        })?;

    match state
        .shift_service
        .create_shift(hospital_id, created_by, payload)
        .await
    {
        Ok(shift) => Ok((StatusCode::CREATED, Json(shift))),
        Err(e) => Err(map_shift_error(e)),
    }
}

/// GET /api/v1/shifts
#[utoipa::path(
    get,
    path = "/api/v1/shifts",
    params(
        ("status" = Option<crate::models::shift::ShiftStatus>, Query, description = "Optional status filter"),
        ("page" = Option<i64>, Query, description = "Page number"),
        ("page_size" = Option<i64>, Query, description = "Page size"),
    ),
    responses(
        (status = 200, description = "Shifts retrieved", body = ShiftListResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "List shifts",
    description = "List shifts with optional status filter and pagination"
)]
pub async fn list_shifts(
    State(state): State<AppState>,
    Query(query): Query<ShiftListQuery>,
) -> AppResult<Json<ShiftListResponse>> {
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(20).clamp(1, 100);

    let (shifts, total) = state
        .shift_service
        .list_shifts(query.status, page, page_size)
        .await
        .map_err(map_shift_error)?;

    let total_pages = (total as f64 / page_size as f64).ceil() as i64;

    Ok(Json(ShiftListResponse {
        shifts,
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

/// POST /api/v1/shifts/preview
#[utoipa::path(
    post,
    path = "/api/v1/shifts/preview",
    request_body = CreateShiftRequest,
    responses(
        (status = 200, description = "Shift preview generated successfully", body = ShiftPreviewResponse),
        (status = 422, description = "Validation error", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Preview a shift before creation",
    description = "Preview how a shift will appear to workers before actually creating it. Shows compensation breakdown, estimated matched workers, and virtual meeting link if applicable."
)]
pub async fn preview_shift(
    State(state): State<AppState>,
    Json(payload): Json<CreateShiftRequest>,
) -> AppResult<Json<ShiftPreviewResponse>> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    match state.shift_service.preview_shift(&payload).await {
        Ok(preview) => Ok(Json(preview.into())),
        Err(e) => Err(map_shift_error(e)),
    }
}

/// GET /api/v1/shifts/{shift_id}
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier")
    ),
    responses(
        (status = 200, description = "Enriched shift details retrieved successfully", body = ShiftDetailResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Get shift details",
    description = "Retrieve the full shift-detail view: base shift fields plus tasks, requirements, hospital rating, and — for in-person shifts — the hospital location. When the caller is a clinician, each requirement is matched against their qualifications."
)]
pub async fn get_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<crate::models::shift::ShiftDetailResponse>> {
    // Best-effort identity: a clinician viewer gets qualification matching;
    // anyone else (or an unauthenticated caller) gets the shift without it.
    let requester_user_id = extract_claims(&headers)
        .ok()
        .and_then(|claims| Uuid::parse_str(&claims.sub).ok());

    match state
        .shift_service
        .get_shift_detail(shift_id, requester_user_id)
        .await
    {
        Ok(detail) => Ok(Json(detail)),
        Err(e) => Err(map_shift_error(e)),
    }
}

/// POST /api/v1/shifts/{shift_id}/interest
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/interest",
    request_body = ShiftInterestRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier")
    ),
    responses(
        (status = 201, description = "Interest recorded"),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Interest already exists, or shift is no longer available", body = ErrorResponse),
        (status = 422, description = "Validation error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Express interest in a shift",
    description = "Clinician expresses interest in an open shift. The hospital admin is notified. Rejected with 409 if the shift is no longer open (BR: no longer available)."
)]
pub async fn express_interest(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    Json(payload): Json<ShiftInterestRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .shift_service
        .express_interest(shift_id, payload.clinician_id)
        .await
        .map(|_| StatusCode::CREATED)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/apply
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/apply",
    request_body = ShiftApplicationRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier")
    ),
    responses(
        (status = 201, description = "Application submitted"),
        (status = 403, description = "Profile incomplete or not allowed", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Already applied or busy", body = ErrorResponse),
        (status = 422, description = "Validation error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Apply for a shift",
    description = "Submit a shift application with profile details and experience"
)]
pub async fn apply_for_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    Json(payload): Json<ShiftApplicationRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .shift_service
        .apply_for_shift(shift_id, payload)
        .await
        .map(|_| StatusCode::CREATED)
        .map_err(map_shift_error)
}

/// GET /api/v1/shifts/{shift_id}/applications
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}/applications",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
        ("page" = Option<i64>, Query, description = "Page number"),
        ("page_size" = Option<i64>, Query, description = "Page size"),
    ),
    responses(
        (status = 200, description = "Applications retrieved", body = ShiftApplicationsResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not authorized", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "List shift applications",
    description = "List applications for a shift (only the shift creator can view)"
)]
pub async fn list_shift_applications(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Query(query): Query<ShiftApplicationsQuery>,
) -> AppResult<Json<ShiftApplicationsResponse>> {
    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(20).clamp(1, 100);

    let (applications, total) = state
        .shift_service
        .list_applications_for_shift(shift_id, requester_user_id, page, page_size)
        .await
        .map_err(map_shift_error)?;

    let total_pages = (total as f64 / page_size as f64).ceil() as i64;

    Ok(Json(ShiftApplicationsResponse {
        applications,
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

/// GET /api/v1/shifts/{shift_id}/interested
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}/interested",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 200, description = "Ranked list of interested clinicians", body = Vec<RankedInterestedClinician>),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not the shift creator", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "List interested workers ranked",
    description = "Return clinicians who have expressed interest in this shift, ranked by FRS §3.4.3 scoring (Distance 30, Rating 25, Experience 20, Acceptance 15, Quals 10). Names are masked to last name only until selection."
)]
pub async fn list_interested_for_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<RankedInterestedClinician>>> {
    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let ranked = state
        .shift_service
        .list_ranked_interested(shift_id, requester_user_id)
        .await
        .map_err(map_shift_error)?;

    Ok(Json(ranked))
}

/// POST /api/v1/shifts/{shift_id}/offer
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/offer",
    request_body = ShiftOfferRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 201, description = "Offer sent", body = ShiftOfferResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not the shift creator", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Clinician did not express interest, shift not open, already offered, or worker already declined this shift", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Send a shift offer to an interested clinician",
    description = "Creates a shift_assignments row with status='offered' and expires_at=now()+30 minutes. Shift remains 'open' until the clinician accepts."
)]
pub async fn offer_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<ShiftOfferRequest>,
) -> AppResult<(StatusCode, Json<ShiftOfferResponse>)> {
    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let (assignment_id, expires_at) = state
        .shift_service
        .offer_shift(shift_id, payload.clinician_id, requester_user_id)
        .await
        .map_err(map_shift_error)?;

    Ok((
        StatusCode::CREATED,
        Json(ShiftOfferResponse {
            assignment_id,
            shift_id,
            clinician_id: payload.clinician_id,
            expires_at,
        }),
    ))
}

/// POST /api/v1/shifts/{shift_id}/accept
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/accept",
    request_body = AcceptShiftRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 204, description = "Offer accepted"),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Caller has no clinician profile", body = ErrorResponse),
        (status = 404, description = "No pending offer for this shift", body = ErrorResponse),
        (status = 409, description = "Offer expired, already responded to, clinician busy, or schedule conflict", body = ErrorResponse),
        (status = 422, description = "NDPR consent missing", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Accept a shift offer",
    description = "Worker accepts a pending shift offer. All 5 NDPR consent booleans must be true. On success: assignment → 'accepted', shift → 'assigned', sibling offers expire."
)]
pub async fn accept_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<AcceptShiftRequest>,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .accept_offer(shift_id, worker_user_id, payload.ndpr_consent)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/decline
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/decline",
    request_body = DeclineShiftRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 204, description = "Offer declined"),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Caller has no clinician profile", body = ErrorResponse),
        (status = 404, description = "No pending offer for this shift", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Decline a shift offer",
    description = "Worker declines a pending shift offer. Shift returns to 'open' so the hospital can offer it to the next ranked candidate."
)]
pub async fn decline_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<DeclineShiftRequest>,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .decline_offer(shift_id, worker_user_id, payload.reason)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/clockin
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/clockin",
    request_body = ClockinRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 201, description = "Clocked in", body = ClockinResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not the assigned clinician", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Outside time window, outside geofence, or wrong status", body = ErrorResponse),
        (status = 422, description = "Validation error (missing GPS, virtual method on in-person shift, etc.)", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Clock in to an assigned shift",
    description = "Worker clocks in. GPS method requires latitude/longitude within the hospital's clock-in radius (default 100m). Virtual method only allowed for virtual shifts. Late-clockin rules per spec §3.6.7."
)]
pub async fn clock_in(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<ClockinRequest>,
) -> AppResult<(StatusCode, Json<ClockinResponse>)> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let response = state
        .shift_service
        .clock_in(shift_id, worker_user_id, payload)
        .await
        .map_err(map_shift_error)?;

    Ok((StatusCode::CREATED, Json(response)))
}

/// POST /api/v1/shifts/{shift_id}/handover
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/handover",
    request_body = SubmitHandoverRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 201, description = "Handover submitted/updated", body = HandoverResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not the assigned clinician", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Wrong shift status or edit window closed", body = ErrorResponse),
        (status = 422, description = "Validation error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Submit (or update within 1h of clock-out) handover documentation",
    description = "Records the F1-H01..H05 handover fields. Editable for 1 hour after clock-out (BR-F1-36). After 48 hours with no hospital action the handover auto-approves (Tier 3)."
)]
pub async fn submit_handover(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<SubmitHandoverRequest>,
) -> AppResult<(StatusCode, Json<HandoverResponse>)> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let row = state
        .shift_service
        .submit_handover(shift_id, worker_user_id, payload)
        .await
        .map_err(map_shift_error)?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// POST /api/v1/shifts/{shift_id}/clockout
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/clockout",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 201, description = "Clocked out", body = ClockoutResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not the assigned clinician", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Handover missing, wrong status, or no clock-in", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Clock out of an in-progress shift",
    description = "Requires a submitted handover (BR-F1-35). Computes worked_minutes and flips shift to 'completed'."
)]
pub async fn clock_out(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<(StatusCode, Json<ClockoutResponse>)> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let response = state
        .shift_service
        .clock_out(shift_id, worker_user_id)
        .await
        .map_err(map_shift_error)?;

    Ok((StatusCode::CREATED, Json(response)))
}

/// POST /api/v1/shifts/{shift_id}/handover/revision
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/handover/revision",
    request_body = HandoverRevisionRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 204, description = "Revision requested"),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not the shift creator", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Handover missing, no clock-out yet, or 24h revision window closed", body = ErrorResponse),
        (status = 422, description = "Validation error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Request a handover revision (hospital)",
    description = "Within 24 hours of clock-out (BR-F1-37), the hospital can request a handover revision with notes."
)]
pub async fn request_handover_revision(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<HandoverRevisionRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .request_handover_revision(shift_id, requester_user_id, payload.revision_notes)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/handover/approve
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/handover/approve",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 204, description = "Handover approved"),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Not the shift creator", body = ErrorResponse),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "No handover submitted, or already approved", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Approve the handover (hospital)",
    description = "Marks the handover as approved by the hospital. The PayoutScheduler picks up approved shifts on its next tick and disburses the clinician's net pay via SafeHaven."
)]
pub async fn approve_handover(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;
    state
        .shift_service
        .approve_handover(shift_id, requester_user_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/ratings/worker
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/ratings/worker",
    request_body = RateWorkerRequest,
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 201, description = "Rating recorded", body = RatingResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, description = "Not the shift creator", body = ErrorResponse),
        (status = 404, body = ErrorResponse),
        (status = 409, description = "Duplicate, shift not completed, or 7-day window closed", body = ErrorResponse),
        (status = 422, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Hospital rates the assigned worker",
    description = "Submit a 1–5 score for the assigned clinician. The cached `clinicians.rating` average is updated in the same transaction."
)]
pub async fn rate_worker(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<RateWorkerRequest>,
) -> AppResult<(StatusCode, Json<RatingResponse>)> {
    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let rating = state
        .shift_service
        .rate_worker(shift_id, requester_user_id, payload)
        .await
        .map_err(map_shift_error)?;

    Ok((StatusCode::CREATED, Json(rating)))
}

/// POST /api/v1/shifts/{shift_id}/ratings/hospital
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/ratings/hospital",
    request_body = RateHospitalRequest,
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 201, description = "Rating recorded", body = RatingResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, description = "Not the assigned clinician", body = ErrorResponse),
        (status = 404, body = ErrorResponse),
        (status = 409, description = "Duplicate, shift not completed, or 7-day window closed", body = ErrorResponse),
        (status = 422, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Worker rates the hospital",
    description = "Submit a 1–5 score plus the 4 sub-dimensions (staff support, equipment, communication, payment timeliness)."
)]
pub async fn rate_hospital(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<RateHospitalRequest>,
) -> AppResult<(StatusCode, Json<RatingResponse>)> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let rating = state
        .shift_service
        .rate_hospital(shift_id, worker_user_id, payload)
        .await
        .map_err(map_shift_error)?;

    Ok((StatusCode::CREATED, Json(rating)))
}

/// PATCH /api/v1/ratings/{rating_id}
#[utoipa::path(
    patch,
    path = "/api/v1/ratings/{rating_id}",
    request_body = EditRatingRequest,
    params(("rating_id" = Uuid, Path, description = "Rating unique identifier")),
    responses(
        (status = 200, description = "Rating updated", body = RatingResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, description = "Not the original rater", body = ErrorResponse),
        (status = 404, body = ErrorResponse),
        (status = 409, description = "48h edit window has closed", body = ErrorResponse),
        (status = 422, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Edit a rating (within 48 hours of submission)",
    description = "Per BR-F1-50 ratings can be updated within 48 hours of submission. Only the original rater may edit."
)]
pub async fn edit_rating(
    State(state): State<AppState>,
    Path(rating_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<EditRatingRequest>,
) -> AppResult<Json<RatingResponse>> {
    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let rating = state
        .shift_service
        .edit_rating(rating_id, requester_user_id, payload)
        .await
        .map_err(map_shift_error)?;

    Ok(Json(rating))
}

/// Default search radius (km) when the client does not supply one.
const NEARBY_DEFAULT_RADIUS_KM: f64 = 5.0;
/// Hard cap on the search radius (km) a client may request.
const NEARBY_MAX_RADIUS_KM: f64 = 50.0;
/// Default page size for nearby-shift discovery.
const NEARBY_DEFAULT_LIMIT: i64 = 50;
/// Hard cap on the page size a client may request.
const NEARBY_MAX_LIMIT: i64 = 100;

/// Query parameters for `GET /api/v1/worker/shifts/nearby`.
#[derive(Debug, serde::Deserialize)]
pub struct NearbyShiftsQuery {
    /// Live latitude of the worker. Must be paired with `lng`.
    pub lat: Option<f64>,
    /// Live longitude of the worker. Must be paired with `lat`.
    pub lng: Option<f64>,
    /// Optional GPS accuracy in metres, stored with the location.
    pub accuracy_meters: Option<f32>,
    /// Search radius in km (default 5, max 50).
    pub radius_km: Option<f64>,
    /// Page size (default 50, max 100).
    pub limit: Option<i64>,
    /// Rows to skip for pagination (default 0).
    pub offset: Option<i64>,
}

/// Normalized nearby-shift request: a validated origin plus paging.
type NearbyParams = (Option<shift_service::WorkerOrigin>, f64, i64, i64);

impl NearbyShiftsQuery {
    /// Validate the raw query and normalize it into `(origin, radius_km, limit,
    /// offset)`. Coordinates are all-or-nothing and range-checked; radius is
    /// clamped to `NEARBY_MAX_RADIUS_KM`; limit is clamped to `NEARBY_MAX_LIMIT`.
    /// Returns a human-readable message on invalid input (mapped to `400`).
    fn resolve(&self) -> Result<NearbyParams, String> {
        let origin = match (self.lat, self.lng) {
            (Some(lat), Some(lng)) => {
                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
                    return Err("lat must be -90..90 and lng must be -180..180".to_string());
                }
                Some(shift_service::WorkerOrigin {
                    lat,
                    lng,
                    accuracy_meters: self.accuracy_meters,
                })
            }
            (None, None) => None,
            _ => return Err("lat and lng must be supplied together".to_string()),
        };

        let radius_km = match self.radius_km {
            Some(r) if r <= 0.0 || r.is_nan() => {
                return Err("radius_km must be greater than 0".to_string())
            }
            Some(r) => r.min(NEARBY_MAX_RADIUS_KM),
            None => NEARBY_DEFAULT_RADIUS_KM,
        };
        let limit = match self.limit {
            Some(l) if l < 1 => return Err("limit must be at least 1".to_string()),
            Some(l) => l.min(NEARBY_MAX_LIMIT),
            None => NEARBY_DEFAULT_LIMIT,
        };
        let offset = match self.offset {
            Some(o) if o < 0 => return Err("offset must not be negative".to_string()),
            Some(o) => o,
            None => 0,
        };

        Ok((origin, radius_km, limit, offset))
    }
}

/// GET /api/v1/worker/shifts/nearby
#[utoipa::path(
    get,
    path = "/api/v1/worker/shifts/nearby",
    params(
        ("lat" = Option<f64>, Query, description = "Live worker latitude (-90..90); pair with lng"),
        ("lng" = Option<f64>, Query, description = "Live worker longitude (-180..180); pair with lat"),
        ("accuracy_meters" = Option<f32>, Query, description = "Optional GPS accuracy in metres"),
        ("radius_km" = Option<f64>, Query, description = "Search radius in km (default 5, max 50)"),
        ("limit" = Option<i64>, Query, description = "Page size (default 50, max 100)"),
        ("offset" = Option<i64>, Query, description = "Rows to skip (default 0)")
    ),
    responses(
        (status = 200, description = "Open shifts within the radius, ranked by urgency then distance", body = Vec<NearbyShiftCard>),
        (status = 400, description = "Invalid coordinates or paging parameters", body = ErrorResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, description = "Caller has no clinician profile", body = ErrorResponse),
        (status = 409, description = "No location supplied and none on file", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Shifts Near You (worker discovery)",
    description = "Returns open shifts within `radius_km` of the worker, ranked by urgency (STAT first) then distance. Supply live `lat`/`lng` to refresh the worker's location; otherwise the last-known location is used. Virtual shifts are always included and carry no distance."
)]
pub async fn list_nearby_shifts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NearbyShiftsQuery>,
) -> AppResult<Json<Vec<NearbyShiftCard>>> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let (origin, radius_km, limit, offset) = query.resolve().map_err(AppError::BadRequest)?;

    let cards = state
        .shift_service
        .list_nearby_shifts_for_worker(worker_user_id, origin, radius_km, limit, offset)
        .await
        .map_err(map_shift_error)?;
    Ok(Json(cards))
}

/// GET /api/v1/worker/shifts/my-applications
#[utoipa::path(
    get,
    path = "/api/v1/worker/shifts/my-applications",
    responses(
        (status = 200, description = "Combined list of expressed interests and applications", body = Vec<MyApplicationEntry>),
        (status = 401, body = ErrorResponse),
        (status = 403, description = "Caller has no clinician profile", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "My Applications tab",
    description = "Lists the worker's expressed interests and formal applications across shifts, newest first."
)]
pub async fn list_my_applications(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<MyApplicationEntry>>> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let rows = state
        .shift_service
        .list_my_applications(worker_user_id)
        .await
        .map_err(map_shift_error)?;
    Ok(Json(rows))
}

/// DELETE /api/v1/shifts/{shift_id}/interest
#[utoipa::path(
    delete,
    path = "/api/v1/shifts/{shift_id}/interest",
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 204, description = "Interest withdrawn"),
        (status = 401, body = ErrorResponse),
        (status = 403, description = "Caller has no clinician profile", body = ErrorResponse),
        (status = 404, description = "Shift not found, or no interest to withdraw", body = ErrorResponse),
        (status = 409, description = "Cannot withdraw after assignment", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Withdraw expressed interest",
    description = "Worker withdraws their interest in an open shift. Only allowed before the shift is assigned (BR-F1-17)."
)]
pub async fn withdraw_interest(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .withdraw_interest(shift_id, worker_user_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/bookmark
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/bookmark",
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 204, description = "Bookmarked"),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Bookmark a shift",
    description = "Worker saves a shift for later (§3.3.4)."
)]
pub async fn bookmark_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .bookmark_shift(shift_id, worker_user_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// DELETE /api/v1/shifts/{shift_id}/bookmark
#[utoipa::path(
    delete,
    path = "/api/v1/shifts/{shift_id}/bookmark",
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 204, description = "Bookmark removed"),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Remove a shift bookmark",
    description = "Worker removes a previously-saved bookmark."
)]
pub async fn unbookmark_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .unbookmark_shift(shift_id, worker_user_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/dismiss
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/dismiss",
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 204, description = "Dismissed"),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Dismiss a shift",
    description = "Worker removes a shift from their nearby feed. The shift itself is unaffected; it just won't appear in this clinician's discovery results."
)]
pub async fn dismiss_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .dismiss_shift(shift_id, worker_user_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/clockin/approval-request
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/clockin/approval-request",
    request_body = ClockinApprovalRequest,
    params(("shift_id" = Uuid, Path, description = "Shift unique identifier")),
    responses(
        (status = 201, description = "Approval request created", body = serde_json::Value),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, body = ErrorResponse),
        (status = 409, description = "Already has a pending or decided request", body = ErrorResponse),
        (status = 422, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Submit a GPS-fallback clock-in approval request (worker)",
    description = "When GPS is too inaccurate to clear the geofence, the worker submits a photo of the hospital entrance plus the device coords for hospital review (§3.6.6)."
)]
pub async fn request_clockin_approval(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<ClockinApprovalRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let claims = extract_claims(&headers)?;
    let worker_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let id = state
        .shift_service
        .request_clockin_approval(shift_id, worker_user_id, payload)
        .await
        .map_err(map_shift_error)?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "approval_request_id": id })),
    ))
}

/// POST /api/v1/clockin-approvals/{request_id}/approve
#[utoipa::path(
    post,
    path = "/api/v1/clockin-approvals/{request_id}/approve",
    request_body = ClockinApprovalDecisionRequest,
    params(("request_id" = Uuid, Path, description = "Approval request id")),
    responses(
        (status = 204, description = "Approved"),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, body = ErrorResponse),
        (status = 409, description = "Already decided", body = ErrorResponse),
        (status = 422, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Approve a manual clock-in request (hospital)",
    description = "Hospital admin approves a pending GPS-fallback clock-in request, unlocking the manual clock-in method for this (shift, clinician)."
)]
pub async fn approve_clockin_request(
    State(state): State<AppState>,
    Path(request_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<ClockinApprovalDecisionRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .decide_clockin_approval(request_id, requester_user_id, true, payload.notes)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/clockin-approvals/{request_id}/deny
#[utoipa::path(
    post,
    path = "/api/v1/clockin-approvals/{request_id}/deny",
    request_body = ClockinApprovalDecisionRequest,
    params(("request_id" = Uuid, Path, description = "Approval request id")),
    responses(
        (status = 204, description = "Denied"),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, body = ErrorResponse),
        (status = 409, description = "Already decided", body = ErrorResponse),
        (status = 422, body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Deny a manual clock-in request (hospital)",
    description = "Hospital admin denies a pending GPS-fallback clock-in request."
)]
pub async fn deny_clockin_request(
    State(state): State<AppState>,
    Path(request_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<ClockinApprovalDecisionRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let claims = extract_claims(&headers)?;
    let requester_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    state
        .shift_service
        .decide_clockin_approval(request_id, requester_user_id, false, payload.notes)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/assign
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/assign",
    request_body = ShiftAssignRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier")
    ),
    responses(
        (status = 204, description = "Shift assigned"),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Shift already assigned", body = ErrorResponse),
        (status = 422, description = "Validation error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Assign a clinician to a shift",
    description = "Hospital assigns a clinician to an open shift"
)]
pub async fn assign_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    Json(payload): Json<ShiftAssignRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .shift_service
        .assign_shift(shift_id, payload.clinician_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/cancel
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/cancel",
    request_body = ShiftCancelRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier")
    ),
    responses(
        (status = 204, description = "Shift cancelled"),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Invalid shift status", body = ErrorResponse),
        (status = 422, description = "Validation error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Cancel a shift",
    description = "Cancel an open or upcoming shift"
)]
pub async fn cancel_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    Json(payload): Json<ShiftCancelRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .shift_service
        .cancel_shift(shift_id, &payload.reason)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// POST /api/v1/shifts/{shift_id}/reschedule
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/reschedule",
    request_body = ShiftRescheduleRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier")
    ),
    responses(
        (status = 204, description = "Shift rescheduled"),
        (status = 404, description = "Shift not found", body = ErrorResponse),
        (status = 409, description = "Invalid shift status", body = ErrorResponse),
        (status = 422, description = "Validation error", body = ErrorResponse)
    ),
    tag = "shifts",
    summary = "Reschedule a shift",
    description = "Update the start time and duration for a shift"
)]
pub async fn reschedule_shift(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    Json(payload): Json<ShiftRescheduleRequest>,
) -> AppResult<StatusCode> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    state
        .shift_service
        .reschedule_shift(shift_id, payload.scheduled_start, payload.duration_hours)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_shift_error)
}

/// Error response for API documentation
#[derive(Debug, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct ErrorDetail {
    pub message: String,
    pub status: u16,
}

fn map_shift_error(e: ShiftServiceError) -> AppError {
    match e {
        ShiftServiceError::ValidationError(msg) => AppError::Validation(msg),
        ShiftServiceError::NotFound(id) => AppError::NotFound(format!("Shift {} not found", id)),
        ShiftServiceError::DatabaseError(e) => AppError::Database(e),
        ShiftServiceError::DuplicateShift(msg) => AppError::Conflict(msg),
        ShiftServiceError::HospitalNotApproved(msg) => AppError::Forbidden(msg),
        ShiftServiceError::DuplicateInterest => {
            AppError::Conflict("Shift interest already exists".to_string())
        }
        ShiftServiceError::DuplicateApplication => {
            AppError::Conflict("Shift application already exists".to_string())
        }
        ShiftServiceError::ProfileIncomplete => {
            AppError::Forbidden("Clinician profile is incomplete".to_string())
        }
        ShiftServiceError::ClinicianBusy => {
            AppError::Conflict("Clinician already assigned to an active shift".to_string())
        }
        ShiftServiceError::NotAuthorized => {
            AppError::Forbidden("Not authorized to view applications".to_string())
        }
        ShiftServiceError::AlreadyAssigned => {
            AppError::Conflict("Shift already assigned".to_string())
        }
        ShiftServiceError::InvalidStatus(msg) => AppError::Conflict(msg),
        ShiftServiceError::TooManyActiveShifts => AppError::Conflict(
            "You have 10 active shifts. Complete or cancel some before creating more".to_string(), ),
        ShiftServiceError::NotInterested => AppError::Conflict(
            "Hospital can only offer this shift to a clinician who expressed interest"
                .to_string(), ),
        ShiftServiceError::DuplicateOffer => {
            AppError::Conflict("Clinician already has an offer for this shift".to_string())
        }
        ShiftServiceError::NoPendingOffer => {
            AppError::NotFound("No pending offer for this shift".to_string())
        }
        ShiftServiceError::OfferExpired => AppError::Conflict("Offer has expired".to_string()),
        ShiftServiceError::ConsentRequired => {
            AppError::Validation("All NDPR consent boxes must be checked".to_string())
        }
        ShiftServiceError::NoClinicianProfile => {
            AppError::Forbidden("Authenticated user has no clinician profile".to_string())
        }
        ShiftServiceError::ScheduleConflict => AppError::Conflict(
            "Shift overlaps with another accepted shift".to_string(), ),
        ShiftServiceError::TooEarlyToClockIn => AppError::Conflict(
            "Clock-in is only allowed within 1 hour of the scheduled start time".to_string(), ),
        ShiftServiceError::MissedShift => AppError::Conflict(
            "Shift was missed (more than 60 minutes late). Cannot clock in.".to_string(), ),
        ShiftServiceError::OutOfGeofence(meters) => AppError::Conflict(format!(
            "You are {} metres from the hospital — outside the clock-in geofence",
            meters
        )),
        ShiftServiceError::HandoverRequired => AppError::Conflict(
            "Handover must be submitted before clock-out".to_string(), ),
        ShiftServiceError::HandoverEditWindowClosed => AppError::Conflict(
            "Handover edit window (1 hour after clock-out) has closed".to_string(), ),
        ShiftServiceError::RevisionWindowClosed => AppError::Conflict(
            "Hospital revision window (24 hours after clock-out) has closed".to_string(), ),
        ShiftServiceError::DuplicateRating => {
            AppError::Conflict("Rating already submitted for this shift".to_string())
        }
        ShiftServiceError::RatingWindowClosed => AppError::Conflict(
            "Rating submission window (7 days after shift completion) has closed".to_string(), ),
        ShiftServiceError::RatingNotFound => AppError::NotFound("Rating not found".to_string()),
        ShiftServiceError::RatingEditWindowClosed => AppError::Conflict(
            "Rating edit window (48 hours) has closed".to_string(), ),
        ShiftServiceError::DuplicateClockinApproval => AppError::Conflict(
            "Clock-in approval request already exists for this shift".to_string(), ),
        ShiftServiceError::ClockinApprovalNotFound => {
            AppError::NotFound("Clock-in approval request not found".to_string())
        }
        ShiftServiceError::ManualClockinNotApproved => AppError::Conflict(
            "Manual clock-in requires an approved GPS-fallback request".to_string(), ),
        ShiftServiceError::InsufficientWalletBalance { required, available } => {
            AppError::PaymentRequired(format!(
                "Insufficient wallet balance: shift requires {} kobo, wallet has {} kobo. Deposit funds before creating this shift.",
                required, available
            ))
        }
        ShiftServiceError::WalletError(msg) => {
            AppError::Conflict(format!("Wallet error: {msg}"))
        }
        ShiftServiceError::LocationRequired => AppError::Conflict(
            "Location required. Share your location or enable GPS to see nearby shifts."
                .to_string(),
        ),
        ShiftServiceError::ShiftUnavailable => {
            AppError::Conflict("Shift is no longer available".to_string())
        }
        ShiftServiceError::WorkerAlreadyDeclined => {
            AppError::Conflict("Worker already declined this shift".to_string())
        }
        ShiftServiceError::OfferAlreadyResponded => {
            AppError::Conflict("This offer has already been responded to".to_string())
        }
    }
}

#[cfg(test)]
mod nearby_query_tests {
    //! SCRUM-24 / US-08 — validation of the `GET /worker/shifts/nearby`
    //! query parameters (origin resolution, radius/limit/offset normalization).
    use super::*;

    fn q(
        lat: Option<f64>,
        lng: Option<f64>,
        radius_km: Option<f64>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> NearbyShiftsQuery {
        NearbyShiftsQuery {
            lat,
            lng,
            accuracy_meters: None,
            radius_km,
            limit,
            offset,
        }
    }

    /// UT001/UT019 — omitting coordinates is valid; origin falls back to the
    /// worker's last-known location downstream (None here).
    #[test]
    fn ut001_no_coords_defaults() {
        let (origin, radius, limit, offset) = q(None, None, None, None, None).resolve().unwrap();
        assert!(origin.is_none());
        assert_eq!(radius, NEARBY_DEFAULT_RADIUS_KM);
        assert_eq!(limit, NEARBY_DEFAULT_LIMIT);
        assert_eq!(offset, 0);
    }

    /// UT003 — valid live GPS is accepted and carried through as the origin.
    #[test]
    fn ut003_valid_gps_accepted() {
        let (origin, _, _, _) = q(Some(6.5244), Some(3.3792), None, None, None)
            .resolve()
            .unwrap();
        let origin = origin.expect("origin present");
        assert!((origin.lat - 6.5244).abs() < 1e-9);
        assert!((origin.lng - 3.3792).abs() < 1e-9);
    }

    /// UT020 — coordinates are all-or-nothing; a lone lat is rejected.
    #[test]
    fn ut020_partial_coords_rejected() {
        assert!(q(Some(6.5), None, None, None, None).resolve().is_err());
        assert!(q(None, Some(3.3), None, None, None).resolve().is_err());
    }

    /// Out-of-range coordinates are rejected.
    #[test]
    fn out_of_range_coords_rejected() {
        assert!(q(Some(91.0), Some(3.3), None, None, None).resolve().is_err());
        assert!(q(Some(6.5), Some(181.0), None, None, None).resolve().is_err());
    }

    /// UT004 — radius is honored and clamped to the 50km ceiling.
    #[test]
    fn ut004_radius_clamped() {
        let (_, radius, _, _) = q(None, None, Some(3.0), None, None).resolve().unwrap();
        assert_eq!(radius, 3.0);
        let (_, clamped, _, _) = q(None, None, Some(999.0), None, None).resolve().unwrap();
        assert_eq!(clamped, NEARBY_MAX_RADIUS_KM);
    }

    /// A non-positive radius is invalid.
    #[test]
    fn non_positive_radius_rejected() {
        assert!(q(None, None, Some(0.0), None, None).resolve().is_err());
        assert!(q(None, None, Some(-1.0), None, None).resolve().is_err());
    }

    /// Paging is validated and the page size clamped to its ceiling.
    #[test]
    fn paging_validated_and_clamped() {
        assert!(q(None, None, None, Some(0), None).resolve().is_err());
        assert!(q(None, None, None, None, Some(-1)).resolve().is_err());
        let (_, _, limit, offset) = q(None, None, None, Some(9999), Some(40)).resolve().unwrap();
        assert_eq!(limit, NEARBY_MAX_LIMIT);
        assert_eq!(offset, 40);
    }
}

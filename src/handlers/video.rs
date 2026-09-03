// ! HTTP surface for LiveKit video consultations.
// !
// ! Thin by convention: extract the claims, make one service call, map the
// ! error. Fine-grained authorization lives in `VideoService`, because the
// ! route-level `require_role` guard cannot know which hospital owns a shift.
// !
// ! There is deliberately no `ErrorResponse` type in this module — six handler
// ! modules already declare one and utoipa keys components by type name, so a
// ! seventh would deepen an existing collision. The shifts one is referenced
// ! instead.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use uuid::Uuid;

use crate::{
    models::video_session::{
        ConsultSessionView, EndConsultRequest, EndConsultResponse, JoinConsultRequest,
        JoinConsultResponse, LeaveConsultResponse,
    },
    routes::AppState,
    services::video_service::VideoServiceError,
    utils::{
        errors::{AppError, AppResult},
        extract_claims,
    },
};

/// POST /api/v1/shifts/{shift_id}/consult/token
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/consult/token",
    request_body = JoinConsultRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 200, description = "Join token minted", body = JoinConsultResponse),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not the assigned clinician, wrong hospital, or observer mode requested by a worker", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift not found", body = crate::handlers::shifts::ErrorResponse),
        (status = 409, description = "Not a virtual shift, status not joinable, outside the consultation window, or already ended", body = crate::handlers::shifts::ErrorResponse),
        (status = 500, description = "LiveKit unavailable", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "video",
    summary = "Mint a LiveKit join token for a virtual shift",
    description = "Returns a short-lived join token plus the LiveKit URL. Hand `url` and `token` straight to the client SDK. `expires_at` is a join deadline, not a call deadline — the call survives it once connected. Idempotent: call again if the user sits on the pre-join screen."
)]
pub async fn issue_join_token(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<JoinConsultRequest>,
) -> AppResult<Json<JoinConsultResponse>> {
    let claims = extract_claims(&headers)?;

    state
        .video_service
        .issue_join_token(shift_id, &claims, payload)
        .await
        .map(Json)
        .map_err(map_video_error)
}

/// GET /api/v1/shifts/{shift_id}/consult
#[utoipa::path(
    get,
    path = "/api/v1/shifts/{shift_id}/consult",
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 200, description = "Consultation state", body = ConsultSessionView),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not a party to this consultation", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift not found, or nobody has requested a token yet", body = crate::handlers::shifts::ErrorResponse),
        (status = 409, description = "Not a virtual shift", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "video",
    summary = "Read the state of a shift's consultation",
    description = "Platform admins (super/operations) get metadata only and can never obtain a token. `live: false` means LiveKit was unreachable and the participant list is the last known webhook-fed state."
)]
pub async fn get_session(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<ConsultSessionView>> {
    let claims = extract_claims(&headers)?;

    state
        .video_service
        .get_session(shift_id, &claims)
        .await
        .map(Json)
        .map_err(map_video_error)
}

/// POST /api/v1/shifts/{shift_id}/consult/leave
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/consult/leave",
    responses(
        (status = 200, description = "Departure recorded", body = LeaveConsultResponse),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not a party to this consultation", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift or session not found", body = crate::handlers::shifts::ErrorResponse)
    ),
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    tag = "video",
    summary = "Report leaving a consultation",
    description = "Fire from the Leave button and `beforeunload` in addition to `room.disconnect()`. Idempotent. Does not end the call for anyone else and does not clock the worker out."
)]
pub async fn leave_session(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<LeaveConsultResponse>> {
    let claims = extract_claims(&headers)?;

    state
        .video_service
        .leave_session(shift_id, &claims)
        .await
        .map(Json)
        .map_err(map_video_error)
}

/// POST /api/v1/shifts/{shift_id}/consult/end
#[utoipa::path(
    post,
    path = "/api/v1/shifts/{shift_id}/consult/end",
    request_body = EndConsultRequest,
    params(
        ("shift_id" = Uuid, Path, description = "Shift unique identifier"),
    ),
    responses(
        (status = 200, description = "Consultation ended", body = EndConsultResponse),
        (status = 401, description = "Missing or invalid token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not the owning hospital", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "Shift or session not found", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "video",
    summary = "End a consultation for everyone",
    description = "Disconnects every participant and marks the session ended. Idempotent: ending an already-ended session returns the original `ended_at`. Does not clock the worker out — a handover is still required."
)]
pub async fn end_session(
    State(state): State<AppState>,
    Path(shift_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<EndConsultRequest>,
) -> AppResult<Json<EndConsultResponse>> {
    let claims = extract_claims(&headers)?;

    state
        .video_service
        .end_session(shift_id, &claims, payload.reason)
        .await
        .map(Json)
        .map_err(map_video_error)
}

fn map_video_error(e: VideoServiceError) -> AppError {
    match e {
        VideoServiceError::Database(e) => AppError::Database(e),
        // The real cause belongs in the logs, not in a clinician's browser.
        VideoServiceError::LiveKit(e) => {
            tracing::error!("LiveKit call failed: {e}");
            AppError::InternalServerError("Video service is unavailable".to_string())
        }
        VideoServiceError::ShiftNotFound(id) => {
            AppError::NotFound(format!("Shift {} not found", id))
        }
        VideoServiceError::SessionNotFound => {
            AppError::NotFound("No consultation has been started for this shift".to_string())
        }
        VideoServiceError::NotVirtualShift => AppError::Conflict(
            "Video consultations are only available for virtual shifts".to_string(),
        ),
        VideoServiceError::NotAuthorized => {
            AppError::Forbidden("Not authorized to join this consultation".to_string())
        }
        VideoServiceError::NoClinicianProfile => {
            AppError::Forbidden("Authenticated user has no clinician profile".to_string())
        }
        VideoServiceError::ShiftNotJoinable(msg) => AppError::Conflict(msg),
        VideoServiceError::OutsideWindow => AppError::Conflict(
            "The consultation opens one hour before the shift starts and closes one hour after it ends".to_string(),
        ),
        VideoServiceError::SessionEnded => {
            AppError::Conflict("This consultation has already ended".to_string())
        }
        VideoServiceError::NotConfigured => {
            AppError::InternalServerError("Video service is not configured".to_string())
        }
    }
}

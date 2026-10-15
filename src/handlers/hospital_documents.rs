//! HTTP surface for a hospital's legal verification documents.
//!
//! Thin by convention: validate, extract the claims, one service call, map the
//! error. The tenant check is in `HospitalDocumentService`, because the
//! route-level `require_role` guard cannot know which hospital the `{hospital_id}`
//! in the path belongs to — and here that difference is a hospital's licence
//! files and a director's ID.
//!
//! The hospital-facing and platform-facing routes are separate handlers rather
//! than one with a role branch, so the `require_permission` gate on the admin
//! side is visible in `create_router` next to the route it protects.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use uuid::Uuid;
use validator::Validate;

use crate::models::registration::{
    HospitalDocumentResponse, ReviewDocumentRequest, UploadDocumentRequest,
};
use crate::routes::AppState;
use crate::services::hospital_document_service::HospitalDocumentError;
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

/// How many drafts a submit-for-review call moved.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct SubmitForReviewResponse {
    pub submitted: u64,
}

/// POST /api/v1/hospitals/{hospital_id}/documents
#[utoipa::path(
    post,
    path = "/api/v1/hospitals/{hospital_id}/documents",
    request_body = UploadDocumentRequest,
    params(("hospital_id" = Uuid, Path, description = "Hospital the document belongs to")),
    responses(
        (status = 201, description = "Document stored", body = HospitalDocumentResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not this caller's hospital", body = crate::handlers::shifts::ErrorResponse),
        (status = 422, description = "Validation error", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "hospitals",
    summary = "Upload a legal verification document",
    description = "Stores the metadata for a document already uploaded to Cloudinary via \
                   `GET /api/v1/uploads/signature?kind=hospital_document`. One document per \
                   `document_type` per hospital: re-uploading replaces the previous file and \
                   returns it to `draft`, clearing any earlier review.",
    security(("bearerAuth" = []))
)]
pub async fn upload_document(
    State(state): State<AppState>,
    Path(hospital_id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<UploadDocumentRequest>,
) -> AppResult<(StatusCode, Json<HospitalDocumentResponse>)> {
    let claims = extract_claims(&headers)?;
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let doc = state
        .hospital_document_service
        .upload(hospital_id, &claims, &payload)
        .await
        .map_err(map_document_error)?;

    Ok((StatusCode::CREATED, Json(doc.into())))
}

/// GET /api/v1/hospitals/{hospital_id}/documents
#[utoipa::path(
    get,
    path = "/api/v1/hospitals/{hospital_id}/documents",
    params(("hospital_id" = Uuid, Path, description = "Hospital whose documents to list")),
    responses(
        (status = 200, description = "Documents on file", body = Vec<HospitalDocumentResponse>),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not this caller's hospital", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "hospitals",
    summary = "List a hospital's documents",
    description = "Every document on file for this hospital, newest first, with its \
                   `submission_status` and — once a reviewer has decided — `reviewed_at` and \
                   `review_notes`. Scoped to the caller's own hospital; a super admin may read \
                   any. `reviewed_by` is deliberately not returned: the hospital needs the \
                   decision and the reason, not the individual reviewer's identity.",
    security(("bearerAuth" = []))
)]
pub async fn list_documents(
    State(state): State<AppState>,
    Path(hospital_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<HospitalDocumentResponse>>> {
    let claims = extract_claims(&headers)?;
    let docs = state
        .hospital_document_service
        .list(hospital_id, &claims)
        .await
        .map_err(map_document_error)?;
    Ok(Json(docs.into_iter().map(Into::into).collect()))
}

/// DELETE /api/v1/hospitals/{hospital_id}/documents/{document_id}
#[utoipa::path(
    delete,
    path = "/api/v1/hospitals/{hospital_id}/documents/{document_id}",
    params(
        ("hospital_id" = Uuid, Path, description = "Hospital the document belongs to"),
        ("document_id" = Uuid, Path, description = "Document to remove")
    ),
    responses(
        (status = 204, description = "Document removed"),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not this caller's hospital", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "No such document for this hospital", body = crate::handlers::shifts::ErrorResponse),
        (status = 409, description = "Already submitted for review", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "hospitals",
    summary = "Delete a draft document",
    description = "Only while the document is still `draft`. Once submitted it is part of the \
                   compliance record and cannot be withdrawn.",
    security(("bearerAuth" = []))
)]
pub async fn delete_document(
    State(state): State<AppState>,
    Path((hospital_id, document_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let claims = extract_claims(&headers)?;
    state
        .hospital_document_service
        .delete(hospital_id, document_id, &claims)
        .await
        .map_err(map_document_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/v1/hospitals/{hospital_id}/documents/submit
#[utoipa::path(
    post,
    path = "/api/v1/hospitals/{hospital_id}/documents/submit",
    params(("hospital_id" = Uuid, Path, description = "Hospital submitting its documents")),
    responses(
        (status = 200, description = "Drafts moved into review", body = SubmitForReviewResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Not this caller's hospital", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "hospitals",
    summary = "Submit documents for compliance review",
    description = "Moves every `draft` document to `under_review`. Documents already approved or \
                   rejected keep their decision, so fixing one file does not reopen the others.",
    security(("bearerAuth" = []))
)]
pub async fn submit_documents(
    State(state): State<AppState>,
    Path(hospital_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Json<SubmitForReviewResponse>> {
    let claims = extract_claims(&headers)?;
    let submitted = state
        .hospital_document_service
        .submit_for_review(hospital_id, &claims)
        .await
        .map_err(map_document_error)?;
    Ok(Json(SubmitForReviewResponse { submitted }))
}

/// GET /api/v1/admin/hospitals/{hospital_id}/documents
#[utoipa::path(
    get,
    path = "/api/v1/admin/hospitals/{hospital_id}/documents",
    params(("hospital_id" = Uuid, Path, description = "Hospital under review")),
    responses(
        (status = 200, description = "Documents with their review state", body = Vec<HospitalDocumentResponse>),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Permission required", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "admin",
    summary = "Read a hospital's documents for review",
    description = "The full rows, not the single derived `documents_status` string that \
                   `GET /admin/hospitals/{id}` returns — a reviewer needs the file URLs, expiry \
                   dates and per-document state.",
    security(("bearerAuth" = []))
)]
pub async fn admin_list_documents(
    State(state): State<AppState>,
    Path(hospital_id): Path<Uuid>,
) -> AppResult<Json<Vec<HospitalDocumentResponse>>> {
    let docs = state
        .hospital_document_service
        .list_for_review(hospital_id)
        .await
        .map_err(map_document_error)?;
    Ok(Json(docs.into_iter().map(Into::into).collect()))
}

/// PATCH /api/v1/admin/hospitals/{hospital_id}/documents/{document_id}
#[utoipa::path(
    patch,
    path = "/api/v1/admin/hospitals/{hospital_id}/documents/{document_id}",
    request_body = ReviewDocumentRequest,
    params(
        ("hospital_id" = Uuid, Path, description = "Hospital under review"),
        ("document_id" = Uuid, Path, description = "Document being decided")
    ),
    responses(
        (status = 200, description = "Decision recorded", body = HospitalDocumentResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::handlers::shifts::ErrorResponse),
        (status = 403, description = "Permission required", body = crate::handlers::shifts::ErrorResponse),
        (status = 404, description = "No such document", body = crate::handlers::shifts::ErrorResponse),
        (status = 422, description = "Rejection without notes, or a return to draft", body = crate::handlers::shifts::ErrorResponse)
    ),
    tag = "admin",
    summary = "Approve or reject a document",
    description = "Writes `submission_status`, `reviewed_by`, `reviewed_at` and `review_notes`. \
                   A rejection must carry notes — the hospital has to know which file to replace \
                   and why. A reviewer cannot set `draft`, which is the hospital's own state.",
    security(("bearerAuth" = []))
)]
pub async fn admin_review_document(
    State(state): State<AppState>,
    Path((_hospital_id, document_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(payload): Json<ReviewDocumentRequest>,
) -> AppResult<Json<HospitalDocumentResponse>> {
    let claims = extract_claims(&headers)?;
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    // The reviewer is resolved from the token, never from the body — an
    // `reviewed_by` accepted from a client would let anyone attribute a
    // compliance decision to someone else.
    let reviewer_user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user ID in token".to_string()))?;

    let doc = state
        .hospital_document_service
        .review(
            document_id,
            payload.submission_status,
            payload.review_notes.as_deref(),
            reviewer_user_id,
        )
        .await
        .map_err(map_document_error)?;

    Ok(Json(doc.into()))
}

fn map_document_error(e: HospitalDocumentError) -> AppError {
    match e {
        HospitalDocumentError::WrongHospital | HospitalDocumentError::NotAuthorized => {
            AppError::Forbidden(e.to_string())
        }
        HospitalDocumentError::NotFound => AppError::NotFound(e.to_string()),
        HospitalDocumentError::NotADraft => AppError::Conflict(e.to_string()),
        HospitalDocumentError::RejectionNeedsNotes
        | HospitalDocumentError::CannotReviewToDraft => AppError::Validation(e.to_string()),
        HospitalDocumentError::Database(err) => {
            tracing::error!("hospital document database error: {err}");
            AppError::InternalServerError("Could not complete the document operation".to_string())
        }
    }
}

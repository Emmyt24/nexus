// ! Cloudinary signed-upload endpoint. Any authenticated user can request a
// ! short-lived signature; the frontend then uploads the image directly to
// ! Cloudinary and saves the returned URL via the hospital-logo / worker-avatar
// ! endpoints.

use axum::{extract::Query, http::HeaderMap, Json};
use serde::Deserialize;
use utoipa::IntoParams;

use crate::services::cloudinary::{self, SignedUpload};
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

#[derive(Debug, Deserialize, IntoParams)]
pub struct SignatureQuery {
    /// What the upload is for: `hospital_logo` or `worker_avatar`.
    pub kind: Option<String>,
}

/// Map an upload `kind` to a fixed Cloudinary folder (kept server-side so a
/// client can't scatter uploads across arbitrary folders).
fn folder_for(kind: Option<&str>) -> &'static str {
    match kind {
        Some("hospital_logo") => "nexuscare/hospital_logos",
        Some("worker_avatar") => "nexuscare/worker_avatars",
        _ => "nexuscare/uploads",
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/uploads/signature",
    params(SignatureQuery),
    responses(
        (status = 200, description = "Signed Cloudinary upload payload", body = SignedUpload),
        (status = 401, description = "Missing or invalid token"),
        (status = 503, description = "Cloudinary not configured")
    ),
    tag = "uploads",
    summary = "Get a signed Cloudinary upload payload",
    description = "Returns { cloud_name, api_key, timestamp, folder, signature, upload_url }. The frontend POSTs a multipart form to `upload_url` with the file plus these fields, then saves the returned `secure_url` (hospital → PATCH /hospitals/{id}, worker → PATCH /clinicians/{id}/avatar)."
)]
pub async fn upload_signature(
    headers: HeaderMap,
    Query(q): Query<SignatureQuery>,
) -> AppResult<Json<SignedUpload>> {
    // Authenticated users only (any role).
    let _claims = extract_claims(&headers)?;

    let folder = folder_for(q.kind.as_deref());
    cloudinary::signed_upload(folder).map(Json).ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("Cloudinary is not configured on the server"))
    })
}

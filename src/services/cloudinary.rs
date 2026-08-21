// ! Cloudinary signed-upload helper. The backend never handles the image bytes;
// ! it only issues a short-lived signature so the frontend can upload directly
// ! to Cloudinary. Signature spec: SHA1 of the alphabetically-sorted upload
// ! params (`folder`, `timestamp`) joined `k=v&k=v`, with the API secret
// ! appended, hex-encoded. See https://cloudinary.com/documentation/upload_images#generating_authentication_signatures

use serde::Serialize;
use sha1::{Digest, Sha1};

/// Everything the frontend needs to POST a signed upload to Cloudinary.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SignedUpload {
    pub cloud_name: String,
    pub api_key: String,
    pub timestamp: i64,
    pub folder: String,
    pub signature: String,
    /// The Cloudinary endpoint to POST the multipart form to.
    pub upload_url: String,
}

/// Build a signed upload payload for `folder`. Returns `None` if the
/// `CLOUDINARY_*` env vars are not configured.
pub fn signed_upload(folder: &str) -> Option<SignedUpload> {
    let cloud_name = non_empty_env("CLOUDINARY_CLOUD_NAME")?;
    let api_key = non_empty_env("CLOUDINARY_API_KEY")?;
    let api_secret = non_empty_env("CLOUDINARY_API_SECRET")?;

    let timestamp = chrono::Utc::now().timestamp();

    // Params the frontend will send (besides file/api_key/signature), sorted
    // alphabetically: folder, timestamp. The frontend MUST send exactly these.
    let to_sign = format!("folder={folder}&timestamp={timestamp}{api_secret}");
    let signature = hex::encode(Sha1::digest(to_sign.as_bytes()));

    Some(SignedUpload {
        upload_url: format!("https://api.cloudinary.com/v1_1/{cloud_name}/image/upload"),
        cloud_name,
        api_key,
        timestamp,
        folder: folder.to_string(),
        signature,
    })
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

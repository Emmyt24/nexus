// ! Inbound webhook endpoints.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::routes::AppState;
use crate::services::wallet_service::WebhookOutcome;

type HmacSha256 = Hmac<Sha256>;

const SIGNATURE_HEADER: &str = "x-safehaven-signature";

/// Validate `body` against `signature_hex` using `secret`. Returns false on
fn signature_matches(body: &[u8], secret: &[u8], signature_hex: &str) -> bool {
    let expected = match hex::decode(signature_hex.trim()) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let computed = mac.finalize().into_bytes();
    if computed.len() != expected.len() {
        return false;
    }
    computed.ct_eq(expected.as_slice()).into()
}

/// POST /api/v1/webhooks/safehaven
#[utoipa::path(
    post,
    path = "/api/v1/webhooks/safehaven",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Acknowledged"),
        (status = 400, description = "Malformed JSON"),
        (status = 401, description = "Invalid signature")
    ),
    tag = "webhooks",
    summary = "SafeHaven gateway webhook receiver",
    description = "Receives transfer / virtual-account / sub-account inflow notifications. Idempotent: re-deliveries are recognised by `data._id` / `data.sessionId`."
)]
pub async fn safehaven_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    // 1. Signature verification. SafeHaven does NOT sign these webhooks (no
    //    signature header is sent — confirmed from a live capture); security
    //    relies on the unguessable callback URL. So we only reject when a
    //    signature header IS present but doesn't match; an absent header is
    //    accepted. (If SafeHaven adds signing later, this still validates it.)
    let secret = std::env::var("SAFEHAVEN_WEBHOOK_SECRET").unwrap_or_default();
    if !secret.is_empty() {
        if let Some(sig) = headers.get(SIGNATURE_HEADER).and_then(|v| v.to_str().ok()) {
            if !sig.is_empty() && !signature_matches(&body, secret.as_bytes(), sig) {
                tracing::warn!("SafeHaven webhook rejected: invalid signature");
                return Err((StatusCode::UNAUTHORIZED, "invalid signature"));
            }
        }
    }

    // 2. Parse JSON. Anything malformed is a 400.
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("SafeHaven webhook payload not JSON: {e}");
            return Err((StatusCode::BAD_REQUEST, "invalid json body"));
        }
    };

    // 3. Dispatch to the wallet service. Failures are logged but we still
    let outcome = match state.wallet_service.process_webhook(&payload).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("SafeHaven webhook processing failed: {e}");
            // Still 200 so SafeHaven stops retrying; row keeps the error.
            return Ok(Json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            })));
        }
    };

    let body = match outcome {
        WebhookOutcome::AlreadySeen => {
            serde_json::json!({ "status": "ok", "deduped": true })
        }
        WebhookOutcome::Ignored => {
            serde_json::json!({ "status": "ok", "ignored": true })
        }
        WebhookOutcome::DepositCredited {
            deposit_id,
            hospital_id,
            amount_kobo,
        } => serde_json::json!({
            "status": "ok",
            "deposit_id": deposit_id,
            "hospital_id": hospital_id,
            "amount_kobo": amount_kobo
        }),
    };

    Ok(Json(body))
}

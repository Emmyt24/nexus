// ! Role-based authorization middleware.

use axum::{
    extract::{Query, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::models::user::UserRole;
use crate::utils::extract_claims_with_query_fallback;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Build an Axum middleware that admits only callers whose JWT `role` claim is
pub fn require_role(
    allowed: &'static [UserRole],
) -> impl Fn(Request, Next) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
       + Clone
       + Send
       + Sync
       + 'static {
    move |req: Request, next: Next| {
        Box::pin(async move {
            // Most routes only ever send the JWT via the Authorization header;
            // this also accepts the `?token=` fallback so routes like the SSE
            // stream (whose handler needs it for EventSource, which can't set
            // custom headers) aren't rejected here before the handler runs.
            let query_token = Query::<TokenQuery>::try_from_uri(req.uri())
                .ok()
                .and_then(|q| q.0.token);

            let claims =
                match extract_claims_with_query_fallback(req.headers(), query_token.as_deref()) {
                    Ok(c) => c,
                    Err(_) => return reject(StatusCode::UNAUTHORIZED, "Missing or invalid token"),
                };
            if !allowed.iter().any(|r| r == &claims.role) {
                return reject(StatusCode::FORBIDDEN, "Insufficient role for this endpoint");
            }
            next.run(req).await
        })
    }
}

fn reject(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

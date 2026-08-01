//! Permission-based authorization middleware for admin endpoints (Admin §1.2).
//! Mirrors `require_role` but checks a single `Permission` against the caller's
//! role via `UserRole::has_permission`, returning an `AppError` JSON envelope.

use axum::{
    extract::Request,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::models::permission::Permission;
use crate::utils::errors::AppError;
use crate::utils::extract_claims;

/// Build an Axum middleware admitting only callers whose role holds `perm`.
pub fn require_permission(
    perm: Permission,
) -> impl Fn(Request, Next) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
       + Clone
       + Send
       + Sync
       + 'static { 
    move |req: Request, next: Next| {
        Box::pin(async move {
            // Reject requests without a valid bearer token.
            let claims = match extract_claims(req.headers()) {
                Ok(c) => c,
                Err(_) => {
                    return AppError::Unauthorized("Missing or invalid token".to_string())
                        .into_response()
                }
            };
            // Reject callers whose role lacks the required permission.
            if !claims.role.has_permission(perm) {
                return AppError::Forbidden(
                    "Insufficient permission for this endpoint".to_string(),
                )
                .into_response();
            }
            next.run(req).await
        })
    }
}

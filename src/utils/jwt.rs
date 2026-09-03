use crate::models::user::{Claims, UserRole};
use crate::utils::errors::AppError;
use axum::http::HeaderMap;
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use uuid::Uuid;

/// The signing/verification secret. Every issuer and the decoder must agree on
/// the fallback, or tokens minted by one path fail to verify on another.
fn jwt_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Mint an access token. This is the single issuer for the whole app — minting
/// a private `Claims` shape elsewhere produces tokens `extract_claims` cannot
/// decode. Returns the token and its lifetime in seconds.
pub fn issue_access_token(
    user_id: Uuid,
    email: &str,
    role: UserRole,
    hospital_id: Option<String>,
) -> Result<(String, u64), String> {
    let expiry_hours: u64 = std::env::var("JWT_EXPIRY_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);

    let now = Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: user_id.to_string(),
        email: email.to_string(),
        role,
        hospital_id,
        exp: now + (expiry_hours as usize * 3600),
        iat: now,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret().as_bytes()),
    )
    .map_err(|e| e.to_string())?;

    Ok((token, expiry_hours * 3600))
}

/// Extract and decode JWT claims from the `Authorization: Bearer <token>` header

pub fn extract_claims(headers: &HeaderMap) -> Result<Claims, AppError> {
    let token = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| {
            AppError::Unauthorized("Missing or invalid Authorization header".to_string())
        })?;

    decode::<Claims>(
        token,
        &DecodingKey::from_secret(jwt_secret().as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .map_err(|_| AppError::Unauthorized("Invalid or expired token".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// A token from `issue_access_token` must decode through `extract_claims`.
    ///
    /// Regression: the clinician-registration flow used to mint its own `Claims`
    /// without `email`, which the shared `Claims` requires — so every token from
    /// `POST /api/v1/clinicians/otp/verify` failed with 401 everywhere.
    #[test]
    fn issued_token_round_trips_through_extract_claims() {
        // The secret is process-global; this test is the only one touching it.
        std::env::set_var("JWT_SECRET", "round-trip-test-secret");

        let user_id = Uuid::new_v4();
        let (token, ttl) = issue_access_token(
            user_id,
            "worker@example.test",
            UserRole::HealthWorker,
            None,
        )
        .expect("token issued");
        assert!(ttl > 0);

        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );

        let claims = extract_claims(&headers).expect("token decodes");
        assert_eq!(claims.sub, user_id.to_string());
        assert_eq!(claims.email, "worker@example.test");
        assert_eq!(claims.role, UserRole::HealthWorker);
        assert_eq!(claims.hospital_id, None);
    }
}

use std::sync::Arc;

use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::user::{LoginResponse, User, UserResponse, UserRole};
use crate::services::email_outbox_service::{EmailOutboxError, EmailOutboxService};
use crate::services::email_templates;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("User not found")]
    NotFound,
    #[error("Invalid or expired OTP")]
    InvalidOtp,
    #[error("Invalid or expired token")]
    InvalidToken,
    #[error("Invalid credentials")]
    InvalidCredentials,
    #[error("Account is deactivated")]
    Deactivated,
    #[error("Email queue error: {0}")]
    EmailQueue(#[from] EmailOutboxError),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Internal error: {0}")]
    Internal(String),
}

pub struct AuthService {
    pool: PgPool,
    email_outbox: Arc<EmailOutboxService>,
}

impl AuthService {
    pub fn new(pool: PgPool, email_outbox: Arc<EmailOutboxService>) -> Self {
        Self { pool, email_outbox }
    }

    // Email OTP login — step 1: send OTP
    pub async fn send_login_otp(&self, email: &str) -> Result<(), AuthError> {
        let email = email.trim().to_lowercase();

        // Verify email belongs to an active user
        let exists: Option<(bool,)> =
            sqlx::query_as("SELECT is_active FROM users WHERE email = $1")
                .bind(&email)
                .fetch_optional(&self.pool)
                .await?;

        let (is_active,) = exists.ok_or(AuthError::NotFound)?;
        if !is_active {
            return Err(AuthError::Deactivated);
        }

        let code = generate_otp();
        let expires_at = Utc::now() + Duration::minutes(10);

        sqlx::query(
            "INSERT INTO login_email_otp_codes (email, code, expires_at) VALUES ($1, $2, $3)",
        )
        .bind(&email)
        .bind(&code)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        let content = email_templates::email_otp(&code, 10);
        self.email_outbox.enqueue_email(&email, &content).await?;
        Ok(())
    }

    // Email OTP login — step 2: verify OTP and issue tokens
    pub async fn verify_login_otp(
        &self,
        email: &str,
        code: &str,
    ) -> Result<LoginResponse, AuthError> {
        let email = email.trim().to_lowercase();

        // Validate OTP
        let otp: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT id, used FROM login_email_otp_codes
             WHERE email = $1 AND code = $2 AND expires_at > NOW()
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&email)
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;

        let (otp_id, used) = otp.ok_or(AuthError::InvalidOtp)?;
        if used {
            return Err(AuthError::InvalidOtp);
        }

        // Mark OTP used
        sqlx::query("UPDATE login_email_otp_codes SET used = TRUE WHERE id = $1")
            .bind(otp_id)
            .execute(&self.pool)
            .await?;

        let user = self.fetch_user_by_email(&email).await?;
        self.complete_login(user).await
    }

    // Email/password login (existing login handler delegates here)
    pub async fn login_with_password(
        &self,
        email: &str,
        password: &str,
    ) -> Result<LoginResponse, AuthError> {
        let user: Option<User> = sqlx::query_as(
            "SELECT id, hospital_id, first_name, last_name, email, phone, password_hash,
                    role, role_label, avatar_url, is_active, last_login_at, created_at, updated_at
             FROM users WHERE email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        let user = user.ok_or(AuthError::InvalidCredentials)?;
        if !user.is_active {
            return Err(AuthError::Deactivated);
        }

        let valid = verify_password(password, &user.password_hash)
            .map_err(|e| AuthError::Internal(e.to_string()))?;
        if !valid {
            return Err(AuthError::InvalidCredentials);
        }

        self.complete_login(user).await
    }

    // Forgot password — send reset link
    pub async fn forgot_password(&self, email: &str) -> Result<(), AuthError> {
        let user: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM users WHERE email = $1 AND is_active = TRUE")
                .bind(email)
                .fetch_optional(&self.pool)
                .await?;

        // Silently succeed if email not found (don't leak user existence)
        let Some((user_id,)) = user else {
            return Ok(());
        };

        let raw_token = Uuid::new_v4().to_string();
        let token_hash = sha256_hex(&raw_token);
        let expires_at = Utc::now() + Duration::hours(1);

        sqlx::query(
            "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at)
             VALUES ($1, $2, $3)",
        )
        .bind(user_id)
        .bind(&token_hash)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        let api_base =
            std::env::var("API_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
        let reset_link = format!("{}/reset-password?token={}", api_base, raw_token);

        let content = email_templates::password_reset(&reset_link);
        self.email_outbox.enqueue_email(email, &content).await?;

        Ok(())
    }

    // Reset password with token
    pub async fn reset_password(
        &self,
        raw_token: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        let token_hash = sha256_hex(raw_token);

        let row: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT user_id, used FROM password_reset_tokens
             WHERE token_hash = $1 AND expires_at > NOW()",
        )
        .bind(&token_hash)
        .fetch_optional(&self.pool)
        .await?;

        let (user_id, used) = row.ok_or(AuthError::InvalidToken)?;
        if used {
            return Err(AuthError::InvalidToken);
        }

        let password_hash =
            hash_password(new_password).map_err(|e| AuthError::Internal(e.to_string()))?;

        let mut tx = self.pool.begin().await?;

        sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
            .bind(&password_hash)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("UPDATE password_reset_tokens SET used = TRUE WHERE token_hash = $1")
            .bind(&token_hash)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    // Refresh access token
    pub async fn refresh_token(&self, raw_token: &str) -> Result<LoginResponse, AuthError> {
        let token_hash = sha256_hex(raw_token);

        let row: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT user_id, revoked FROM refresh_tokens
             WHERE token_hash = $1 AND expires_at > NOW()",
        )
        .bind(&token_hash)
        .fetch_optional(&self.pool)
        .await?;

        let (user_id, revoked) = row.ok_or(AuthError::InvalidToken)?;
        if revoked {
            return Err(AuthError::InvalidToken);
        }

        // Rotate: revoke old token
        sqlx::query("UPDATE refresh_tokens SET revoked = TRUE WHERE token_hash = $1")
            .bind(&token_hash)
            .execute(&self.pool)
            .await?;

        let user = self.fetch_user_by_id(user_id).await?;
        self.complete_login(user).await
    }

    // Logout — revoke refresh token
    pub async fn logout(&self, raw_token: &str) -> Result<(), AuthError> {
        let token_hash = sha256_hex(raw_token);
        sqlx::query("UPDATE refresh_tokens SET revoked = TRUE WHERE token_hash = $1")
            .bind(&token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // Internal helpers

    async fn complete_login(&self, user: User) -> Result<LoginResponse, AuthError> {
        // Update last_login_at
        sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
            .bind(user.id)
            .execute(&self.pool)
            .await?;

        let (access_token, expiry_secs) =
            issue_access_token(&user).map_err(|e| AuthError::Internal(e))?;

        let refresh_token = self.issue_refresh_token(user.id).await?;

        // role-based redirect path (all admin sub-roles share the admin dashboard)
        let redirect_to = match user.role {
            UserRole::SuperAdmin => "/dashboard/super-admin",
            UserRole::OperationsAdmin
            | UserRole::VerificationAdmin
            | UserRole::FinanceAdmin => "/dashboard/admin",
            UserRole::HospitalAdmin => "/dashboard/hospital",
            UserRole::HealthWorker => "/dashboard/staff",
        }
        .to_string();

        Ok(LoginResponse {
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: expiry_secs,
            redirect_to,
            user: UserResponse::from(user),
        })
    }

    async fn issue_refresh_token(&self, user_id: Uuid) -> Result<String, AuthError> {
        let raw = Uuid::new_v4().to_string();
        let token_hash = sha256_hex(&raw);
        let days: i64 = std::env::var("JWT_REFRESH_EXPIRY_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let expires_at = Utc::now() + Duration::days(days);

        sqlx::query(
            "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
        )
        .bind(user_id)
        .bind(&token_hash)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(raw)
    }

    async fn fetch_user_by_email(&self, email: &str) -> Result<User, AuthError> {
        sqlx::query_as(
            "SELECT id, hospital_id, first_name, last_name, email, phone, password_hash,
                    role, role_label, avatar_url, is_active, last_login_at, created_at, updated_at
             FROM users WHERE email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::NotFound)
    }

    pub async fn fetch_user_by_id(&self, id: Uuid) -> Result<User, AuthError> {
        sqlx::query_as(
            "SELECT id, hospital_id, first_name, last_name, email, phone, password_hash,
                    role, role_label, avatar_url, is_active, last_login_at, created_at, updated_at
             FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::NotFound)
    }
}

// Pure functions

fn generate_otp() -> String {
    use rand::Rng;
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000))
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

fn issue_access_token(user: &User) -> Result<(String, u64), String> {
    crate::utils::jwt::issue_access_token(
        user.id,
        &user.email,
        user.role.clone(),
        user.hospital_id.map(|id| id.to_string()),
    )
}

pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    use argon2::{
        password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
        Argon2,
    };
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> Result<bool, argon2::password_hash::Error> {
    use argon2::{
        password_hash::{PasswordHash, PasswordVerifier},
        Argon2,
    };
    let parsed = PasswordHash::new(hash)?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otp_is_six_numeric_digits() {
        for _ in 0..20 {
            let code = generate_otp();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
            let n: u32 = code.parse().unwrap();
            assert!(n < 1_000_000);
        }
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex("hello"), sha256_hex("hello"));
        assert_ne!(sha256_hex("hello"), sha256_hex("world"));
        assert_eq!(sha256_hex("hello").len(), 64); // 32 bytes = 64 hex chars
    }

    #[test]
    fn password_hash_and_verify_roundtrip() {
        let hash = hash_password("SecurePass123!").unwrap();
        assert!(verify_password("SecurePass123!", &hash).unwrap());
        assert!(!verify_password("WrongPassword", &hash).unwrap());
    }

    #[test]
    fn different_passwords_produce_different_hashes() {
        let h1 = hash_password("password1").unwrap();
        let h2 = hash_password("password1").unwrap(); // same input, different salt
        assert_ne!(h1, h2);
    }
}

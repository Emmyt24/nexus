use std::sync::Arc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::repositories::identity_verification::{
    IdentityRepoError, IdentityVerificationRepository,
};
use crate::services::encryption::EncryptionService;
use crate::services::safehaven::{SafeHavenClient, SafeHavenError};

/// Deterministic salted SHA-256 of a raw BVN/NIN, hex-encoded. The salt
/// blocks trivial rainbow-table attacks against 11-digit inputs.
fn number_hash(number: &str) -> String {
    let salt = std::env::var("ENCRYPTION_SALT").unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(number.as_bytes());
    hex::encode(hasher.finalize())
}

/// Who an identity verification belongs to.
#[derive(Debug, Clone, Copy)]
pub enum IdentityOwner {
    Hospital,
    Clinician,
}

impl IdentityOwner {
    fn as_str(self) -> &'static str {
        match self {
            IdentityOwner::Hospital => "hospital",
            IdentityOwner::Clinician => "clinician",
        }
    }
}

/// BVN or NIN.
#[derive(Debug, Clone, Copy)]
pub enum IdentityKind {
    Bvn,
    Nin,
}

impl IdentityKind {
    /// DB enum value (lowercase).
    fn as_db(self) -> &'static str {
        match self {
            IdentityKind::Bvn => "bvn",
            IdentityKind::Nin => "nin",
        }
    }

    /// SafeHaven `type` value (uppercase).
    fn as_provider(self) -> &'static str {
        match self {
            IdentityKind::Bvn => "BVN",
            IdentityKind::Nin => "NIN",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "BVN" => Some(IdentityKind::Bvn),
            "NIN" => Some(IdentityKind::Nin),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Verification has not been initiated for this identity")]
    NotInitiated,
    #[error("This BVN/NIN has already been verified for another account")]
    NumberAlreadyInUse,
    #[error("Payment provider error: {0}")]
    Provider(#[from] SafeHavenError),
    #[error("Encryption error: {0}")]
    Encryption(String),
    #[error("Database error: {0}")]
    Database(#[from] IdentityRepoError),
}

pub struct IdentityVerificationService {
    safehaven: Arc<SafeHavenClient>,
    encryption: Arc<EncryptionService>,
    repo: Arc<IdentityVerificationRepository>,
}

impl IdentityVerificationService {
    pub fn new(
        safehaven: Arc<SafeHavenClient>,
        encryption: Arc<EncryptionService>,
        repo: Arc<IdentityVerificationRepository>,
    ) -> Self {
        Self {
            safehaven,
            encryption,
            repo,
        }
    }

    /// Initiate BVN/NIN verification: calls SafeHaven (which sends an OTP),
    /// encrypts the number, and stores a pending row. Rejects the request if
    /// the same number is already verified against a different owner.
    pub async fn initiate(
        &self,
        owner: IdentityOwner,
        owner_id: Uuid,
        id_type: IdentityKind,
        number: &str,
    ) -> Result<(), IdentityError> {
        let number = number.trim();
        if number.len() != 11 || !number.chars().all(|c| c.is_ascii_digit()) {
            return Err(IdentityError::Validation(
                "BVN/NIN must be 11 digits".to_string(),
            ));
        }

        let hash = number_hash(number);

        if self
            .repo
            .is_number_taken(id_type.as_db(), &hash, owner.as_str(), owner_id)
            .await?
        {
            return Err(IdentityError::NumberAlreadyInUse);
        }

        let identity_id = self
            .safehaven
            .initiate_identity_verification(id_type.as_provider(), number)
            .await?;

        let encrypted = self
            .encryption
            .encrypt_token(number)
            .map_err(|e| IdentityError::Encryption(e.to_string()))?;

        self.repo
            .upsert_pending(
                owner.as_str(),
                owner_id,
                id_type.as_db(),
                &encrypted,
                &hash,
                &identity_id,
            )
            .await?;

        Ok(())
    }

    /// Validate the OTP for a previously initiated verification.
    pub async fn validate(
        &self,
        owner: IdentityOwner,
        owner_id: Uuid,
        id_type: IdentityKind,
        otp: &str,
    ) -> Result<(), IdentityError> {
        let row = self
            .repo
            .get(owner.as_str(), owner_id, id_type.as_db())
            .await?
            .ok_or(IdentityError::NotInitiated)?;
        let identity_id = row
            .provider_identity_id
            .ok_or(IdentityError::NotInitiated)?;

        // Guard the race: another owner may have completed verification of
        // the same number while this row sat as `pending`. Decrypt our copy
        // and re-check before spending the OTP against SafeHaven.
        let raw = self
            .encryption
            .decrypt_token(&row.identity_number)
            .map_err(|e| IdentityError::Encryption(e.to_string()))?;
        let hash = number_hash(&raw);
        if self
            .repo
            .is_number_taken(id_type.as_db(), &hash, owner.as_str(), owner_id)
            .await?
        {
            return Err(IdentityError::NumberAlreadyInUse);
        }

        let payload = self
            .safehaven
            .validate_identity_verification(&identity_id, id_type.as_provider(), otp)
            .await?;

        self.repo
            .mark_verified(owner.as_str(), owner_id, id_type.as_db(), &payload)
            .await?;

        Ok(())
    }

    /// True if either BVN or NIN is verified for this owner (either suffices).
    pub async fn any_verified(
        &self,
        owner: IdentityOwner,
        owner_id: Uuid,
    ) -> Result<bool, IdentityError> {
        Ok(self.repo.any_verified(owner.as_str(), owner_id).await?)
    }

    /// Decrypt a stored, verified identity number (e.g. to pass a verified BVN
    /// into wallet sub-account provisioning). Returns None if not present.
    pub async fn decrypted_number(
        &self,
        owner: IdentityOwner,
        owner_id: Uuid,
        id_type: IdentityKind,
    ) -> Result<Option<String>, IdentityError> {
        Ok(self
            .verified_identity(owner, owner_id, id_type)
            .await?
            .map(|v| v.number))
    }

    /// Fetch a verified identity's decrypted number plus the SafeHaven
    /// `provider_identity_id` (the verification `_id`). SafeHaven's sub-account
    /// endpoint requires this `identityId` for BVN/NIN. Returns None unless the
    /// identity is verified and has a provider id.
    pub async fn verified_identity(
        &self,
        owner: IdentityOwner,
        owner_id: Uuid,
        id_type: IdentityKind,
    ) -> Result<Option<VerifiedIdentity>, IdentityError> {
        let row = self
            .repo
            .get(owner.as_str(), owner_id, id_type.as_db())
            .await?;
        match row {
            Some(r) if r.status == "verified" => {
                let number = self
                    .encryption
                    .decrypt_token(&r.identity_number)
                    .map_err(|e| IdentityError::Encryption(e.to_string()))?;
                Ok(Some(VerifiedIdentity {
                    number,
                    provider_identity_id: r.provider_identity_id,
                }))
            }
            _ => Ok(None),
        }
    }
}

/// A verified identity's decrypted number + SafeHaven verification id.
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    pub number: String,
    pub provider_identity_id: Option<String>,
}

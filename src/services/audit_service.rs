use std::sync::Arc;
use uuid::Uuid;

use crate::models::admin_registration::{AuditEntry, NewAuditEntry};
use crate::models::registration::{ActorType, AuditEventType, RegistrationStatus};
use crate::repositories::audit::{AuditError, AuditRepository};

#[derive(Debug, thiserror::Error)]
pub enum AuditServiceError {
    #[error("Audit logging failed: {0}")]
    LoggingFailed(#[from] AuditError),

    #[error("Serialization failed: {0}")]
    SerializationFailed(#[from] serde_json::Error),
}

/// Service for logging registration events to immutable audit trail
pub struct AuditService {
    audit_repo: Arc<AuditRepository>,
}

impl AuditService {
    pub fn new(audit_repo: Arc<AuditRepository>) -> Self {
        Self { audit_repo }
    }

    /// Log hospital registration event
    pub async fn log_registration(
        &self,
        hospital_id: Uuid,
        user_id: Option<Uuid>,
        details: RegistrationDetails,
    ) -> Result<AuditEntry, AuditServiceError> {
        let metadata = serde_json::to_value(&details)?;

        let entry = NewAuditEntry {
            hospital_id,
            event_type: AuditEventType::RegistrationCreated,
            actor_id: user_id,
            actor_type: ActorType::User,
            old_value: None,
            new_value: Some(metadata.clone()),
            metadata: Some(metadata),
        };

        let audit_entry = self.audit_repo.create(entry).await?;
        Ok(audit_entry)
    }

    /// Log status change event
    pub async fn log_status_change(
        &self,
        hospital_id: Uuid,
        admin_id: Option<Uuid>,
        old_status: RegistrationStatus,
        new_status: RegistrationStatus,
        reason: Option<String>,
    ) -> Result<AuditEntry, AuditServiceError> {
        let old_value = serde_json::to_value(&old_status)?;
        let new_value = serde_json::to_value(&new_status)?;

        let mut metadata = serde_json::json!({
            "old_status": old_status,
            "new_status": new_status,
        });

        if let Some(reason) = reason {
            metadata["reason"] = serde_json::Value::String(reason);
        }

        let entry = NewAuditEntry {
            hospital_id,
            event_type: AuditEventType::StatusChanged,
            actor_id: admin_id,
            actor_type: ActorType::Admin,
            old_value: Some(old_value),
            new_value: Some(new_value),
            metadata: Some(metadata),
        };

        let audit_entry = self.audit_repo.create(entry).await?;
        Ok(audit_entry)
    }

    /// Log document upload event
    pub async fn log_document_upload(
        &self,
        hospital_id: Uuid,
        document_id: Uuid,
        user_id: Uuid,
    ) -> Result<AuditEntry, AuditServiceError> {
        let metadata = serde_json::json!({
            "document_id": document_id,
        });

        let entry = NewAuditEntry {
            hospital_id,
            event_type: AuditEventType::DocumentUploaded,
            actor_id: Some(user_id),
            actor_type: ActorType::User,
            old_value: None,
            new_value: Some(metadata.clone()),
            metadata: Some(metadata),
        };

        let audit_entry = self.audit_repo.create(entry).await?;
        Ok(audit_entry)
    }

    /// Log payment method addition
    pub async fn log_payment_method_added(
        &self,
        hospital_id: Uuid,
        user_id: Uuid,
        payment_method_type: String,
    ) -> Result<AuditEntry, AuditServiceError> {
        let metadata = serde_json::json!({
            "payment_method_type": payment_method_type,
        });

        let entry = NewAuditEntry {
            hospital_id,
            event_type: AuditEventType::PaymentMethodAdded,
            actor_id: Some(user_id),
            actor_type: ActorType::User,
            old_value: None,
            new_value: Some(metadata.clone()),
            metadata: Some(metadata),
        };

        let audit_entry = self.audit_repo.create(entry).await?;
        Ok(audit_entry)
    }

    /// Log location update
    pub async fn log_location_updated(
        &self,
        hospital_id: Uuid,
        user_id: Uuid,
        latitude: f64,
        longitude: f64,
    ) -> Result<AuditEntry, AuditServiceError> {
        let metadata = serde_json::json!({
            "latitude": latitude,
            "longitude": longitude,
        });

        let entry = NewAuditEntry {
            hospital_id,
            event_type: AuditEventType::LocationUpdated,
            actor_id: Some(user_id),
            actor_type: ActorType::User,
            old_value: None,
            new_value: Some(metadata.clone()),
            metadata: Some(metadata),
        };

        let audit_entry = self.audit_repo.create(entry).await?;
        Ok(audit_entry)
    }

    /// Get complete audit trail for a hospital
    pub async fn get_audit_trail(
        &self,
        hospital_id: Uuid,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, AuditServiceError> {
        let entries = self
            .audit_repo
            .find_by_hospital_id(hospital_id, limit)
            .await?;
        Ok(entries)
    }
}

/// Registration details for audit logging
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RegistrationDetails {
    pub hospital_name: String,
    pub email: String,
    pub registration_number: String,
}

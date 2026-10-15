use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

// Registration Status Enum (for hospital admin registration workflow)

/// Registration status for hospital admin registration workflow (AC-01 to AC-05)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "registration_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum RegistrationStatus {
    Pending,
    Approved,
    Rejected,
}

// Audit Event Types

#[derive(Debug, Clone, Copy, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "audit_event_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    RegistrationCreated,
    StatusChanged,
    DocumentUploaded,
    PaymentMethodAdded,
    LocationUpdated,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "actor_type", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ActorType {
    User,
    Admin,
    System,
}

// Enums

/// The specific type of legal document being uploaded (Step 2 — Legal Verification).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "document_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DocumentType {
    /// Valid hospital registration from the Ministry of Health (REQUIRED)
    OperationalLicense,
    /// Certification of clinical quality and safety protocols
    MedicalCertificateOfStandards,
    /// Proof of current tax status and commercial standing (TCC)
    TaxComplianceCertificate,
    /// CAC certificate of incorporation
    CacCertificate,
    /// Director / trustee identification document
    DirectorId,
    Other,
}

/// The issuing authority for a legal document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "issuing_authority", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum IssuingAuthority {
    MinistryOfHealthFederal,
    MinistryOfHealthState,
    NafdacFederal,
    CorporateAffairsCommission,
    FederalInlandRevenueService,
    Other,
}

/// Submission state of the legal verification step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "submission_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SubmissionStatus {
    /// Saved but not yet submitted for review
    Draft,
    /// Submitted — awaiting NexusCare compliance review (24-48 business hours)
    UnderReview,
    /// Approved by the compliance team
    Approved,
    /// Rejected — hospital must re-upload corrected documents
    Rejected,
}

// Hospital document

/// A legal document uploaded by a hospital during Step 2 (Legal Verification).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct HospitalDocument {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub document_type: DocumentType,

    /// Uploaded file URL (PDF, PNG, or JPG — max 10 MB)
    pub file_url: String,
    pub file_name: String,
    /// MIME type stored for validation / display
    pub file_mime_type: Option<String>,
    /// File size in bytes
    pub file_size_bytes: Option<i64>,

    // --- Credential metadata filled in by the hospital ---
    pub credential_number: Option<String>,
    pub expiry_date: Option<NaiveDate>,
    pub issuing_authority: Option<IssuingAuthority>,

    pub submission_status: SubmissionStatus,
    pub uploaded_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,

    // --- Compliance review fields (filled by NexusCare staff) ---
    pub reviewed_at: Option<DateTime<Utc>>,
    pub reviewed_by: Option<Uuid>,
    pub review_notes: Option<String>,
}

// Request / response types

/// Payload for uploading a single legal document (Step 2).
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct UploadDocumentRequest {
    pub document_type: DocumentType,

    #[validate(url(message = "file_url must be a valid URL"))]
    pub file_url: String,

    #[validate(length(min = 1, max = 255))]
    pub file_name: String,

    pub file_mime_type: Option<String>,
    pub file_size_bytes: Option<i64>,

    /// e.g. "HOSP-4829-X"
    #[validate(length(max = 100))]
    pub credential_number: Option<String>,

    pub expiry_date: Option<NaiveDate>,
    pub issuing_authority: Option<IssuingAuthority>,
}

/// Payload for saving the legal step as a draft or submitting for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitLegalStepRequest {
    /// true = submit for compliance review; false = save as draft
    pub submit: bool,
}

/// Compliance decision on a single document, written by platform staff.
///
/// `submission_status` is restricted to the two review outcomes plus
/// `under_review`: a reviewer cannot push a document back to `draft`, which is
/// the hospital's own state and would silently discard the review trail.
#[derive(Debug, Clone, Serialize, Deserialize, Validate, ToSchema)]
pub struct ReviewDocumentRequest {
    pub submission_status: SubmissionStatus,
    /// Required when rejecting — a rejection with no reason is unactionable for
    /// the hospital, which has to re-upload something.
    #[validate(length(max = 2000))]
    pub review_notes: Option<String>,
}

/// Response returned after a document upload or update.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HospitalDocumentResponse {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub document_type: DocumentType,
    pub file_url: String,
    pub file_name: String,
    pub file_mime_type: Option<String>,
    pub file_size_bytes: Option<i64>,
    pub credential_number: Option<String>,
    pub expiry_date: Option<NaiveDate>,
    pub issuing_authority: Option<IssuingAuthority>,
    pub submission_status: SubmissionStatus,
    pub uploaded_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub review_notes: Option<String>,
}

impl From<HospitalDocument> for HospitalDocumentResponse {
    fn from(d: HospitalDocument) -> Self {
        Self {
            id: d.id,
            hospital_id: d.hospital_id,
            document_type: d.document_type,
            file_url: d.file_url,
            file_name: d.file_name,
            file_mime_type: d.file_mime_type,
            file_size_bytes: d.file_size_bytes,
            credential_number: d.credential_number,
            expiry_date: d.expiry_date,
            issuing_authority: d.issuing_authority,
            submission_status: d.submission_status,
            uploaded_at: d.uploaded_at,
            updated_at: d.updated_at,
            reviewed_at: d.reviewed_at,
            review_notes: d.review_notes,
        }
    }
}

// Registration audit log

/// Immutable record of every registration step transition for a hospital.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct RegistrationAuditLog {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub previous_step: Option<String>,
    pub new_step: String,
    /// NULL when the system triggers the transition automatically
    pub changed_by: Option<Uuid>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
}

// Onboarding notifications

/// Channel through which a status-change notification is delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "notification_channel", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum NotificationChannel {
    Email,
    Sms,
    InApp,
}

/// The event that triggered a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "notification_event", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum NotificationEvent {
    /// Documents submitted — verification clock started (24-48h)
    DocumentsSubmitted,
    /// Compliance review completed — approved
    VerificationApproved,
    /// Compliance review completed — rejected (action required)
    VerificationRejected,
    /// Onboarding fully complete — access granted
    AccessGranted,
    /// A document is expiring within 30 days
    DocumentExpiryWarning,
}

/// Delivery status of a single notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "notification_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    Pending,
    Sent,
    Failed,
    Read,
}

/// A notification sent to a hospital contact when their onboarding status changes.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OnboardingNotification {
    pub id: Uuid,
    pub hospital_id: Uuid,
    /// The user who should receive this notification (hospital admin)
    pub recipient_user_id: Option<Uuid>,
    pub channel: NotificationChannel,
    pub event: NotificationEvent,
    pub subject: Option<String>,
    pub body: String,
    pub status: NotificationStatus,
    pub sent_at: Option<DateTime<Utc>>,
    pub read_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Notification preferences stored per hospital.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct HospitalNotificationPreferences {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub email_enabled: bool,
    pub sms_enabled: bool,
    pub in_app_enabled: bool,
    /// Phone number to use for SMS (may differ from the main hospital phone)
    pub sms_phone_number: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// Payload for updating notification preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateNotificationPreferencesRequest {
    pub email_enabled: Option<bool>,
    pub sms_enabled: Option<bool>,
    pub in_app_enabled: Option<bool>,
    pub sms_phone_number: Option<String>,
}

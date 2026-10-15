//! Per-patient handover notes, the consult waiting room, and companion-device
//! handoffs.
//!
//! The doctor's clinical note is **not** here — it lives in
//! `crate::models::consultation_note`, which `20240063` extended with
//! `shift_id` / `session_id` and the locking columns. A second table for the
//! same notes about the same patient would be a data-integrity problem.
//!
//! Status columns on these tables are `TEXT` + `CHECK` rather than Postgres
//! enums (see `migrations/20240063_patient_records_and_consult_devices.sql`),
//! so the enums below map to `TEXT` and every new value stays a plain
//! `ALTER TABLE`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

use crate::models::consultation_note::ConsultationNote;
use crate::models::patient::NewPatientRequest;

// Enums

/// Where a queued patient is in the consultation. `Removed` is a withdrawal by
/// the hospital, not a clinical outcome — kept rather than deleted so the queue
/// stays auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum QueueEntryState {
    /// In the waiting room, not yet called.
    Waiting,
    /// With the doctor right now. At most one per session, enforced by
    /// `uq_consult_queue_one_in_consult`.
    InConsult,
    /// Seen and released.
    Seen,
    /// Pulled from the queue by the hospital without being seen.
    Removed,
}

impl QueueEntryState {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueEntryState::Waiting => "waiting",
            QueueEntryState::InConsult => "in_consult",
            QueueEntryState::Seen => "seen",
            QueueEntryState::Removed => "removed",
        }
    }
}

// Rows

/// One `(session, patient)` queue entry.
#[derive(Debug, Clone, FromRow)]
pub struct ConsultQueueEntry {
    pub id: Uuid,
    pub session_id: Uuid,
    pub shift_id: Option<Uuid>,
    pub patient_id: Uuid,
    pub hospital_id: Uuid,
    pub state: QueueEntryState,
    pub position: i32,
    pub added_by: Uuid,
    pub called_at: Option<DateTime<Utc>>,
    pub seen_at: Option<DateTime<Utc>>,
    pub removed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A queue entry joined to the patient's triage data and to whether each note
/// has been filed, so the waiting-room screen needs no per-patient follow-up
/// request.
#[derive(Debug, Clone, FromRow)]
pub struct ConsultQueueEntryWithPatient {
    pub id: Uuid,
    pub session_id: Uuid,
    pub patient_id: Uuid,
    pub state: QueueEntryState,
    pub position: i32,
    pub called_at: Option<DateTime<Utc>>,
    pub seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub full_name: String,
    pub age: f32,
    pub gender: String,
    pub severity_level: String,
    pub predictive_risk_score: Option<f32>,
    /// `true` once this patient has at least one consultation note on this shift.
    pub has_clinical_note: bool,
    /// `true` once the on-site party has filed this patient's handover note.
    pub has_handover_note: bool,
}

/// The on-site hospital party's note. Exactly one per `(shift, patient)`.
#[derive(Debug, Clone, FromRow)]
pub struct PatientHandoverNote {
    pub id: Uuid,
    pub shift_id: Uuid,
    pub patient_id: Uuid,
    pub session_id: Option<Uuid>,
    pub hospital_id: Uuid,
    pub author_user_id: Uuid,

    pub summary: String,
    pub outstanding_tasks: serde_json::Value,
    pub medications_given: serde_json::Value,
    pub escalation_required: bool,
    pub escalation_reason: Option<String>,

    pub submitted_at: DateTime<Utc>,
    pub editable_until: DateTime<Utc>,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single-use grant for a companion device. The plaintext code is never
/// stored — only `code_hash` — so this row cannot be replayed into a token.
#[derive(Debug, Clone, FromRow)]
pub struct ConsultDeviceHandoff {
    pub id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub code_hash: String,
    pub device_ordinal: i32,
    pub participant_role: crate::models::video_session::ParticipantRole,
    pub mode: crate::models::video_session::JoinMode,
    pub device_label: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub redeemed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

// Request DTOs

/// `POST /api/v1/shifts/{shift_id}/consult/queue`.
///
/// Supply **exactly one** of `patient_id` (an existing patient of this
/// hospital) or `patient` (intake details for someone not yet on file).
/// Neither or both is a validation error. Creating a patient through
/// `patient` also runs the ML triage pipeline, identically to standalone
/// intake.
///
/// Exactly one of `patient_id` (queue someone already on file) or `patient`
/// (intake a new one and queue them in the same transaction) must be set. That
/// is a conditional rule, so it is enforced in the service rather than by
/// `validator`, matching how `ShiftService::validate_request` handles the
/// `pay_type`-dependent compensation fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate, ToSchema)]
pub struct AddPatientToQueueRequest {
    /// An existing patient of this hospital. Supply this **or** `patient`,
    /// never both and never neither.
    #[serde(default)]
    pub patient_id: Option<Uuid>,
    #[serde(default)]
    #[validate(nested)]
    /// Intake details for a patient who is not on file yet. Creating them here
    /// also runs the ML triage pipeline, exactly as the standalone intake does.
    pub patient: Option<NewPatientRequest>,
}

/// `PUT /api/v1/shifts/{shift_id}/patients/{patient_id}/handover-note`.
/// Idempotent: a second submit inside the edit window updates the single row
/// for this `(shift, patient)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate, ToSchema)]
pub struct SubmitPatientHandoverNoteRequest {
    /// What the on-site party is handing over about this patient. Required.
    #[validate(length(min = 1, max = 10000, message = "A handover summary is required"))]
    pub summary: String,

    /// Free-form JSON items still to be done for this patient.
    #[serde(default)]
    pub outstanding_tasks: Vec<serde_json::Value>,

    /// Free-form JSON records of what was administered during the shift.
    #[serde(default)]
    pub medications_given: Vec<serde_json::Value>,

    /// Flags this patient as needing escalation. When `true`,
    /// `escalation_reason` must be non-blank or the request is rejected.
    #[serde(default)]
    pub escalation_required: bool,

    /// Why escalation is needed. Required whenever `escalation_required` is
    /// `true`; a blank reason is a validation error, not a silent accept.
    #[validate(length(max = 2000))]
    #[serde(default)]
    pub escalation_reason: Option<String>,
}

/// `POST /api/v1/shifts/{shift_id}/consult/handoff` — every field optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct CreateHandoffRequest {
    /// Free text shown beside the device in the participant list, e.g. "iPhone".
    #[serde(default)]
    pub device_label: Option<String>,
}

/// `POST /api/v1/consult/handoff/redeem` — unauthenticated; the code is the
/// credential.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate, ToSchema)]
pub struct RedeemHandoffRequest {
    /// The single-use code, read from the `#c=` fragment of `handoff_url`.
    /// Valid for 180 seconds and good for exactly one redemption.
    #[validate(length(min = 1, max = 128, message = "A handoff code is required"))]
    pub code: String,
}

// Response DTOs

/// One patient in the waiting room. Carries no clinical detail beyond triage —
/// the notes themselves are fetched per patient.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConsultQueuePatientView {
    /// The queue entry's own id. Use this — not `patient_id` — on the
    /// `/call`, `/seen` and `DELETE` routes.
    pub entry_id: Uuid,
    /// The patient this entry refers to.
    pub patient_id: Uuid,
    /// The patient's full name. Absent for platform admins, who never receive
    /// this list at all.
    pub full_name: String,
    /// Years, as a float — the ML triage model's input type.
    pub age: f32,
    /// As recorded at intake.
    pub gender: String,
    /// Triage severity recorded at intake.
    pub severity_level: String,
    /// From the ML triage pipeline; `null` until the prediction completes.
    pub predictive_risk_score: Option<f32>,
    /// `waiting` -> `in_consult` -> `seen`, or `removed`.
    pub state: QueueEntryState,
    /// Queue order, ascending. Not unique and not renumbered on removal, so
    /// sort by it rather than treating it as an index.
    pub position: i32,
    /// When the clinician called this patient in; `null` while `waiting`.
    pub called_at: Option<DateTime<Utc>>,
    /// When the consultation finished; `null` until `seen`.
    pub seen_at: Option<DateTime<Utc>>,
    /// When the patient was added to the waiting room.
    pub queued_at: DateTime<Utc>,
    /// Whether the doctor has filed a clinical note, so the list needs no
    /// per-patient follow-up request.
    pub has_clinical_note: bool,
    /// Whether the hospital-side party has filed a handover note.
    pub has_handover_note: bool,
}

/// The waiting-room block on `GET /consult` and `GET /consult/queue`.
///
/// `patients` is empty for platform admins, who get the counts but never
/// patient identities — the same NDPR line `authorize_shift_access` already
/// draws for the video stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ConsultWaitingRoomView {
    /// Patients queued and not yet called. This is the waiting-room badge.
    pub waiting: i64,
    /// Patients currently with the doctor. At most `1` per session.
    pub in_consult: i64,
    /// Patients whose consultation has finished.
    pub seen: i64,
    /// Everything ever queued for this session, `removed` included. The three
    /// live counts above do not sum to it.
    pub total: i64,
    /// Empty for platform admins. Render counts from the fields above, never
    /// from this array's length.
    pub patients: Vec<ConsultQueuePatientView>,
}

/// A per-patient handover note as returned to an entitled reader.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PatientHandoverNoteView {
    /// The note's own id.
    pub id: Uuid,
    /// The shift this note belongs to.
    pub shift_id: Uuid,
    /// The patient this note is about.
    pub patient_id: Uuid,
    /// The hospital-side author, resolved from their token — never accepted
    /// from the request body.
    pub author_user_id: Uuid,
    /// Display name for the author; `null` if it could not be resolved.
    pub author_name: Option<String>,
    /// The handover narrative for this patient.
    pub summary: String,
    /// The JSON array submitted as `outstanding_tasks`.
    pub outstanding_tasks: serde_json::Value,
    /// The JSON array submitted as `medications_given`.
    pub medications_given: serde_json::Value,
    /// Whether this patient was flagged as needing escalation.
    pub escalation_required: bool,
    /// Why escalation is needed; `null` unless `escalation_required`.
    pub escalation_reason: Option<String>,
    /// When the note was last submitted, not when the row was created.
    pub submitted_at: DateTime<Utc>,
    /// `false` once `editable_until` has passed.
    pub editable: bool,
    /// One hour after the note was first filed. A later submit is a 409.
    pub editable_until: DateTime<Utc>,
}

/// One patient's complete record for a shift.
///
/// `queue_state` is `null` when the patient was never queued for this shift —
/// a clinical record outlives its queue entry. `handover_note` is `null` until
/// the hospital-side party files one.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PatientRecordView {
    /// The patient these records are about.
    pub patient_id: Uuid,
    /// The patient's full name.
    pub full_name: String,
    /// Years, as a float — the ML triage model's input type.
    pub age: f32,
    /// As recorded at intake.
    pub gender: String,
    /// Triage severity recorded at intake.
    pub severity_level: String,
    /// `null` when the patient was never queued for this shift — a clinical
    /// record outlives its queue entry.
    pub queue_state: Option<QueueEntryState>,
    /// Every note the doctor filed for this patient on this shift, newest
    /// first. Amendments appear as their own entries, linked by
    /// `amends_note_id`.
    pub clinical_notes: Vec<ConsultationNote>,
    /// The hospital-side note, if one was filed.
    pub handover_note: Option<PatientHandoverNoteView>,
}

/// `GET /api/v1/shifts/{shift_id}/patient-records` — the hospital's read of
/// everything a shift produced. Remains available after the shift completes.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShiftPatientRecordsView {
    /// The shift these records belong to.
    pub shift_id: Uuid,
    /// Lets the UI tell a live shift from a final record.
    pub shift_status: String,
    /// `null` until the shift ends.
    pub shift_ended_at: Option<DateTime<Utc>>,
    /// Every patient this shift touched, via the queue, a clinical note or a
    /// handover note.
    pub patients_total: i64,
    /// How many of them reached the `seen` state.
    pub patients_seen: i64,
    /// Empty for platform admins, who receive counts only.
    pub patients: Vec<PatientRecordView>,
    /// `true` when the caller was served counts without clinical content.
    pub metadata_only: bool,
}

/// `200` from `POST /consult/handoff`.
///
/// `handoff_url` carries the code in the URL **fragment**, which browsers never
/// send to the server — so the credential stays out of access logs, out of any
/// `Referer`, and out of the proxy chain. Show it as a QR code or hand it to
/// the user's own device; it is single-use and short-lived.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateHandoffResponse {
    /// The consultation the companion device will join.
    pub session_id: Uuid,
    /// Open this on the second device. The code sits in the `#c=` fragment.
    pub handoff_url: String,
    /// The plaintext code, returned exactly once. Only its hash is stored.
    pub code: String,
    /// The slot the companion device will occupy: `2` for the first phone,
    /// `3` for the next. Slot `1` is always the primary device.
    pub device_ordinal: i32,
    /// 180 seconds out. After this the code is dead and a new one is needed.
    pub expires_at: DateTime<Utc>,
}

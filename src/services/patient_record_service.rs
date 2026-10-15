//! The consult waiting room, per-patient handover notes, and the hospital's
//! per-shift roll-up.
//!
//! Two rules shape this module.
//!
//! **One tenant boundary.** Authorization delegates to
//! `VideoService::authorize_shift_data_access` rather than being re-derived
//! here, so clinical-record access and room access cannot drift apart. What
//! this module adds is only ever a *narrowing*: who may write, as opposed to
//! who may look.
//!
//! **Who the actor is comes from the token.** No request DTO carries a
//! `clinician_id`, `author_user_id` or `hospital_id` — they are resolved from
//! `Claims` and from the shift. This is the rule commit bc63001 established for
//! the interest and apply flows, and for the same reason: accepting an actor id
//! from the client is an IDOR.

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::models::consultation_note::ConsultationNote;
use crate::models::patient_record::{
    AddPatientToQueueRequest, ConsultQueueEntry, ConsultWaitingRoomView, PatientHandoverNote,
    PatientHandoverNoteView, PatientRecordView, QueueEntryState, ShiftPatientRecordsView,
    SubmitPatientHandoverNoteRequest,
};
use crate::models::shift::Shift;
use crate::models::user::Claims;
use crate::models::video_session::{VideoSession, VideoSessionStatus};
use crate::repositories::patient::PatientRepository;
use crate::repositories::patient_record::PatientRecordRepository;
use crate::repositories::video_session::VideoSessionRepository;
use crate::services::consultation_note_service::ConsultationNoteService;
use crate::services::patient_prediction_service::PatientPredictionService;
use crate::services::video_service::{ClinicalAccess, ShiftDataRole, VideoService};

/// A distinct variant per failure, rather than one shared `NotAuthorized`. The
/// shifts domain reuses a single variant across eight ownership checks, which
/// is why a 403 from `approve_handover` reports "Not authorized to view
/// applications".
#[derive(Debug, thiserror::Error)]
pub enum PatientRecordServiceError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Validation failed: {0}")]
    Validation(String),

    #[error("Shift not found: {0}")]
    ShiftNotFound(Uuid),

    #[error("Patient not found: {0}")]
    PatientNotFound(Uuid),

    #[error("No consultation has been started for this shift")]
    SessionNotFound,

    #[error("This consultation has already ended")]
    SessionEnded,

    #[error("Queue entry not found: {0}")]
    QueueEntryNotFound(Uuid),

    #[error("This patient is already in the queue for this consultation")]
    PatientAlreadyQueued,

    #[error("Another patient is already in consultation")]
    AnotherPatientInConsult,

    #[error("Cannot move a queue entry from {from} to {to}")]
    IllegalQueueTransition { from: String, to: String },

    #[error("No handover note has been filed for this patient")]
    HandoverNoteNotFound,

    #[error("The edit window for this handover note has closed")]
    HandoverEditWindowClosed,

    /// Only the shift's assigned clinician may call patients in or release them.
    #[error("Only the clinician assigned to this shift can do this")]
    NotTheAssignedClinician,

    /// Only an admin of the owning hospital may run the queue or file a
    /// per-patient handover note — that is the on-site party.
    #[error("Only an admin of this shift's hospital can do this")]
    NotTheOwningHospital,

    #[error("Not authorized to access this shift's patient records")]
    NotAuthorized,

    #[error("Authenticated user has no clinician profile")]
    NoClinicianProfile,

    #[error("Patient belongs to a different hospital")]
    PatientHospitalMismatch,
}

impl From<crate::services::video_service::VideoServiceError> for PatientRecordServiceError {
    fn from(e: crate::services::video_service::VideoServiceError) -> Self {
        use crate::services::video_service::VideoServiceError as V;
        match e {
            V::Database(e) => PatientRecordServiceError::Database(e),
            V::ShiftNotFound(id) => PatientRecordServiceError::ShiftNotFound(id),
            V::SessionNotFound => PatientRecordServiceError::SessionNotFound,
            V::SessionEnded => PatientRecordServiceError::SessionEnded,
            V::NoClinicianProfile => PatientRecordServiceError::NoClinicianProfile,
            _ => PatientRecordServiceError::NotAuthorized,
        }
    }
}

impl From<crate::services::consultation_note_service::ConsultationNoteError>
    for PatientRecordServiceError
{
    fn from(e: crate::services::consultation_note_service::ConsultationNoteError) -> Self {
        use crate::services::consultation_note_service::ConsultationNoteError as C;
        match e {
            C::Database(e) => PatientRecordServiceError::Database(e),
            C::PatientNotFound(id) => PatientRecordServiceError::PatientNotFound(id),
            other => PatientRecordServiceError::Validation(other.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, PatientRecordServiceError>;

pub struct PatientRecordService {
    repo: Arc<PatientRecordRepository>,
    video_repo: Arc<VideoSessionRepository>,
    /// Holds the one tenant boundary. Never re-implemented here.
    video_service: Arc<VideoService>,
    /// Owns `consultation_notes`; the roll-up reads clinical notes through it
    /// rather than reaching into that table directly.
    notes: Arc<ConsultationNoteService>,
    patient_repo: Arc<PatientRepository>,
    prediction_service: Arc<PatientPredictionService>,
    pool: sqlx::PgPool,
}

impl PatientRecordService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<PatientRecordRepository>,
        video_repo: Arc<VideoSessionRepository>,
        video_service: Arc<VideoService>,
        notes: Arc<ConsultationNoteService>,
        patient_repo: Arc<PatientRepository>,
        prediction_service: Arc<PatientPredictionService>,
        pool: sqlx::PgPool,
    ) -> Self {
        Self {
            repo,
            video_repo,
            video_service,
            notes,
            patient_repo,
            prediction_service,
            pool,
        }
    }

    // Shared preamble

    /// Load the shift and settle who the caller is to it, in one place.
    async fn shift_and_role(
        &self,
        shift_id: Uuid,
        claims: &Claims,
    ) -> Result<(Shift, ShiftDataRole)> {
        let shift = self.video_service.load_shift_for_authz(shift_id).await?;
        let role = self
            .video_service
            .authorize_shift_data_access(&shift, claims)
            .await?;
        Ok((shift, role))
    }

    /// The live session for a shift, required for anything session-scoped.
    async fn live_session(&self, shift_id: Uuid) -> Result<VideoSession> {
        let session = self
            .video_repo
            .find_by_shift(shift_id)
            .await?
            .ok_or(PatientRecordServiceError::SessionNotFound)?;
        if session.status == VideoSessionStatus::Ended {
            return Err(PatientRecordServiceError::SessionEnded);
        }
        Ok(session)
    }

    // Waiting room

    /// Add a patient to a live consultation's waiting room.
    ///
    /// Either queues someone already on file, or intakes a new patient and
    /// queues them. The new-patient path puts the patient row, its pending ML
    /// prediction and the queue entry in **one** transaction, so a failure
    /// cannot leave an un-queued patient or an un-triaged queue entry behind.
    pub async fn add_patient_to_queue(
        &self,
        shift_id: Uuid,
        claims: &Claims,
        request: AddPatientToQueueRequest,
    ) -> Result<ConsultQueueEntry> {
        let (shift, role) = self.shift_and_role(shift_id, claims).await?;
        // Running the queue is the hospital's job: they are the ones with the
        // patient physically in front of them.
        if role != ShiftDataRole::HospitalAdmin {
            return Err(PatientRecordServiceError::NotTheOwningHospital);
        }

        let added_by = claims_user_id(claims)?;
        let session = self.live_session(shift_id).await?;

        // Conditional, so it lives here rather than in a `#[validate]`.
        let (patient_id, new_patient) = match (request.patient_id, request.patient) {
            (Some(id), None) => (Some(id), None),
            (None, Some(p)) => (None, Some(p)),
            (Some(_), Some(_)) => {
                return Err(PatientRecordServiceError::Validation(
                    "Send either patient_id or patient, not both".to_string(),
                ))
            }
            (None, None) => {
                return Err(PatientRecordServiceError::Validation(
                    "Send either patient_id (an existing patient) or patient (a new intake)"
                        .to_string(),
                ))
            }
        };

        // An existing patient must belong to this hospital. Checked before the
        // transaction opens so the failure is cheap.
        if let Some(id) = patient_id {
            let patient = self
                .patient_repo
                .find_by_id(id)
                .await
                .map_err(|e| PatientRecordServiceError::Validation(e.to_string()))?
                .ok_or(PatientRecordServiceError::PatientNotFound(id))?;
            if patient.hospital_id != shift.hospital_id {
                return Err(PatientRecordServiceError::PatientHospitalMismatch);
            }
            if self
                .repo
                .find_entry_for_patient(session.id, id)
                .await?
                .is_some()
            {
                return Err(PatientRecordServiceError::PatientAlreadyQueued);
            }
        }

        let mut tx = self.pool.begin().await?;

        let patient_id = match (patient_id, new_patient) {
            (Some(id), _) => id,
            (None, Some(req)) => {
                let (patient, _prediction) = self
                    .prediction_service
                    .ingest_patient_in_tx(&mut tx, shift.hospital_id, added_by, req)
                    .await
                    .map_err(|e| PatientRecordServiceError::Validation(e.to_string()))?;
                patient.id
            }
            (None, None) => unreachable!("validated above"),
        };

        let entry = self
            .repo
            .enqueue_tx(
                &mut tx,
                session.id,
                session.shift_id,
                patient_id,
                shift.hospital_id,
                added_by,
            )
            .await
            .map_err(map_unique_violation)?;

        tx.commit().await?;

        Ok(entry)
    }

    pub async fn waiting_room(
        &self,
        shift_id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultWaitingRoomView> {
        let (_shift, role) = self.shift_and_role(shift_id, claims).await?;
        let session = self
            .video_repo
            .find_by_shift(shift_id)
            .await?
            .ok_or(PatientRecordServiceError::SessionNotFound)?;

        Ok(self
            .video_service
            .waiting_room_view(session.id, role.clinical_access().is_full())
            .await?)
    }

    /// Call the next patient in. The partial unique index on
    /// `(session_id) WHERE state = 'in_consult'` is what guarantees the doctor
    /// is only ever with one patient — a second call surfaces as
    /// `AnotherPatientInConsult` rather than silently queue-jumping.
    pub async fn call_patient(
        &self,
        shift_id: Uuid,
        entry_id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultQueueEntry> {
        self.clinician_queue_transition(
            shift_id,
            entry_id,
            claims,
            QueueEntryState::Waiting,
            QueueEntryState::InConsult,
        )
        .await
    }

    /// Mark the current patient seen and release the in-consult slot.
    pub async fn mark_patient_seen(
        &self,
        shift_id: Uuid,
        entry_id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultQueueEntry> {
        self.clinician_queue_transition(
            shift_id,
            entry_id,
            claims,
            QueueEntryState::InConsult,
            QueueEntryState::Seen,
        )
        .await
    }

    async fn clinician_queue_transition(
        &self,
        shift_id: Uuid,
        entry_id: Uuid,
        claims: &Claims,
        from: QueueEntryState,
        to: QueueEntryState,
    ) -> Result<ConsultQueueEntry> {
        let (_shift, role) = self.shift_and_role(shift_id, claims).await?;
        // Calling and releasing a patient is the attending doctor's act.
        if !matches!(role, ShiftDataRole::Clinician(_)) {
            return Err(PatientRecordServiceError::NotTheAssignedClinician);
        }

        let entry = self.entry_on_shift(shift_id, entry_id).await?;

        self.repo
            .transition_state(entry.id, from, to)
            .await
            .map_err(map_unique_violation)?
            .ok_or(PatientRecordServiceError::IllegalQueueTransition {
                from: entry.state.as_str().to_string(),
                to: to.as_str().to_string(),
            })
    }

    /// Withdraw a patient from the queue without seeing them.
    pub async fn remove_from_queue(
        &self,
        shift_id: Uuid,
        entry_id: Uuid,
        claims: &Claims,
    ) -> Result<ConsultQueueEntry> {
        let (_shift, role) = self.shift_and_role(shift_id, claims).await?;
        if role != ShiftDataRole::HospitalAdmin {
            return Err(PatientRecordServiceError::NotTheOwningHospital);
        }

        let entry = self.entry_on_shift(shift_id, entry_id).await?;
        self.repo
            .transition_state(entry.id, entry.state, QueueEntryState::Removed)
            .await?
            .ok_or(PatientRecordServiceError::IllegalQueueTransition {
                from: entry.state.as_str().to_string(),
                to: "removed".to_string(),
            })
    }

    /// Fetch an entry and confirm it really belongs to the shift in the path.
    /// Without this the shift id would be decorative, and a caller authorized
    /// on their own shift could mutate another shift's queue.
    async fn entry_on_shift(&self, shift_id: Uuid, entry_id: Uuid) -> Result<ConsultQueueEntry> {
        let entry = self
            .repo
            .find_entry(entry_id)
            .await?
            .ok_or(PatientRecordServiceError::QueueEntryNotFound(entry_id))?;
        if entry.shift_id != Some(shift_id) {
            return Err(PatientRecordServiceError::QueueEntryNotFound(entry_id));
        }
        Ok(entry)
    }

    // Clinical notes (read-only here; writes live in ConsultationNoteService)

    pub async fn list_clinical_notes(
        &self,
        shift_id: Uuid,
        patient_id: Uuid,
        claims: &Claims,
    ) -> Result<Vec<ConsultationNote>> {
        let (shift, role) = self.shift_and_role(shift_id, claims).await?;
        if role.clinical_access() != ClinicalAccess::Full {
            return Err(PatientRecordServiceError::NotAuthorized);
        }
        self.patient_on_shift(&shift, patient_id).await?;

        Ok(self
            .notes
            .list_for_shift_patient(shift_id, patient_id)
            .await?)
    }

    // Per-patient handover notes

    /// File (or refile) the on-site party's note for one patient. Idempotent on
    /// `(shift, patient)`; a refile inside the edit window updates the row.
    pub async fn submit_handover_note(
        &self,
        shift_id: Uuid,
        patient_id: Uuid,
        claims: &Claims,
        request: SubmitPatientHandoverNoteRequest,
    ) -> Result<PatientHandoverNote> {
        let (shift, role) = self.shift_and_role(shift_id, claims).await?;
        // Whoever is on site for this patient may file their handover note:
        // the hospital's own admin in the consult room, or the assigned
        // clinician — who *is* the on-site party on an in-person shift, and who
        // already authors the shift-level handover (`POST /shifts/{id}/handover`
        // is HealthWorker-gated). Restricting this to HospitalAdmin would have
        // left an in-person shift with nobody able to file one.
        //
        // `ShiftDataRole::Clinician` is only ever returned for the *assigned*
        // clinician — `authorize_shift_data_access` rejects every other health
        // worker before this point — so this does not widen the tenant boundary.
        match role {
            ShiftDataRole::HospitalAdmin | ShiftDataRole::Clinician(_) => {}
            // NDPR gives platform staff no lawful basis to author clinical
            // content, only to read counts and metadata.
            ShiftDataRole::PlatformAdmin => {
                return Err(PatientRecordServiceError::NotTheOwningHospital)
            }
        }

        if request.escalation_required
            && request
                .escalation_reason
                .as_deref()
                .map(|r| r.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(PatientRecordServiceError::Validation(
                "An escalation reason is required when escalation_required is true".to_string(),
            ));
        }

        let author_user_id = claims_user_id(claims)?;
        self.patient_on_shift(&shift, patient_id).await?;

        let session_id = self.video_service.session_id_for_shift(shift_id).await?;

        self.repo
            .upsert_handover_note(
                shift_id,
                patient_id,
                session_id,
                shift.hospital_id,
                author_user_id,
                &request.summary,
                &serde_json::Value::Array(request.outstanding_tasks),
                &serde_json::Value::Array(request.medications_given),
                request.escalation_required,
                request.escalation_reason.as_deref(),
            )
            .await?
            .ok_or(PatientRecordServiceError::HandoverEditWindowClosed)
    }

    pub async fn get_handover_note(
        &self,
        shift_id: Uuid,
        patient_id: Uuid,
        claims: &Claims,
    ) -> Result<PatientHandoverNoteView> {
        let (_shift, role) = self.shift_and_role(shift_id, claims).await?;
        if role.clinical_access() != ClinicalAccess::Full {
            return Err(PatientRecordServiceError::NotAuthorized);
        }

        let note = self
            .repo
            .find_handover_note(shift_id, patient_id)
            .await?
            .ok_or(PatientRecordServiceError::HandoverNoteNotFound)?;
        let names = self
            .author_name_map(std::iter::once(note.author_user_id))
            .await?;
        Ok(handover_view(note, &names))
    }

    // The hospital's roll-up

    /// Everything a shift produced, per patient.
    ///
    /// Deliberately **not** gated on the shift having ended: the hospital-side
    /// party authors the handover notes, so withholding them until completion
    /// would hide their own work from them. `shift_status` is returned so a
    /// client can tell a live shift from a final record.
    pub async fn shift_patient_records(
        &self,
        shift_id: Uuid,
        claims: &Claims,
    ) -> Result<ShiftPatientRecordsView> {
        let (shift, role) = self.shift_and_role(shift_id, claims).await?;
        let full = role.clinical_access() == ClinicalAccess::Full;

        let rows = self.repo.patients_touched_by_shift(shift_id).await?;
        let patients_total = rows.len() as i64;
        let patients_seen = rows
            .iter()
            .filter(|r| r.queue_state == Some(QueueEntryState::Seen))
            .count() as i64;

        let shift_status = format!("{:?}", shift.status).to_lowercase();

        // Platform staff stop here: counts, never content or identities.
        if !full {
            return Ok(ShiftPatientRecordsView {
                shift_id,
                shift_status,
                shift_ended_at: shift.actual_end,
                patients_total,
                patients_seen,
                patients: Vec::new(),
                metadata_only: true,
            });
        }

        // Two queries for the whole shift rather than two per patient.
        let all_notes = self.notes.list_for_shift(shift_id).await?;
        let all_handovers = self.repo.list_handover_notes_for_shift(shift_id).await?;
        let names = self
            .author_name_map(
                all_notes
                    .iter()
                    .map(|n| n.recorded_by)
                    .chain(all_handovers.iter().map(|h| h.author_user_id)),
            )
            .await?;

        let patients = rows
            .into_iter()
            .map(|r| PatientRecordView {
                patient_id: r.patient_id,
                full_name: r.full_name,
                age: r.age,
                gender: r.gender,
                severity_level: r.severity_level,
                queue_state: r.queue_state,
                clinical_notes: all_notes
                    .iter()
                    .filter(|n| n.patient_id == r.patient_id)
                    .cloned()
                    .collect(),
                handover_note: all_handovers
                    .iter()
                    .find(|h| h.patient_id == r.patient_id)
                    .cloned()
                    .map(|h| handover_view(h, &names)),
            })
            .collect();

        Ok(ShiftPatientRecordsView {
            shift_id,
            shift_status,
            shift_ended_at: shift.actual_end,
            patients_total,
            patients_seen,
            patients,
            metadata_only: false,
        })
    }

    // Helpers

    /// A patient's record may only be touched through a shift at their own
    /// hospital. Without this, an authorized clinician could reach any patient
    /// id in the system.
    async fn patient_on_shift(&self, shift: &Shift, patient_id: Uuid) -> Result<()> {
        let patient = self
            .patient_repo
            .find_by_id(patient_id)
            .await
            .map_err(|e| PatientRecordServiceError::Validation(e.to_string()))?
            .ok_or(PatientRecordServiceError::PatientNotFound(patient_id))?;
        if patient.hospital_id != shift.hospital_id {
            return Err(PatientRecordServiceError::PatientHospitalMismatch);
        }
        Ok(())
    }

    async fn author_name_map(
        &self,
        ids: impl Iterator<Item = Uuid>,
    ) -> Result<std::collections::HashMap<Uuid, String>> {
        let mut unique: Vec<Uuid> = ids.collect();
        unique.sort();
        unique.dedup();
        if unique.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        Ok(self.repo.author_names(&unique).await?.into_iter().collect())
    }
}

fn handover_view(
    h: PatientHandoverNote,
    names: &std::collections::HashMap<Uuid, String>,
) -> PatientHandoverNoteView {
    PatientHandoverNoteView {
        author_name: names.get(&h.author_user_id).cloned(),
        editable: h.editable_until > Utc::now(),
        id: h.id,
        shift_id: h.shift_id,
        patient_id: h.patient_id,
        author_user_id: h.author_user_id,
        summary: h.summary,
        outstanding_tasks: h.outstanding_tasks,
        medications_given: h.medications_given,
        escalation_required: h.escalation_required,
        escalation_reason: h.escalation_reason,
        submitted_at: h.submitted_at,
        editable_until: h.editable_until,
    }
}

/// Turn the queue's two uniqueness guards into errors a client can act on,
/// rather than a 500. They are enforced in the database on purpose — checking
/// first and inserting second would race.
fn map_unique_violation(e: sqlx::Error) -> PatientRecordServiceError {
    if let sqlx::Error::Database(ref db) = e {
        match db.constraint() {
            Some("uq_consult_queue_one_in_consult") => {
                return PatientRecordServiceError::AnotherPatientInConsult
            }
            Some("uq_consult_queue_patient") => {
                return PatientRecordServiceError::PatientAlreadyQueued
            }
            _ => {}
        }
    }
    PatientRecordServiceError::Database(e)
}

fn claims_user_id(claims: &Claims) -> Result<Uuid> {
    Uuid::parse_str(&claims.sub).map_err(|_| PatientRecordServiceError::NotAuthorized)
}

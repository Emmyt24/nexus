//! Legal verification documents for a hospital.
//!
//! Authorization lives here rather than at the route, because the route guard
//! can see that a caller is a `hospital_admin` but not *which* hospital's
//! documents the path refers to. Every method that takes a `hospital_id` from
//! the path checks it against `Claims` before touching a row — a licence file
//! and a director's ID are exactly the kind of thing one hospital must not be
//! able to read from another by changing a UUID in a URL.
//!
//! Platform staff get a separate pair of methods: they review documents across
//! all hospitals, which is their job, and they are gated by `Permission` at the
//! route instead.

use std::sync::Arc;

use uuid::Uuid;

use crate::models::registration::{
    HospitalDocument, SubmissionStatus, UploadDocumentRequest,
};
use crate::models::user::{Claims, UserRole};
use crate::repositories::hospital_document::HospitalDocumentRepository;

#[derive(Debug, thiserror::Error)]
pub enum HospitalDocumentError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A caller asking about a hospital that is not theirs. A distinct variant
    /// per failure, so a 403 here can never be reported with another check's
    /// message.
    #[error("not authorized for this hospital's documents")]
    WrongHospital,
    #[error("role may not manage hospital documents")]
    NotAuthorized,
    #[error("document not found")]
    NotFound,
    /// Deleting is only for a document the hospital has not submitted yet.
    #[error("only a draft document can be deleted")]
    NotADraft,
    #[error("a rejection must include review notes")]
    RejectionNeedsNotes,
    #[error("a reviewer cannot return a document to draft")]
    CannotReviewToDraft,
}

pub struct HospitalDocumentService {
    repo: Arc<HospitalDocumentRepository>,
}

impl HospitalDocumentService {
    pub fn new(repo: Arc<HospitalDocumentRepository>) -> Self {
        Self { repo }
    }

    /// The tenant boundary for this surface. There is no RLS; this is it.
    ///
    /// A super admin is allowed through for support work, matching
    /// `authorize_shift_data_access`. Note this is a *document* check, not a
    /// clinical one — a licence is corporate paperwork, so the NDPR
    /// metadata-only rule that walls platform staff off from patient content
    /// does not apply here.
    fn authorize(claims: &Claims, hospital_id: Uuid) -> Result<(), HospitalDocumentError> {
        match claims.role {
            UserRole::SuperAdmin => Ok(()),
            UserRole::HospitalAdmin => {
                let own = claims
                    .hospital_id
                    .as_deref()
                    .and_then(|id| Uuid::parse_str(id).ok());
                if own == Some(hospital_id) {
                    Ok(())
                } else {
                    Err(HospitalDocumentError::WrongHospital)
                }
            }
            _ => Err(HospitalDocumentError::NotAuthorized),
        }
    }

    pub async fn upload(
        &self,
        hospital_id: Uuid,
        claims: &Claims,
        req: &UploadDocumentRequest,
    ) -> Result<HospitalDocument, HospitalDocumentError> {
        Self::authorize(claims, hospital_id)?;
        Ok(self.repo.upsert(hospital_id, req).await?)
    }

    pub async fn list(
        &self,
        hospital_id: Uuid,
        claims: &Claims,
    ) -> Result<Vec<HospitalDocument>, HospitalDocumentError> {
        Self::authorize(claims, hospital_id)?;
        Ok(self.repo.list_for_hospital(hospital_id).await?)
    }

    pub async fn delete(
        &self,
        hospital_id: Uuid,
        document_id: Uuid,
        claims: &Claims,
    ) -> Result<(), HospitalDocumentError> {
        Self::authorize(claims, hospital_id)?;

        // Distinguish "not yours / does not exist" from "already submitted", so
        // the hospital gets an actionable error instead of a bare 404 after a
        // reviewer has looked at the file.
        let existing = self
            .repo
            .get_for_hospital(hospital_id, document_id)
            .await?
            .ok_or(HospitalDocumentError::NotFound)?;

        if existing.submission_status != SubmissionStatus::Draft {
            return Err(HospitalDocumentError::NotADraft);
        }

        if self.repo.delete_draft(hospital_id, document_id).await? {
            Ok(())
        } else {
            // Lost a race with a reviewer between the read and the delete.
            Err(HospitalDocumentError::NotADraft)
        }
    }

    /// Submit every draft this hospital holds for compliance review.
    pub async fn submit_for_review(
        &self,
        hospital_id: Uuid,
        claims: &Claims,
    ) -> Result<u64, HospitalDocumentError> {
        Self::authorize(claims, hospital_id)?;
        Ok(self.repo.submit_drafts_for_review(hospital_id).await?)
    }

    /// Platform-staff read across hospitals. Gated by `Permission` at the route.
    pub async fn list_for_review(
        &self,
        hospital_id: Uuid,
    ) -> Result<Vec<HospitalDocument>, HospitalDocumentError> {
        Ok(self.repo.list_for_hospital(hospital_id).await?)
    }

    /// Record a compliance decision on one document.
    pub async fn review(
        &self,
        document_id: Uuid,
        status: SubmissionStatus,
        notes: Option<&str>,
        reviewer_user_id: Uuid,
    ) -> Result<HospitalDocument, HospitalDocumentError> {
        // `draft` belongs to the hospital. Letting a reviewer write it back
        // would erase the review trail and leave the document looking untouched.
        if status == SubmissionStatus::Draft {
            return Err(HospitalDocumentError::CannotReviewToDraft);
        }

        // A rejection the hospital cannot act on is worse than no decision: it
        // has to know which document to replace and why.
        if status == SubmissionStatus::Rejected
            && notes.map(|n| n.trim().is_empty()).unwrap_or(true)
        {
            return Err(HospitalDocumentError::RejectionNeedsNotes);
        }

        self.repo.get_any(document_id).await?.ok_or(HospitalDocumentError::NotFound)?;

        self.repo
            .review(document_id, &status, notes, reviewer_user_id)
            .await?
            .ok_or(HospitalDocumentError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(role: UserRole, hospital_id: Option<Uuid>) -> Claims {
        Claims {
            sub: Uuid::new_v4().to_string(),
            email: "t@example.test".to_string(),
            role,
            hospital_id: hospital_id.map(|h| h.to_string()),
            exp: 0,
            iat: 0,
        }
    }

    #[test]
    fn an_admin_may_only_reach_their_own_hospital() {
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();

        assert!(HospitalDocumentService::authorize(
            &claims(UserRole::HospitalAdmin, Some(mine)),
            mine
        )
        .is_ok());

        assert!(matches!(
            HospitalDocumentService::authorize(
                &claims(UserRole::HospitalAdmin, Some(mine)),
                theirs
            ),
            Err(HospitalDocumentError::WrongHospital)
        ));
    }

    #[test]
    fn an_admin_with_no_hospital_claim_is_refused() {
        assert!(matches!(
            HospitalDocumentService::authorize(
                &claims(UserRole::HospitalAdmin, None),
                Uuid::new_v4()
            ),
            Err(HospitalDocumentError::WrongHospital)
        ));
    }

    #[test]
    fn a_health_worker_has_no_business_here() {
        assert!(matches!(
            HospitalDocumentService::authorize(
                &claims(UserRole::HealthWorker, None),
                Uuid::new_v4()
            ),
            Err(HospitalDocumentError::NotAuthorized)
        ));
    }

    #[test]
    fn a_super_admin_may_reach_any_hospital() {
        assert!(HospitalDocumentService::authorize(
            &claims(UserRole::SuperAdmin, None),
            Uuid::new_v4()
        )
        .is_ok());
    }

    #[test]
    fn authorization_failures_have_distinct_variants() {
        // The shifts domain reuses one NotAuthorized across eight checks, which
        // is why a 403 from approve_handover reports "Not authorized to view
        // applications". These two must not collapse into one.
        let wrong = HospitalDocumentService::authorize(
            &claims(UserRole::HospitalAdmin, Some(Uuid::new_v4())),
            Uuid::new_v4(),
        );
        let role = HospitalDocumentService::authorize(
            &claims(UserRole::FinanceAdmin, None),
            Uuid::new_v4(),
        );
        assert_ne!(
            format!("{:?}", wrong.unwrap_err()),
            format!("{:?}", role.unwrap_err())
        );
    }
}

//! SQL for `hospital_documents` — a hospital's legal verification files.
//!
//! The table, its three enums and the DTOs for it have existed since
//! `20240004_create_hospital_documents.sql` and `models::registration`, but
//! until now nothing ever wrote a row: the only references in the codebase were
//! six `SELECT`s inside a `CASE` that derives a `documents_status` string. The
//! practical effect was that a hospital could upload a licence to Cloudinary and
//! the URL was dropped on the floor, and `documents_status` read `'unverified'`
//! for every hospital forever because no row could exist.
//!
//! `UNIQUE (hospital_id, document_type)` means one document of each type per
//! hospital, so the write is an upsert: re-uploading a corrected licence should
//! replace the old one, not accumulate.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::registration::{HospitalDocument, SubmissionStatus, UploadDocumentRequest};

/// Every column of the row, in struct order. Hoisted so the three reads cannot
/// drift apart — the `completed_at` bug earlier in this branch came from exactly
/// that, four column lists that silently disagreed with the struct.
const DOC_COLUMNS: &str = "id, hospital_id, document_type, file_url, file_name, \
     file_mime_type, file_size_bytes, credential_number, expiry_date, \
     issuing_authority, submission_status, uploaded_at, updated_at, \
     reviewed_at, reviewed_by, review_notes";

pub struct HospitalDocumentRepository {
    pool: PgPool,
}

impl HospitalDocumentRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert or replace this hospital's document of the given type.
    ///
    /// A re-upload resets the row to `draft` and clears the previous review, so
    /// a rejected document that gets corrected is not still carrying the old
    /// rejection notes when a reviewer next sees it.
    pub async fn upsert(
        &self,
        hospital_id: Uuid,
        req: &UploadDocumentRequest,
    ) -> Result<HospitalDocument, sqlx::Error> {
        sqlx::query_as::<_, HospitalDocument>(&format!(
            r#"
            INSERT INTO hospital_documents (
                hospital_id, document_type, file_url, file_name, file_mime_type,
                file_size_bytes, credential_number, expiry_date, issuing_authority,
                submission_status
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'draft')
            ON CONFLICT (hospital_id, document_type) DO UPDATE SET
                file_url          = EXCLUDED.file_url,
                file_name         = EXCLUDED.file_name,
                file_mime_type    = EXCLUDED.file_mime_type,
                file_size_bytes   = EXCLUDED.file_size_bytes,
                credential_number = EXCLUDED.credential_number,
                expiry_date       = EXCLUDED.expiry_date,
                issuing_authority = EXCLUDED.issuing_authority,
                submission_status = 'draft',
                reviewed_at       = NULL,
                reviewed_by       = NULL,
                review_notes      = NULL,
                updated_at        = NOW()
            RETURNING {DOC_COLUMNS}
            "#
        ))
        .bind(hospital_id)
        .bind(&req.document_type)
        .bind(&req.file_url)
        .bind(&req.file_name)
        .bind(&req.file_mime_type)
        .bind(req.file_size_bytes)
        .bind(&req.credential_number)
        .bind(req.expiry_date)
        .bind(&req.issuing_authority)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn list_for_hospital(
        &self,
        hospital_id: Uuid,
    ) -> Result<Vec<HospitalDocument>, sqlx::Error> {
        sqlx::query_as::<_, HospitalDocument>(&format!(
            "SELECT {DOC_COLUMNS} FROM hospital_documents \
             WHERE hospital_id = $1 ORDER BY uploaded_at DESC"
        ))
        .bind(hospital_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Fetch one document *scoped to its hospital*.
    ///
    /// The `hospital_id` predicate is in the statement rather than checked after
    /// the read: an id alone is guessable, and this is the only thing standing
    /// between one hospital and another's licence files. There is no RLS.
    pub async fn get_for_hospital(
        &self,
        hospital_id: Uuid,
        document_id: Uuid,
    ) -> Result<Option<HospitalDocument>, sqlx::Error> {
        sqlx::query_as::<_, HospitalDocument>(&format!(
            "SELECT {DOC_COLUMNS} FROM hospital_documents \
             WHERE id = $1 AND hospital_id = $2"
        ))
        .bind(document_id)
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Platform-staff read: by id, with no tenant predicate.
    pub async fn get_any(
        &self,
        document_id: Uuid,
    ) -> Result<Option<HospitalDocument>, sqlx::Error> {
        sqlx::query_as::<_, HospitalDocument>(&format!(
            "SELECT {DOC_COLUMNS} FROM hospital_documents WHERE id = $1"
        ))
        .bind(document_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Delete a document, but only while it is still the hospital's own draft.
    ///
    /// The `submission_status = 'draft'` guard is inside the statement, so a
    /// delete racing a reviewer's decision loses rather than erasing a document
    /// that has already been reviewed. Returns whether a row went.
    pub async fn delete_draft(
        &self,
        hospital_id: Uuid,
        document_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM hospital_documents \
             WHERE id = $1 AND hospital_id = $2 AND submission_status = 'draft'",
        )
        .bind(document_id)
        .bind(hospital_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Record a compliance decision.
    pub async fn review(
        &self,
        document_id: Uuid,
        status: &SubmissionStatus,
        notes: Option<&str>,
        reviewed_by: Uuid,
    ) -> Result<Option<HospitalDocument>, sqlx::Error> {
        sqlx::query_as::<_, HospitalDocument>(&format!(
            r#"
            UPDATE hospital_documents
               SET submission_status = $2,
                   review_notes      = $3,
                   reviewed_by       = $4,
                   reviewed_at       = NOW(),
                   updated_at        = NOW()
             WHERE id = $1
            RETURNING {DOC_COLUMNS}
            "#
        ))
        .bind(document_id)
        .bind(status)
        .bind(notes)
        .bind(reviewed_by)
        .fetch_optional(&self.pool)
        .await
    }

    /// Move every draft of a hospital into review in one statement.
    ///
    /// Only drafts: a document already approved or rejected keeps its decision,
    /// so re-submitting after fixing one file does not reopen the others.
    pub async fn submit_drafts_for_review(
        &self,
        hospital_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE hospital_documents SET submission_status = 'under_review', \
             updated_at = NOW() \
             WHERE hospital_id = $1 AND submission_status = 'draft'",
        )
        .bind(hospital_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

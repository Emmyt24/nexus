use sqlx::PgPool;
use uuid::Uuid;

use crate::models::consultation_note::{ConsultationNote, ConsultationTranscriptSegment};

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),

    #[error("Consultation note not found: {0}")]
    NotFound(Uuid),
}

pub struct ConsultationNoteRepository {
    pool: PgPool,
}

impl ConsultationNoteRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        patient_id: Uuid,
        hospital_id: Uuid,
        recorded_by: Uuid,
    ) -> Result<ConsultationNote, RepositoryError> {
        let note = sqlx::query_as::<_, ConsultationNote>(
            r#"
            INSERT INTO consultation_notes (patient_id, hospital_id, recorded_by)
            VALUES ($1, $2, $3)
            RETURNING *
            "#,
        )
        .bind(patient_id)
        .bind(hospital_id)
        .bind(recorded_by)
        .fetch_one(&self.pool)
        .await?;

        Ok(note)
    }

    pub async fn find_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<ConsultationNote>, RepositoryError> {
        let note = sqlx::query_as::<_, ConsultationNote>(
            "SELECT * FROM consultation_notes WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(note)
    }

    pub async fn list_by_patient(
        &self,
        patient_id: Uuid,
    ) -> Result<Vec<ConsultationNote>, RepositoryError> {
        let notes = sqlx::query_as::<_, ConsultationNote>(
            "SELECT * FROM consultation_notes WHERE patient_id = $1 ORDER BY created_at DESC",
        )
        .bind(patient_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(notes)
    }

    pub async fn segments_for_note(
        &self,
        note_id: Uuid,
    ) -> Result<Vec<ConsultationTranscriptSegment>, RepositoryError> {
        let segments = sqlx::query_as::<_, ConsultationTranscriptSegment>(
            "SELECT * FROM consultation_transcript_segments \
             WHERE consultation_note_id = $1 ORDER BY sequence",
        )
        .bind(note_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(segments)
    }

    /// Appends a transcribed (or failed) chunk and, on success, the chunk's
    /// text + duration to the note's running transcript/duration in one
    /// statement — avoids a read-modify-write race between concurrent chunk
    /// uploads for the same note (the browser uploads chunks sequentially in
    /// practice, but nothing enforces that server-side).
    pub async fn append_segment(
        &self,
        note_id: Uuid,
        sequence: i32,
        audio_path: &str,
        audio_duration_seconds: Option<f32>,
        transcript_text: &str,
        status: &str,
        last_error: Option<&str>,
    ) -> Result<ConsultationTranscriptSegment, RepositoryError> {
        let mut tx = self.pool.begin().await?;

        let segment = sqlx::query_as::<_, ConsultationTranscriptSegment>(
            r#"
            INSERT INTO consultation_transcript_segments
                (consultation_note_id, sequence, audio_path, audio_duration_seconds,
                 transcript_text, status, last_error)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING *
            "#,
        )
        .bind(note_id)
        .bind(sequence)
        .bind(audio_path)
        .bind(audio_duration_seconds)
        .bind(transcript_text)
        .bind(status)
        .bind(last_error)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE consultation_notes
            SET full_transcript = trim(both ' ' from full_transcript || ' ' || $2),
                duration_seconds = duration_seconds + $3
            WHERE id = $1
            "#,
        )
        .bind(note_id)
        .bind(transcript_text)
        .bind(audio_duration_seconds.unwrap_or(0.0).round() as i32)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(segment)
    }

    pub async fn update_fields(
        &self,
        id: Uuid,
        chief_complaint: Option<&str>,
        history_of_present_illness: Option<&str>,
        assessment: Option<&str>,
        plan: Option<&str>,
    ) -> Result<ConsultationNote, RepositoryError> {
        let note = sqlx::query_as::<_, ConsultationNote>(
            r#"
            UPDATE consultation_notes
            SET chief_complaint = COALESCE($2, chief_complaint),
                history_of_present_illness = COALESCE($3, history_of_present_illness),
                assessment = COALESCE($4, assessment),
                plan = COALESCE($5, plan)
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(id)
        .bind(chief_complaint)
        .bind(history_of_present_illness)
        .bind(assessment)
        .bind(plan)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RepositoryError::NotFound(id))?;

        Ok(note)
    }

    pub async fn mark_completed(&self, id: Uuid) -> Result<ConsultationNote, RepositoryError> {
        let note = sqlx::query_as::<_, ConsultationNote>(
            r#"
            UPDATE consultation_notes
            SET status = 'completed', completed_at = NOW()
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RepositoryError::NotFound(id))?;

        Ok(note)
    }
}

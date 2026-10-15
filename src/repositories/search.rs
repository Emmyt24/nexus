//! SQL for the hospital-scoped search.
//!
//! Every statement here carries its own `hospital_id` predicate. There is no
//! RLS in this database — the comment in `video_service` ("The tenant boundary.
//! There is no RLS — this check *is* it") applies with extra force to a
//! multi-entity search, because one forgotten `AND hospital_id = $1` leaks a
//! different tenant's data on a surface built to return whatever matches.
//!
//! Two shapes of scope appear below:
//!
//!  * `shifts` and `patients` carry `hospital_id` directly — one predicate.
//!  * `shift_handovers` has no tenant column and is reached through `shifts`.
//!  * `clinicians` has no tenant column *and no tenant* — it is a platform-wide
//!    marketplace pool. It is scoped by **relationship** instead: assigned to,
//!    applied to, or expressed interest in one of this hospital's shifts.
//!
//! Matching is plain `ILIKE`. No `pg_trgm`: the migration set contains no
//! `CREATE EXTENSION` at all, and `main.rs` runs migrations at boot with a fatal
//! error, so a role that cannot install the extension would stop the service
//! from starting. Trigram or `tsvector` indexing is the upgrade path if these
//! sequential scans start to hurt.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::search::HospitalSearchHit;

pub struct SearchRepository {
    pool: PgPool,
}

/// Escape the LIKE metacharacters in a user's query.
///
/// Without this, `q = "%"` matches every row in every bucket, and `_` quietly
/// becomes a single-character wildcard. `AdminRepository::search` builds its
/// pattern with a bare `format!("%{}%", query)` and has both problems; this does
/// not inherit them. The backslash must be escaped first or it would double the
/// escapes introduced after it.
fn like_pattern(query: &str) -> String {
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

impl SearchRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// This hospital's shifts, by role title, specialty, department or label.
    pub async fn search_shifts(
        &self,
        hospital_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<HospitalSearchHit>, sqlx::Error> {
        sqlx::query_as::<_, HospitalSearchHit>(
            r#"
            SELECT s.id,
                   'shift' AS kind,
                   s.role_title AS title,
                   NULLIF(TRIM(CONCAT_WS(' · ',
                       s.specialty, s.department, s.status::text)), '') AS subtitle,
                   s.id AS shift_id,
                   s.scheduled_start AS occurred_at
            FROM shifts s
            WHERE s.hospital_id = $1
              AND (s.role_title    ILIKE $2
                OR s.specialty     ILIKE $2
                OR s.department    ILIKE $2
                OR s.shift_label   ILIKE $2)
            ORDER BY s.scheduled_start DESC
            LIMIT $3
            "#,
        )
        .bind(hospital_id)
        .bind(like_pattern(query))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Clinicians this hospital has actually dealt with.
    ///
    /// The `EXISTS` is the tenant boundary. Dropping it would turn this into a
    /// directory of every clinician on the platform, which is what
    /// `GET /admin/search` is for and why that endpoint is gated on a
    /// platform-staff permission.
    ///
    /// The projection is narrow by design: name, role and specialty. No email,
    /// no licence number, no `user_id` — a hospital that has worked with someone
    /// does not thereby get their contact details.
    pub async fn search_clinicians(
        &self,
        hospital_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<HospitalSearchHit>, sqlx::Error> {
        sqlx::query_as::<_, HospitalSearchHit>(
            r#"
            SELECT c.id,
                   'clinician' AS kind,
                   CONCAT_WS(' ', c.first_name, c.last_name) AS title,
                   NULLIF(TRIM(CONCAT_WS(' · ',
                       c.role_title, c.specialty::text)), '') AS subtitle,
                   NULL::uuid AS shift_id,
                   c.created_at AS occurred_at
            FROM clinicians c
            WHERE (c.first_name           ILIKE $2
                OR c.last_name            ILIKE $2
                OR c.role_title           ILIKE $2
                OR c.specialty::text      ILIKE $2
                OR CONCAT_WS(' ', c.first_name, c.last_name) ILIKE $2)
              AND EXISTS (
                  SELECT 1
                  FROM shifts s
                  WHERE s.hospital_id = $1
                    AND (
                        s.assigned_clinician_id = c.id
                     OR EXISTS (SELECT 1 FROM shift_applications a
                                 WHERE a.shift_id = s.id AND a.clinician_id = c.id)
                     OR EXISTS (SELECT 1 FROM shift_interests i
                                 WHERE i.shift_id = s.id AND i.clinician_id = c.id)
                    )
              )
            ORDER BY c.last_name ASC, c.first_name ASC
            LIMIT $3
            "#,
        )
        .bind(hospital_id)
        .bind(like_pattern(query))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// This hospital's patients, **by name only**.
    ///
    /// `patients` also holds `symptoms`, `existing_conditions` and
    /// `disease_type`. Those are clinical details, and indexing them into a
    /// general search box turns an operational tool into a way to enumerate
    /// patients by condition. Name is what someone looking up a record needs.
    pub async fn search_patients(
        &self,
        hospital_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<HospitalSearchHit>, sqlx::Error> {
        sqlx::query_as::<_, HospitalSearchHit>(
            r#"
            SELECT p.id,
                   'patient' AS kind,
                   p.full_name AS title,
                   NULL::text AS subtitle,
                   NULL::uuid AS shift_id,
                   p.created_at AS occurred_at
            FROM patients p
            WHERE p.hospital_id = $1
              AND p.full_name ILIKE $2
            ORDER BY p.created_at DESC
            LIMIT $3
            "#,
        )
        .bind(hospital_id)
        .bind(like_pattern(query))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Handover notes on this hospital's shifts. No tenant column of its own, so
    /// the scope comes from the join.
    pub async fn search_handovers(
        &self,
        hospital_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<HospitalSearchHit>, sqlx::Error> {
        sqlx::query_as::<_, HospitalSearchHit>(
            r#"
            SELECT h.id,
                   'handover' AS kind,
                   s.role_title AS title,
                   LEFT(h.instructions, 160) AS subtitle,
                   h.shift_id,
                   h.submitted_at AS occurred_at
            FROM shift_handovers h
            JOIN shifts s ON s.id = h.shift_id
            WHERE s.hospital_id = $1
              AND (h.instructions     ILIKE $2
                OR h.equipment_status ILIKE $2)
            ORDER BY h.submitted_at DESC
            LIMIT $3
            "#,
        )
        .bind(hospital_id)
        .bind(like_pattern(query))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_metacharacters_are_escaped() {
        // A bare `%` would otherwise match every row in every bucket.
        assert_eq!(like_pattern("%"), "%\\%%");
        assert_eq!(like_pattern("_"), "%\\_%");
        // The backslash is escaped first, so its own escape is not re-escaped.
        assert_eq!(like_pattern("\\"), "%\\\\%");
        assert_eq!(like_pattern("50%_off"), "%50\\%\\_off%");
    }

    #[test]
    fn ordinary_text_is_wrapped_for_a_substring_match() {
        assert_eq!(like_pattern("emergency"), "%emergency%");
    }
}

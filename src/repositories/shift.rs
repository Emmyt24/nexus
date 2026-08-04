use chrono::{Duration, Utc};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::registration::RegistrationStatus;
use crate::models::shift::{
    CreateShiftRequest, PayType, Shift, ShiftApplication, ShiftApplicationStatus, ShiftPriority,
    ShiftStatus, ShiftType,
};

/// Raw row backing the ranking query. Internal to the repo —
#[derive(Debug, Clone, FromRow)]
pub struct InterestedClinicianRow {
    pub clinician_id: Uuid,
    pub first_name: String,
    pub last_name: String,
    pub rating: f32,
    pub rating_count: i32,
    pub completed_shifts: i64,
    pub accepts: i64,
    pub declines: i64,
    pub expires: i64,
    pub clinician_lat: Option<f64>,
    pub clinician_lng: Option<f64>,
}

/// Raw row backing the eligibility query. The service decides
#[derive(Debug, Clone, FromRow)]
pub struct EligibleClinicianRow {
    pub clinician_id: Uuid,
    pub user_id: Uuid,
    pub first_name: String,
    pub email: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

/// Raw row backing the "Shifts Near You" query. Distance and final
#[derive(Debug, Clone, FromRow)]
pub struct NearbyShiftRow {
    pub shift_id: Uuid,
    pub hospital_id: Uuid,
    pub hospital_name: Option<String>,
    pub role_title: String,
    pub specialty: Option<String>,
    pub shift_type: ShiftType,
    pub priority: ShiftPriority,
    pub scheduled_start: chrono::DateTime<chrono::Utc>,
    pub duration_hours: f32,
    pub pay_type: PayType,
    pub rate_kobo_per_hour: Option<i64>,
    pub fixed_rate_kobo: Option<i64>,
    pub stat_bonus_kobo: Option<i64>,
    /// Great-circle distance from the worker's origin to the hospital, in km.
    /// `None` for virtual shifts and for in-person shifts whose hospital has no
    /// recorded location. Computed in SQL so paging and sorting stay correct.
    pub distance_km: Option<f64>,
    pub interest_expressed: bool,
}

pub struct ShiftRepository {
    pool: PgPool,
}

impl ShiftRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Check if a hospital is approved to create shifts
    pub async fn check_hospital_approved(&self, hospital_id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query_scalar::<_, Option<RegistrationStatus>>(
            r#"
            SELECT admin_registration_status
            FROM hospitals
            WHERE id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(matches!(result, Some(Some(RegistrationStatus::Approved))))
    }

    /// Get hospital name by ID
    pub async fn get_hospital_name(
        &self,
        hospital_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT name
            FROM hospitals
            WHERE id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn get_hospital_contact(
        &self,
        hospital_id: Uuid,
    ) -> Result<Option<(String, String)>, sqlx::Error> {
        sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT name, email
            FROM hospitals
            WHERE id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn get_clinician_contact(
        &self,
        clinician_id: Uuid,
    ) -> Result<Option<(String, String, String)>, sqlx::Error> {
        sqlx::query_as::<_, (String, String, String)>(
            r#"
            SELECT c.first_name, c.last_name, u.email
            FROM clinicians c
            JOIN users u ON c.user_id = u.id
            WHERE c.id = $1
            "#,
        )
        .bind(clinician_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Resolve `clinicians.id` from the authenticated user's `users.id`.

    pub async fn find_clinician_id_for_user(
        &self,
        user_id: Uuid,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id FROM clinicians WHERE user_id = $1
            "#,
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Resolve `users.id` from a `clinicians.id` — used to target push
    /// notifications at the clinician behind a shift assignment.
    pub async fn get_clinician_user_id(
        &self,
        clinician_id: Uuid,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(r#"SELECT user_id FROM clinicians WHERE id = $1"#)
            .bind(clinician_id)
            .fetch_optional(&self.pool)
            .await
    }

    /// Get the hospital's first registered location coordinates

    pub async fn get_hospital_coordinates(
        &self,
        hospital_id: Uuid,
    ) -> Result<Option<(f64, f64)>, sqlx::Error> {
        sqlx::query_as::<_, (f64, f64)>(
            r#"
            SELECT latitude, longitude
            FROM hospital_locations
            WHERE hospital_id = $1
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// fetch interested clinicians for a shift along with the data
    pub async fn list_interested_with_stats(
        &self,
        shift_id: Uuid,
    ) -> Result<Vec<InterestedClinicianRow>, sqlx::Error> {
        sqlx::query_as::<_, InterestedClinicianRow>(
            r#"
            SELECT
                c.id                                                  AS clinician_id,
                c.first_name,
                c.last_name,
                c.rating,
                c.rating_count,
                (SELECT COUNT(*) FROM shifts s2
                    WHERE s2.assigned_clinician_id = c.id AND s2.status = 'completed') AS completed_shifts,
                (SELECT COUNT(*) FROM shift_assignments a
                    WHERE a.clinician_id = c.id AND a.status = 'accepted') AS accepts,
                (SELECT COUNT(*) FROM shift_assignments a
                    WHERE a.clinician_id = c.id AND a.status = 'declined') AS declines,
                (SELECT COUNT(*) FROM shift_assignments a
                    WHERE a.clinician_id = c.id AND a.status = 'expired')  AS expires,
                cl.latitude  AS clinician_lat,
                cl.longitude AS clinician_lng
            FROM shift_interests si
            JOIN clinicians c          ON c.id = si.clinician_id
            LEFT JOIN clinician_locations cl ON cl.clinician_id = c.id
            WHERE si.shift_id = $1
            ORDER BY si.expressed_at ASC
            "#,
        )
        .bind(shift_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Fetch a pending offer for `(shift, clinician)` in `offered`

    pub async fn get_pending_offer(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<Option<(Uuid, chrono::DateTime<chrono::Utc>)>, sqlx::Error> {
        sqlx::query_as::<_, (Uuid, chrono::DateTime<chrono::Utc>)>(
            r#"
            SELECT id, expires_at
            FROM shift_assignments
            WHERE shift_id = $1 AND clinician_id = $2 AND status = 'offered'
            LIMIT 1
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Accept an offer inside a transaction

    /// Accept an offer inside a transaction. The `status = 'offered'` guard plus
    /// the row lock the UPDATE takes serialize concurrent accepts: only the
    /// first transaction flips the row, so callers must treat a `0` return
    /// (no rows updated) as "already responded to" and abort.
    pub async fn accept_offer_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        assignment_id: Uuid,
        ndpr_consent: &serde_json::Value,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE shift_assignments
               SET status        = 'accepted',
                   responded_at  = NOW(),ndpr_consent  = $2,
                   updated_at    = NOW()
             WHERE id = $1 AND status = 'offered'
            "#,
        )
        .bind(assignment_id)
        .bind(ndpr_consent)
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected())
    }

    /// Whether this clinician has already declined an offer for this shift
    /// (SCRUM-14 / US-06 AC-04 — cannot re-offer a decliner).
    pub async fn has_declined_offer(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM shift_assignments
                WHERE shift_id = $1 AND clinician_id = $2 AND status = 'declined'
            )
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Cancel sibling offers when one is accepted. Marks every

    pub async fn cancel_sibling_offers_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        keep_assignment_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE shift_assignments
               SET status       = 'expired',
                   responded_at = NOW(),updated_at   = NOW()
             WHERE shift_id = $1
               AND id      <> $2
               AND status   = 'offered'
            "#,
        )
        .bind(shift_id)
        .bind(keep_assignment_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Mark the shift assigned and stamp the chosen clinician

    pub async fn assign_shift_to_clinician_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE shifts
               SET status                = 'assigned',
                   assigned_clinician_id = $2,
                   updated_at            = NOW()
             WHERE id = $1
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Decline an offer

    pub async fn decline_offer(
        &self,
        assignment_id: Uuid,
        reason: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE shift_assignments
               SET status         = 'declined',
                   responded_at   = NOW(),decline_reason = $2,
                   updated_at     = NOW()
             WHERE id = $1 AND status = 'offered'
            "#,
        )
        .bind(assignment_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// conflict check. Returns true if the clinician has

    pub async fn has_conflicting_shift(
        &self,
        clinician_id: Uuid,
        candidate_start: chrono::DateTime<chrono::Utc>,
        candidate_end: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, sqlx::Error> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM shifts
            WHERE assigned_clinician_id = $1
              AND status IN ('assigned', 'upcoming', 'in_progress')
              AND scheduled_start < $3
              AND scheduled_end   > $2
            "#,
        )
        .bind(clinician_id)
        .bind(candidate_start)
        .bind(candidate_end)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Create a `shift_assignments` row marking an offer to a

    pub async fn create_assignment_offer(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Uuid, sqlx::Error> {
        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO shift_assignments (shift_id, clinician_id, status, expires_at)
            VALUES ($1, $2, 'offered', $3)
            RETURNING id
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// broadcast radius (km) for this hospital's primary location

    pub async fn get_broadcast_radius_km(
        &self,
        hospital_id: Uuid,
    ) -> Result<Option<f64>, sqlx::Error> {
        sqlx::query_scalar::<_, f64>(
            r#"
            SELECT shift_broadcast_radius_km
            FROM hospital_locations
            WHERE hospital_id = $1
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// clock_in_radius_meters for this hospital's primary location

    pub async fn get_clock_in_radius_meters(
        &self,
        hospital_id: Uuid,
    ) -> Result<Option<i32>, sqlx::Error> {
        sqlx::query_scalar::<_, i32>(
            r#"
            SELECT clock_in_radius_meters
            FROM hospital_locations
            WHERE hospital_id = $1
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Record a clock-in inside the create-shift-in-progress

    pub async fn record_clockin_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        clinician_id: Uuid,
        method: &crate::models::shift::ClockinMethod,
        latitude: Option<f64>,
        longitude: Option<f64>,
        distance_meters: Option<f32>,
        late_minutes: i32,
        late_penalty_applied: bool,
    ) -> Result<Uuid, sqlx::Error> {
        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO shift_attendance (
                shift_id, clinician_id, clockin_at, clockin_method,
                clockin_latitude, clockin_longitude, clockin_distance_meters,
                late_minutes, late_penalty_applied
            )
            VALUES ($1, $2, NOW(), $3, $4, $5, $6, $7, $8)
            ON CONFLICT (shift_id) DO UPDATE
              SET clockin_at               = EXCLUDED.clockin_at,
                  clockin_method           = EXCLUDED.clockin_method,
                  clockin_latitude         = EXCLUDED.clockin_latitude,
                  clockin_longitude        = EXCLUDED.clockin_longitude,
                  clockin_distance_meters  = EXCLUDED.clockin_distance_meters,
                  late_minutes             = EXCLUDED.late_minutes,
                  late_penalty_applied     = EXCLUDED.late_penalty_applied,
                  updated_at               = NOW()
            RETURNING id
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .bind(method)
        .bind(latitude)
        .bind(longitude)
        .bind(distance_meters)
        .bind(late_minutes)
        .bind(late_penalty_applied)
        .fetch_one(&mut **tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE shifts
               SET status     = 'in_progress',
                   updated_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(shift_id)
        .execute(&mut **tx)
        .await?;

        Ok(id)
    }

    /// Upsert a handover row for the given shift.

    pub async fn upsert_handover(
        &self,
        shift_id: Uuid,
        patients_seen: i32,
        critical_patients: &serde_json::Value,
        pending_tasks: &serde_json::Value,
        instructions: &str,
        equipment_status: Option<&str>,
    ) -> Result<crate::models::shift::HandoverResponse, sqlx::Error> {
        sqlx::query_as::<_, crate::models::shift::HandoverResponse>(
            r#"
            INSERT INTO shift_handovers (
                shift_id, patients_seen, critical_patients, pending_tasks,
                instructions, equipment_status,
                submitted_at, editable_until, auto_approve_after
            )
            VALUES (
                $1, $2, $3, $4, $5, $6,
                NOW(), NOW() + INTERVAL '1 hour', NOW() + INTERVAL '48 hours'
            )
            ON CONFLICT (shift_id) DO UPDATE
              SET patients_seen     = EXCLUDED.patients_seen,
                  critical_patients = EXCLUDED.critical_patients,
                  pending_tasks     = EXCLUDED.pending_tasks,
                  instructions      = EXCLUDED.instructions,
                  equipment_status  = EXCLUDED.equipment_status,
                  updated_at        = NOW()
            RETURNING
                id, shift_id, patients_seen, critical_patients, pending_tasks,
                instructions, equipment_status,
                submitted_at, editable_until, auto_approve_after,
                hospital_approved_at, revision_requested_at, revision_notes
            "#,
        )
        .bind(shift_id)
        .bind(patients_seen)
        .bind(critical_patients)
        .bind(pending_tasks)
        .bind(instructions)
        .bind(equipment_status)
        .fetch_one(&self.pool)
        .await
    }

    /// Fetch the existing handover row, if any

    pub async fn get_handover(
        &self,
        shift_id: Uuid,
    ) -> Result<Option<crate::models::shift::HandoverResponse>, sqlx::Error> {
        sqlx::query_as::<_, crate::models::shift::HandoverResponse>(
            r#"
            SELECT id, shift_id, patients_seen, critical_patients, pending_tasks,
                   instructions, equipment_status,
                   submitted_at, editable_until, auto_approve_after,
                   hospital_approved_at, revision_requested_at, revision_notes
            FROM shift_handovers
            WHERE shift_id = $1
            "#,
        )
        .bind(shift_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Read the current clock-in time for a shift, used to compute

    pub async fn get_attendance_clockin(
        &self,
        shift_id: Uuid,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, sqlx::Error> {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            r#"
            SELECT clockin_at FROM shift_attendance WHERE shift_id = $1
            "#,
        )
        .bind(shift_id)
        .fetch_optional(&self.pool)
        .await
        .map(|opt| opt.flatten())
    }

    /// Read clockout_at to enforce the 24-hour revision window

    pub async fn get_attendance_clockout(
        &self,
        shift_id: Uuid,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, sqlx::Error> {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            r#"
            SELECT clockout_at FROM shift_attendance WHERE shift_id = $1
            "#,
        )
        .bind(shift_id)
        .fetch_optional(&self.pool)
        .await
        .map(|opt| opt.flatten())
    }

    /// Record clockout inside a transaction and flip the shift

    pub async fn record_clockout_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        worked_minutes: i32,
    ) -> Result<Uuid, sqlx::Error> {
        let id: Uuid = sqlx::query_scalar(
            r#"
            UPDATE shift_attendance
               SET clockout_at    = NOW(),worked_minutes = $2,
                   updated_at     = NOW()
             WHERE shift_id = $1
             RETURNING id
            "#,
        )
        .bind(shift_id)
        .bind(worked_minutes)
        .fetch_one(&mut **tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE shifts
               SET status = 'completed', updated_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(shift_id)
        .execute(&mut **tx)
        .await?;

        Ok(id)
    }

    /// Request a handover revision (hospital-side, within 24h of

    pub async fn request_handover_revision(
        &self,
        shift_id: Uuid,
        notes: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE shift_handovers
               SET revision_requested_at = NOW(),revision_notes        = $2,
                   updated_at            = NOW()
             WHERE shift_id = $1
            "#,
        )
        .bind(shift_id)
        .bind(notes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Hospital explicitly approves the handover, unlocking the

    pub async fn approve_handover(&self, shift_id: Uuid) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r#"
            UPDATE shift_handovers
               SET hospital_approved_at = NOW(),updated_at           = NOW()
             WHERE shift_id = $1
               AND hospital_approved_at IS NULL
            "#,
        )
        .bind(shift_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Insert a rating row. `window_closes_at` is the 7-day cap
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_rating(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        rater_user_id: Uuid,
        ratee_id: Uuid,
        ratee_kind: &str,
        score: i16,
        dimensions: Option<&serde_json::Value>,
        comment: Option<&str>,
        window_closes_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::models::shift::RatingResponse, sqlx::Error> {
        sqlx::query_as::<_, crate::models::shift::RatingResponse>(
            r#"
            INSERT INTO shift_ratings (
                shift_id, rater_user_id, ratee_id, ratee_kind,
                score, dimensions, comment, is_anonymous,
                editable_until, window_closes_at
            )
            VALUES (
                $1, $2, $3, $4::rating_ratee_kind,
                $5, $6, $7, TRUE,
                NOW() + INTERVAL '48 hours', $8
            )
            RETURNING
                id, shift_id, ratee_id, ratee_kind::text AS ratee_kind,
                score, dimensions, comment, is_anonymous,
                editable_until, window_closes_at, created_at
            "#,
        )
        .bind(shift_id)
        .bind(rater_user_id)
        .bind(ratee_id)
        .bind(ratee_kind)
        .bind(score)
        .bind(dimensions)
        .bind(comment)
        .bind(window_closes_at)
        .fetch_one(&mut **tx)
        .await
    }

    /// Edit a rating within the 48h window. Caller validates the

    pub async fn update_rating(
        &self,
        rating_id: Uuid,
        score: Option<i16>,
        dimensions: Option<&serde_json::Value>,
        comment: Option<&str>,
    ) -> Result<crate::models::shift::RatingResponse, sqlx::Error> {
        sqlx::query_as::<_, crate::models::shift::RatingResponse>(
            r#"
            UPDATE shift_ratings
               SET score      = COALESCE($2, score),
                   dimensions = COALESCE($3, dimensions),
                   comment    = COALESCE($4, comment),
                   updated_at = NOW()
             WHERE id = $1
             RETURNING
                id, shift_id, ratee_id, ratee_kind::text AS ratee_kind,
                score, dimensions, comment, is_anonymous,
                editable_until, window_closes_at, created_at
            "#,
        )
        .bind(rating_id)
        .bind(score)
        .bind(dimensions)
        .bind(comment)
        .fetch_one(&self.pool)
        .await
    }

    /// Fetch a rating by id (for the edit handler's auth check).

    pub async fn get_rating_for_edit(
        &self,
        rating_id: Uuid,
    ) -> Result<Option<(crate::models::shift::RatingResponse, Uuid)>, sqlx::Error> {
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                Uuid,
                String,
                i16,
                Option<serde_json::Value>,
                Option<String>,
                bool,
                chrono::DateTime<chrono::Utc>,
                chrono::DateTime<chrono::Utc>,
                chrono::DateTime<chrono::Utc>,
                Uuid,
            ),
        >(
            r#"
            SELECT id, shift_id, ratee_id, ratee_kind::text,
                   score, dimensions, comment, is_anonymous,
                   editable_until, window_closes_at, created_at, rater_user_id
            FROM shift_ratings
            WHERE id = $1
            "#,
        )
        .bind(rating_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|t| {
            (
                crate::models::shift::RatingResponse {
                    id: t.0,
                    shift_id: t.1,
                    ratee_id: t.2,
                    ratee_kind: t.3,
                    score: t.4,
                    dimensions: t.5,
                    comment: t.6,
                    is_anonymous: t.7,
                    editable_until: t.8,
                    window_closes_at: t.9,
                    created_at: t.10,
                },
                t.11,
            )
        }))
    }

    /// Recompute the clinician's cached average rating after a

    pub async fn recompute_clinician_rating_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        clinician_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE clinicians c
               SET rating = COALESCE((
                       SELECT AVG(score)::REAL
                       FROM shift_ratings r
                       WHERE r.ratee_kind = 'clinician'
                         AND r.ratee_id   = c.id
                   ), 0.0),
                   rating_count = (
                       SELECT COUNT(*)::INTEGER
                       FROM shift_ratings r
                       WHERE r.ratee_kind = 'clinician'
                         AND r.ratee_id   = c.id
                   ),
                   updated_at = NOW()
             WHERE c.id = $1
            "#,
        )
        .bind(clinician_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Open shifts within `radius_km` of the worker's origin, excluding shifts
    /// they have dismissed. Virtual shifts are always included (they have no
    /// location to filter on); in-person shifts are kept only when the exact
    /// haversine distance from `(origin_lat, origin_lng)` to the hospital is
    /// within the radius. Results are ordered by urgency rank, then distance
    /// (nulls last), then start time, and paged with `limit`/`offset`.
    ///
    /// The bounding-box predicate is an index-friendly prefilter; correctness
    /// comes from the exact haversine `<= radius_km` gate that follows it.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_nearby_shifts(
        &self,
        clinician_id: Uuid,
        origin_lat: f64,
        origin_lng: f64,
        radius_km: f64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<NearbyShiftRow>, sqlx::Error> {
        sqlx::query_as::<_, NearbyShiftRow>(
            r#"
            WITH origin AS (
                SELECT $2::double precision AS lat,
                       $3::double precision AS lng
            )
            SELECT
                s.id              AS shift_id,
                s.hospital_id,
                h.name            AS hospital_name,
                s.role_title,
                s.specialty,
                s.shift_type      AS "shift_type: _",
                s.priority        AS "priority: _",
                s.scheduled_start,
                s.duration_hours,
                s.pay_type        AS "pay_type: _",
                s.rate_kobo_per_hour,
                s.fixed_rate_kobo,
                s.stat_bonus_kobo,
                -- Exact great-circle distance (km). NULL when the hospital has
                -- no recorded coordinates. 6371.0088 = mean Earth radius (km).
                CASE WHEN hl.latitude IS NOT NULL THEN
                    2 * 6371.0088 * asin(sqrt(
                        power(sin(radians(hl.latitude - o.lat) / 2), 2)
                        + cos(radians(o.lat)) * cos(radians(hl.latitude))
                          * power(sin(radians(hl.longitude - o.lng) / 2), 2)
                    ))
                END               AS distance_km,
                EXISTS (
                    SELECT 1 FROM shift_interests si
                    WHERE si.shift_id = s.id AND si.clinician_id = $1
                ) AS interest_expressed
            FROM shifts s
            JOIN hospitals h                ON h.id = s.hospital_id
            LEFT JOIN hospital_locations hl ON hl.hospital_id = h.id
            CROSS JOIN origin o
            WHERE s.status = 'open'
              AND NOT EXISTS (
                  SELECT 1 FROM shift_dismissals sd
                  WHERE sd.shift_id = s.id AND sd.clinician_id = $1
              )
              AND (
                  -- Virtual shifts: no location, always in range.
                  s.shift_type = 'virtual'
                  -- In-person without coordinates: data gap, don't hide it.
                  OR hl.latitude IS NULL
                  -- In-person with coordinates: bounding box (index prefilter)
                  -- then exact haversine gate. 111.045 km per degree latitude;
                  -- longitude degrees shrink by cos(lat), clamped to avoid a
                  -- divide-by-zero near the poles.
                  OR (
                      hl.latitude BETWEEN o.lat - ($4 / 111.045)
                                      AND o.lat + ($4 / 111.045)
                      AND hl.longitude BETWEEN
                              o.lng - ($4 / (111.045 * GREATEST(cos(radians(o.lat)), 0.01)))
                          AND o.lng + ($4 / (111.045 * GREATEST(cos(radians(o.lat)), 0.01)))
                      AND 2 * 6371.0088 * asin(sqrt(
                              power(sin(radians(hl.latitude - o.lat) / 2), 2)
                              + cos(radians(o.lat)) * cos(radians(hl.latitude))
                                * power(sin(radians(hl.longitude - o.lng) / 2), 2)
                          )) <= $4
                  )
              )
            ORDER BY
                CASE s.priority
                    WHEN 'stat'      THEN 0
                    WHEN 'urgent'    THEN 1
                    WHEN 'normal'    THEN 2
                    WHEN 'scheduled' THEN 3
                    ELSE 4
                END,
                distance_km ASC NULLS LAST,
                s.scheduled_start ASC
            LIMIT $5 OFFSET $6
            "#,
        )
        .bind(clinician_id)
        .bind(origin_lat)
        .bind(origin_lng)
        .bind(radius_km)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    /// Upsert the worker's last-known GPS position. Called when a nearby-shift
    /// request supplies live coordinates so later requests and other features
    /// (Workforce Pool distance, clock-in) see the freshest location.
    pub async fn upsert_clinician_location(
        &self,
        clinician_id: Uuid,
        latitude: f64,
        longitude: f64,
        accuracy_meters: Option<f32>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO clinician_locations
                (clinician_id, latitude, longitude, accuracy_meters, recorded_at)
            VALUES ($1, $2, $3, $4, NOW())
            ON CONFLICT (clinician_id) DO UPDATE
                SET latitude        = EXCLUDED.latitude,
                    longitude       = EXCLUDED.longitude,
                    accuracy_meters = EXCLUDED.accuracy_meters,
                    recorded_at     = NOW()
            "#,
        )
        .bind(clinician_id)
        .bind(latitude)
        .bind(longitude)
        .bind(accuracy_meters)
        .fetch_optional(&self.pool)
        .await
        .map(|_| ())
    }

    /// The worker's last-known GPS origin, if any has been recorded.
    pub async fn get_clinician_location(
        &self,
        clinician_id: Uuid,
    ) -> Result<Option<(f64, f64)>, sqlx::Error> {
        let row = sqlx::query_as::<_, (f64, f64)>(
            r#"
            SELECT latitude, longitude
            FROM clinician_locations
            WHERE clinician_id = $1
            "#,
        )
        .bind(clinician_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// List a clinician's expressed interests + formal applications

    pub async fn list_my_applications(
        &self,
        clinician_id: Uuid,
    ) -> Result<Vec<crate::models::shift::MyApplicationEntry>, sqlx::Error> {
        let interests = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                chrono::DateTime<chrono::Utc>,
                ShiftStatus,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            r#"
            SELECT s.id, s.hospital_id, s.role_title, s.scheduled_start,
                   s.status AS "status: _", si.expressed_at AS created_at
            FROM shift_interests si
            JOIN shifts s ON s.id = si.shift_id
            WHERE si.clinician_id = $1
            "#,
        )
        .bind(clinician_id)
        .fetch_all(&self.pool)
        .await?;

        let applications = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                chrono::DateTime<chrono::Utc>,
                ShiftStatus,
                ShiftApplicationStatus,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            r#"
            SELECT s.id, s.hospital_id, s.role_title, s.scheduled_start,
                   s.status AS "status: _",
                   a.status AS "app_status: _",
                   a.created_at
            FROM shift_applications a
            JOIN shifts s ON s.id = a.shift_id
            WHERE a.clinician_id = $1
            "#,
        )
        .bind(clinician_id)
        .fetch_all(&self.pool)
        .await?;

        let mut rows: Vec<crate::models::shift::MyApplicationEntry> = interests
            .into_iter()
            .map(|(sid, hid, title, start, status, created)| {
                crate::models::shift::MyApplicationEntry {
                    shift_id: sid,
                    hospital_id: hid,
                    role_title: title,
                    scheduled_start: start,
                    shift_status: status,
                    kind: "interest".to_string(),
                    application_status: None,
                    created_at: created,
                }
            })
            .collect();
        rows.extend(applications.into_iter().map(
            |(sid, hid, title, start, status, app_status, created)| {
                crate::models::shift::MyApplicationEntry {
                    shift_id: sid,
                    hospital_id: hid,
                    role_title: title,
                    scheduled_start: start,
                    shift_status: status,
                    kind: "application".to_string(),
                    application_status: Some(app_status),
                    created_at: created,
                }
            },
        ));

        // Newest first
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(rows)
    }

    /// Withdraw a previously expressed interest. Returns the

    pub async fn withdraw_interest(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM shift_interests
            WHERE shift_id = $1 AND clinician_id = $2
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Insert a bookmark. Idempotent via the unique constraint

    pub async fn bookmark_shift(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO shift_bookmarks (shift_id, clinician_id)
            VALUES ($1, $2)
            ON CONFLICT (shift_id, clinician_id) DO NOTHING
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a bookmark. Returns rows-affected so the caller can

    pub async fn unbookmark_shift(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM shift_bookmarks
            WHERE shift_id = $1 AND clinician_id = $2
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Record a dismissal so the shift no longer appears in this

    pub async fn dismiss_shift(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO shift_dismissals (shift_id, clinician_id)
            VALUES ($1, $2)
            ON CONFLICT (shift_id, clinician_id) DO NOTHING
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Find clinicians eligible to receive a broadcast for the

    pub async fn find_eligible_clinicians(
        &self,
        allowed_specialties: &[crate::models::clinician::ClinicalSpecialty],
    ) -> Result<Vec<EligibleClinicianRow>, sqlx::Error> {
        sqlx::query_as::<_, EligibleClinicianRow>(
            r#"
            SELECT
                c.id           AS clinician_id,
                c.user_id,
                c.first_name,
                u.email,
                cl.latitude,
                cl.longitude
            FROM clinicians c
            JOIN users u                    ON u.id = c.user_id
            LEFT JOIN clinician_locations cl ON cl.clinician_id = c.id
            WHERE c.availability = 'available_now'
              AND c.is_active   = TRUE
              AND c.is_verified = TRUE
              AND c.specialty   = ANY($1::clinical_specialty[])
              AND NOT EXISTS (
                  SELECT 1 FROM shifts s
                  WHERE s.assigned_clinician_id = c.id
                    AND s.status = 'in_progress'
              )
            "#,
        )
        .bind(allowed_specialties)
        .fetch_all(&self.pool)
        .await
    }

    /// Insert a `shift_broadcast_records` audit row. `broadcast_by`

    pub async fn record_broadcast(
        &self,
        shift_id: Uuid,
        broadcast_by: Option<Uuid>,
        eligible_count: i32,
        radius_km: f64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO shift_broadcast_records (
                shift_id, broadcast_by, eligible_clinicians_count, broadcast_radius_km
            )
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(shift_id)
        .bind(broadcast_by)
        .bind(eligible_count)
        .bind(radius_km)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Find open shifts that are due for re-broadcast per their

    pub async fn find_shifts_due_for_rebroadcast(&self) -> Result<Vec<Shift>, sqlx::Error> {
        sqlx::query_as::<_, Shift>(
            r#"
            SELECT s.id, s.hospital_id, s.role_category, s.role_title, s.specialty,
                   s.department, s.shift_type, s.status, s.priority, s.urgency_bonus_pct,
                   s.scheduled_start, s.duration_hours, s.scheduled_end,
                   s.assigned_clinician_id, s.rate_kobo_per_hour, s.fixed_rate_kobo,
                   s.pay_type, s.stat_bonus_kobo, s.effective_rate_kobo_per_hour,
                   s.grand_total_kobo, s.shift_label, s.job_description,
                   s.draft_quality_score, s.notes, s.created_by,
                   s.broadcast_consent_confirmed, s.matched_clinicians_at_publish,
                   s.broadcast_at, s.billing_triggered_at, s.created_at, s.updated_at,
                   h.name AS hospital_name
            FROM shifts s
            JOIN hospitals h ON s.hospital_id = h.id
            WHERE s.status = 'open'
              AND s.priority IN ('stat', 'urgent')
              AND (
                  SELECT COALESCE(MAX(r.broadcast_at), 'epoch')
                  FROM shift_broadcast_records r
                  WHERE r.shift_id = s.id
              ) + (
                  CASE s.priority
                      WHEN 'stat'   THEN INTERVAL '15 minutes'
                      WHEN 'urgent' THEN INTERVAL '30 minutes'
                  END
              ) <= NOW()
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Sweep step: flip every offered assignment whose

    pub async fn expire_due_offers(&self) -> Result<Vec<(Uuid, Uuid, Uuid, String)>, sqlx::Error> {
        sqlx::query_as::<_, (Uuid, Uuid, Uuid, String)>(
            r#"
            WITH expired AS (
                UPDATE shift_assignments a
                   SET status       = 'expired',
                       responded_at = NOW(),updated_at   = NOW()
                 WHERE status     = 'offered'
                   AND expires_at < NOW()
                RETURNING id, shift_id
            )
            SELECT e.id, e.shift_id, s.hospital_id, s.role_title
            FROM expired e
            JOIN shifts s ON s.id = e.shift_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Sweep step: auto-approve handovers whose 48h window has

    pub async fn auto_approve_due_handovers(
        &self,
    ) -> Result<Vec<(Uuid, Uuid, Uuid, Uuid, String)>, sqlx::Error> {
        sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid, String)>(
            r#"
            WITH approved AS (
                UPDATE shift_handovers h
                   SET hospital_approved_at = NOW(),updated_at           = NOW()
                 WHERE h.hospital_approved_at IS NULL
                   AND h.revision_requested_at IS NULL
                   AND h.auto_approve_after < NOW()
                RETURNING id, shift_id
            )
            SELECT a.id, a.shift_id, s.assigned_clinician_id, s.hospital_id, s.role_title
            FROM approved a
            JOIN shifts s ON s.id = a.shift_id
            WHERE s.assigned_clinician_id IS NOT NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Recompute and cache `clinicians.acceptance_rate_pct` from

    pub async fn recompute_clinician_acceptance_rate(
        &self,
        clinician_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE clinicians c
               SET acceptance_rate_pct = (
                       SELECT CASE
                                  WHEN COUNT(*) = 0 THEN NULL
                                  ELSE (
                                      COUNT(*) FILTER (WHERE a.status = 'accepted')::REAL
                                      / COUNT(*)::REAL
                                  ) * 100.0
                              END
                       FROM shift_assignments a
                       WHERE a.clinician_id = c.id
                         AND a.status IN ('accepted', 'declined', 'expired')
                   ),
                   updated_at = NOW()
             WHERE c.id = $1
            "#,
        )
        .bind(clinician_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Recompute acceptance rates for every clinician whose

    pub async fn recompute_acceptance_rates_bulk(
        &self,
        clinician_ids: &[Uuid],
    ) -> Result<(), sqlx::Error> {
        if clinician_ids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            r#"
            UPDATE clinicians c
               SET acceptance_rate_pct = (
                       SELECT CASE
                                  WHEN COUNT(*) = 0 THEN NULL
                                  ELSE (
                                      COUNT(*) FILTER (WHERE a.status = 'accepted')::REAL
                                      / COUNT(*)::REAL
                                  ) * 100.0
                              END
                       FROM shift_assignments a
                       WHERE a.clinician_id = c.id
                         AND a.status IN ('accepted', 'declined', 'expired')
                   ),
                   updated_at = NOW()
             WHERE c.id = ANY($1::uuid[])
            "#,
        )
        .bind(clinician_ids)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Free-text qualifications attached to a clinician profile

    pub async fn list_clinician_qualifications(
        &self,
        clinician_id: Uuid,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT qualification
            FROM clinician_qualifications
            WHERE clinician_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(clinician_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Required qualifications for a shift (free-text tags from

    pub async fn list_shift_requirements(
        &self,
        shift_id: Uuid,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT qualification
            FROM shift_requirements
            WHERE shift_id = $1
            ORDER BY sort_order ASC, created_at ASC
            "#,
        )
        .bind(shift_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Ordered task labels for a shift (SCRUM-25 / US-09 AC-03), i.e. the
    /// `task` category of `shift_description_items`.
    pub async fn list_shift_tasks(&self, shift_id: Uuid) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT label
            FROM shift_description_items
            WHERE shift_id = $1 AND category = 'task'
            ORDER BY sort_order ASC, created_at ASC
            "#,
        )
        .bind(shift_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Average score (1–5) and count of submitted ratings for a hospital
    /// (SCRUM-25 / US-09 AC-05). Returns `(None, 0)` when the hospital has no
    /// ratings yet.
    pub async fn hospital_rating_summary(
        &self,
        hospital_id: Uuid,
    ) -> Result<(Option<f64>, i64), sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<f64>, i64)>(
            r#"
            SELECT AVG(score)::float8 AS average,
                   COUNT(*)          AS count
            FROM shift_ratings
            WHERE ratee_kind = 'hospital' AND ratee_id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Insert a clock-in approval request. Returns the record id.

    pub async fn create_clockin_approval_request(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
        latitude: Option<f64>,
        longitude: Option<f64>,
        photo_bytes: &[u8],
        photo_mime_type: Option<&str>,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO clockin_approval_requests
                (shift_id, clinician_id, latitude, longitude, photo_bytes, photo_mime_type)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .bind(latitude)
        .bind(longitude)
        .bind(photo_bytes)
        .bind(photo_mime_type)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn get_clockin_approval_request(
        &self,
        id: Uuid,
    ) -> Result<Option<crate::models::shift::ClockinApprovalRecord>, sqlx::Error> {
        sqlx::query_as::<_, crate::models::shift::ClockinApprovalRecord>(
            r#"
            SELECT id, shift_id, clinician_id, latitude, longitude,
                   status::text AS status, submitted_at, decided_at, decision_notes
            FROM clockin_approval_requests
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Whether the (shift, clinician) pair has an approved clock-in

    pub async fn has_approved_clockin_request(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM clockin_approval_requests
            WHERE shift_id = $1 AND clinician_id = $2 AND status = 'approved'
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn decide_clockin_approval_request(
        &self,
        id: Uuid,
        decided_by: Uuid,
        approve: bool,
        notes: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE clockin_approval_requests
               SET status        = CASE WHEN $2 THEN 'approved' ELSE 'denied' END::clockin_approval_status,
                   decided_at    = NOW(),decided_by    = $3,
                   decision_notes = $4,
                   updated_at    = NOW()
             WHERE id = $1 AND status = 'pending'
            "#,
        )
        .bind(id)
        .bind(approve)
        .bind(decided_by)
        .bind(notes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// count shifts the hospital currently has in "active, unfilled" states.

    pub async fn count_active_unfilled_shifts(
        &self,
        hospital_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM shifts
            WHERE hospital_id = $1
              AND status IN ('open', 'upcoming')
            "#,
        )
        .bind(hospital_id)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn create(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        created_by: Uuid,
        request: CreateShiftRequest,
    ) -> Result<Shift, sqlx::Error> {
        let id = Uuid::new_v4();
        let scheduled_end =
            request.scheduled_start + Duration::hours(request.duration_hours as i64);

        // Calculate effective rate and grand total
        let (effective_rate, grand_total) = self.calculate_compensation(&request);

        sqlx::query(
            r#"
            INSERT INTO shifts (
                id, hospital_id, role_category, role_title, specialty, department,
                shift_type, status, priority, urgency_bonus_pct,
                scheduled_start, duration_hours, scheduled_end,
                pay_type, rate_kobo_per_hour, fixed_rate_kobo, stat_bonus_kobo,
                effective_rate_kobo_per_hour, grand_total_kobo,
                shift_label, job_description, notes, created_by, broadcast_consent_confirmed,
                created_at, updated_at
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                $11, $12, $13, $14, $15, $16, $17, $18, $19,
                $20, $21, $22, $23, $24, NOW(), NOW()
            )
            "#,
        )
        .bind(id)
        .bind(hospital_id)
        .bind(&request.role_category)
        .bind(&request.role_title)
        .bind(&request.specialty)
        .bind(&request.department)
        .bind(&request.shift_type)
        .bind(ShiftStatus::Open)
        .bind(&request.priority)
        .bind(request.urgency_bonus_pct)
        .bind(request.scheduled_start)
        .bind(request.duration_hours)
        .bind(scheduled_end)
        .bind(&request.pay_type)
        .bind(request.rate_kobo_per_hour)
        .bind(request.fixed_rate_kobo)
        .bind(request.stat_bonus_kobo)
        .bind(effective_rate)
        .bind(grand_total)
        .bind(&request.shift_label)
        .bind(&request.job_description)
        .bind(&request.notes)
        .bind(created_by)
        .bind(request.broadcast_consent_confirmed)
        .execute(&mut **tx)
        .await?;

        let shift = sqlx::query_as::<_, Shift>(
            r#"
            SELECT s.*, h.name as hospital_name
            FROM shifts s
            LEFT JOIN hospitals h ON s.hospital_id = h.id
            WHERE s.id = $1
            "#,
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;

        Ok(shift)
    }

    pub async fn broadcast_shift(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        matched_count: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE shifts
            SET broadcast_at = NOW(),matched_clinicians_at_publish = $2,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(shift_id)
        .bind(matched_count)
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    pub async fn get_by_id(&self, shift_id: Uuid) -> Result<Option<Shift>, sqlx::Error> {
        sqlx::query_as::<_, Shift>(
            r#"
            SELECT
                s.id, s.hospital_id, h.name as hospital_name,
                s.role_category, s.role_title, s.specialty, s.department,
                s.shift_type, s.status, s.priority, s.urgency_bonus_pct,
                s.scheduled_start, s.duration_hours, s.scheduled_end,
                s.actual_start, s.actual_end, s.assigned_clinician_id,
                s.rate_kobo_per_hour, s.fixed_rate_kobo, s.pay_type, s.stat_bonus_kobo,
                s.effective_rate_kobo_per_hour, s.grand_total_kobo,
                s.shift_label, s.job_description, s.draft_quality_score, s.notes,
                s.created_by, s.broadcast_consent_confirmed, s.matched_clinicians_at_publish,
                s.broadcast_at, s.billing_triggered_at, s.created_at, s.updated_at
            FROM shifts s
            LEFT JOIN hospitals h ON s.hospital_id = h.id
            WHERE s.id = $1
            "#,
        )
        .bind(shift_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn list_shifts(
        &self,
        status_filter: Option<ShiftStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Shift>, sqlx::Error> {
        if let Some(status) = status_filter {
            sqlx::query_as::<_, Shift>(
                r#"
                SELECT
                    s.id, s.hospital_id, h.name as hospital_name,
                    s.role_category, s.role_title, s.specialty, s.department,
                    s.shift_type, s.status, s.priority, s.urgency_bonus_pct,
                    s.scheduled_start, s.duration_hours, s.scheduled_end,
                    s.actual_start, s.actual_end, s.assigned_clinician_id,
                    s.rate_kobo_per_hour, s.fixed_rate_kobo, s.pay_type, s.stat_bonus_kobo,
                    s.effective_rate_kobo_per_hour, s.grand_total_kobo,
                    s.shift_label, s.job_description, s.draft_quality_score, s.notes,
                    s.created_by, s.broadcast_consent_confirmed, s.matched_clinicians_at_publish,
                    s.broadcast_at, s.billing_triggered_at, s.created_at, s.updated_at
                FROM shifts s
                LEFT JOIN hospitals h ON s.hospital_id = h.id
                WHERE s.status = $1
                ORDER BY s.created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(status)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as::<_, Shift>(
                r#"
                SELECT
                    s.id, s.hospital_id, h.name as hospital_name,
                    s.role_category, s.role_title, s.specialty, s.department,
                    s.shift_type, s.status, s.priority, s.urgency_bonus_pct,
                    s.scheduled_start, s.duration_hours, s.scheduled_end,
                    s.actual_start, s.actual_end, s.assigned_clinician_id,
                    s.rate_kobo_per_hour, s.fixed_rate_kobo, s.pay_type, s.stat_bonus_kobo,
                    s.effective_rate_kobo_per_hour, s.grand_total_kobo,
                    s.shift_label, s.job_description, s.draft_quality_score, s.notes,
                    s.created_by, s.broadcast_consent_confirmed, s.matched_clinicians_at_publish,
                    s.broadcast_at, s.billing_triggered_at, s.created_at, s.updated_at
                FROM shifts s
                LEFT JOIN hospitals h ON s.hospital_id = h.id
                ORDER BY s.created_at DESC
                LIMIT $1 OFFSET $2
                "#,
            )
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
        }
    }

    pub async fn count_shifts(
        &self,
        status_filter: Option<ShiftStatus>,
    ) -> Result<i64, sqlx::Error> {
        if let Some(status) = status_filter {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT COUNT(*)
                FROM shifts
                WHERE status = $1
                "#,
            )
            .bind(status)
            .fetch_one(&self.pool)
            .await
        } else {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT COUNT(*) FROM shifts
                "#,
            )
            .fetch_one(&self.pool)
            .await
        }
    }

    pub async fn clinician_has_active_assignment(
        &self,
        clinician_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*)
            FROM shifts
            WHERE assigned_clinician_id = $1
              AND status IN ('upcoming', 'in_progress')
            "#,
        )
        .bind(clinician_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0 > 0)
    }

    pub async fn get_clinician_profile_snapshot(
        &self,
        clinician_id: Uuid,
    ) -> Result<Option<(String, String, Option<String>, Option<String>)>, sqlx::Error> {
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
            r#"
            SELECT first_name, last_name, license_number, clinician_role::text
            FROM clinicians
            WHERE id = $1
            "#,
        )
        .bind(clinician_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn create_application(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        clinician_id: Uuid,
        applicant_name: &str,
        license_number: &str,
        role: &str,
        years_experience: i32,
        experience_summary: Option<&str>,
    ) -> Result<ShiftApplication, sqlx::Error> {
        sqlx::query_as::<_, ShiftApplication>(
            r#"
            INSERT INTO shift_applications (
                shift_id, clinician_id, applicant_name, license_number,
                role, years_experience, experience_summary
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING
                id, shift_id, clinician_id, applicant_name, license_number,
                role, years_experience, experience_summary, status, created_at, updated_at
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .bind(applicant_name)
        .bind(license_number)
        .bind(role)
        .bind(years_experience)
        .bind(experience_summary)
        .fetch_one(&mut **tx)
        .await
    }

    pub async fn list_applications_for_shift(
        &self,
        shift_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ShiftApplication>, sqlx::Error> {
        sqlx::query_as::<_, ShiftApplication>(
            r#"
            SELECT id, shift_id, clinician_id, applicant_name, license_number,
                   role, years_experience, experience_summary, status, created_at, updated_at
            FROM shift_applications
            WHERE shift_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(shift_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn count_applications_for_shift(&self, shift_id: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM shift_applications
            WHERE shift_id = $1
            "#,
        )
        .bind(shift_id)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn update_application_status(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        clinician_id: Uuid,
        status: ShiftApplicationStatus,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE shift_applications
            SET status = $3,
                updated_at = NOW()
            WHERE shift_id = $1 AND clinician_id = $2
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .bind(status)
        .execute(&mut **tx)
        .await?;

        Ok(result.rows_affected())
    }

    pub async fn add_interest(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
        is_top_match: bool,
        is_waitlisted: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO shift_interests (shift_id, clinician_id, is_top_match, is_waitlisted)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .bind(is_top_match)
        .bind(is_waitlisted)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn assign_clinician(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        clinician_id: Uuid,
        new_status: ShiftStatus,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE shifts
            SET assigned_clinician_id = $2,
                status = $3,
                updated_at = NOW()
            WHERE id = $1 AND status = 'open'
            "#,
        )
        .bind(shift_id)
        .bind(clinician_id)
        .bind(new_status)
        .execute(&mut **tx)
        .await?;

        Ok(result.rows_affected())
    }

    pub async fn cancel_shift(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE shifts
            SET status = 'cancelled',
                updated_at = NOW()
            WHERE id = $1 AND status IN ('open', 'upcoming')
            "#,
        )
        .bind(shift_id)
        .execute(&mut **tx)
        .await?;

        Ok(result.rows_affected())
    }

    pub async fn reschedule_shift(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        scheduled_start: chrono::DateTime<Utc>,
        duration_hours: f32,
        scheduled_end: chrono::DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE shifts
            SET scheduled_start = $2,
                duration_hours = $3,
                scheduled_end = $4,
                updated_at = NOW()
            WHERE id = $1 AND status IN ('open', 'upcoming')
            "#,
        )
        .bind(shift_id)
        .bind(scheduled_start)
        .bind(duration_hours)
        .bind(scheduled_end)
        .execute(&mut **tx)
        .await?;

        Ok(result.rows_affected())
    }

    /// Find similar shift within time window
    pub async fn find_similar_shift(
        &self,
        hospital_id: Uuid,
        role_title: &str,
        scheduled_start: chrono::DateTime<Utc>,
        created_after: chrono::DateTime<Utc>,
    ) -> Result<Option<Shift>, sqlx::Error> {
        sqlx::query_as::<_, Shift>(
            r#"
            SELECT
                s.id, s.hospital_id, h.name as hospital_name,
                s.role_category, s.role_title, s.specialty, s.department,
                s.shift_type, s.status, s.priority, s.urgency_bonus_pct,
                s.scheduled_start, s.duration_hours, s.scheduled_end,
                s.actual_start, s.actual_end, s.assigned_clinician_id,
                s.rate_kobo_per_hour, s.fixed_rate_kobo, s.pay_type, s.stat_bonus_kobo,
                s.effective_rate_kobo_per_hour, s.grand_total_kobo,
                s.shift_label, s.job_description, s.draft_quality_score, s.notes,
                s.created_by, s.broadcast_consent_confirmed, s.matched_clinicians_at_publish,
                s.broadcast_at, s.billing_triggered_at, s.created_at, s.updated_at
            FROM shifts s
            LEFT JOIN hospitals h ON s.hospital_id = h.id
            WHERE s.hospital_id = $1
              AND s.role_title = $2
              AND s.scheduled_start = $3
              AND s.created_at > $4
              AND s.status = 'open'
            ORDER BY s.created_at DESC
            LIMIT 1
            "#,
        )
        .bind(hospital_id)
        .bind(role_title)
        .bind(scheduled_start)
        .bind(created_after)
        .fetch_optional(&self.pool)
        .await
    }

    /// AC-04 / F1-F15: Store the auto-generated virtual consultation link

    pub async fn update_virtual_link(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        virtual_link: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE shifts
            SET virtual_link = $2,
                updated_at   = NOW()
            WHERE id = $1
            "#,
        )
        .bind(shift_id)
        .bind(virtual_link)
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    /// F1-F12 / F1-F13 / F1-F14 — atomically persist the shift's

    pub async fn insert_shift_description_and_requirements(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        shift_id: Uuid,
        tasks: &[String],
        equipment: &[String],
        requirements: &[String],
    ) -> Result<(), sqlx::Error> {
        for (idx, label) in tasks.iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO shift_description_items (shift_id, category, label, sort_order)
                VALUES ($1, 'task', $2, $3)
                "#,
            )
            .bind(shift_id)
            .bind(label)
            .bind(idx as i16)
            .execute(&mut **tx)
            .await?;
        }

        for (idx, label) in equipment.iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO shift_description_items (shift_id, category, label, sort_order)
                VALUES ($1, 'equipment', $2, $3)
                "#,
            )
            .bind(shift_id)
            .bind(label)
            .bind(idx as i16)
            .execute(&mut **tx)
            .await?;
        }

        for (idx, qualification) in requirements.iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO shift_requirements (shift_id, qualification, sort_order)
                VALUES ($1, $2, $3)
                "#,
            )
            .bind(shift_id)
            .bind(qualification)
            .bind(idx as i16)
            .execute(&mut **tx)
            .await?;
        }

        Ok(())
    }

    fn calculate_compensation(&self, request: &CreateShiftRequest) -> (Option<i64>, Option<i64>) {
        let base_amount = match request.pay_type {
            PayType::HourlyRate => request
                .rate_kobo_per_hour
                .map(|rate| (rate as f64 * request.duration_hours as f64) as i64),
            PayType::FixedRate => request.fixed_rate_kobo,
        };

        let effective_rate = match request.pay_type {
            PayType::HourlyRate => request.rate_kobo_per_hour.map(|rate| {
                if let Some(bonus_pct) = request.urgency_bonus_pct {
                    rate + (rate * bonus_pct as i64 / 100)
                } else {
                    rate
                }
            }),
            PayType::FixedRate => None,
        };

        let grand_total = base_amount.map(|base| {
            let stat_bonus = request.stat_bonus_kobo.unwrap_or(0);
            base + stat_bonus
        });

        (effective_rate, grand_total)
    }
}

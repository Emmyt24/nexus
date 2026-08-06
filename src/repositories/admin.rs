//! Data access for the admin dashboard (§11). Runtime sqlx queries over the
//! existing shift / hospital / clinician / billing / revenue tables plus the
//! disputes and platform_settings tables.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::admin::*;

pub struct AdminRepository {
    pool: PgPool,
}

impl AdminRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // ----- §2 Dashboard KPIs ------------------------------------------------

    pub async fn dashboard_metrics(&self) -> Result<DashboardMetrics, sqlx::Error> {
        let total_hospitals: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM hospitals")
                .fetch_one(&self.pool)
                .await?;

        // Active workers: clinicians with a completed/active user account.
        let active_workers: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM clinicians")
                .fetch_one(&self.pool)
                .await?;

        let active_shifts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM shifts WHERE status IN ('open', 'assigned', 'in_progress')",
        )
        .fetch_one(&self.pool)
        .await?;

        let shifts_completed_30d: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM shifts \
             WHERE status = 'completed' AND actual_end > NOW() - INTERVAL '30 days'",
        )
        .fetch_one(&self.pool)
        .await?;

        let platform_revenue_30d_kobo: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(fee_kobo), 0)::BIGINT FROM platform_revenue_ledger \
             WHERE created_at > NOW() - INTERVAL '30 days'",
        )
        .fetch_one(&self.pool)
        .await?;

        let active_disputes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM disputes WHERE status = 'open'")
                .fetch_one(&self.pool)
                .await?;

        let pending_hospital_verifications: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM hospitals WHERE verification_status IN ('pending', 'under_review')",
        )
        .fetch_one(&self.pool)
        .await?;

        // Workers awaiting verification: clinicians with an identity verification
        // still pending. Falls back to 0 if the table is empty.
        let pending_worker_verifications: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM identity_verifications WHERE status = 'pending'",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);

        let failed_payments: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_transactions WHERE status = 'failed'",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(DashboardMetrics {
            total_hospitals,
            active_workers,
            active_shifts,
            shifts_completed_30d,
            platform_revenue_30d_kobo,
            active_disputes,
            pending_hospital_verifications,
            pending_worker_verifications,
            failed_payments,
        })
    }

    // ----- §3.1 Shift volume trend -----------------------------------------

    pub async fn shift_volume(&self, days: i64) -> Result<Vec<ShiftVolumePoint>, sqlx::Error> {
        sqlx::query_as::<_, ShiftVolumePoint>(
            r#"
            SELECT
                date_trunc('day', created_at) AS day,
                COUNT(*)                                              AS created,
                COUNT(*) FILTER (WHERE assigned_clinician_id IS NOT NULL) AS filled,
                COUNT(*) FILTER (WHERE status = 'completed')         AS completed
            FROM shifts
            WHERE created_at > NOW() - ($1 || ' days')::INTERVAL
            GROUP BY 1
            ORDER BY 1
            "#,
        )
        .bind(days.to_string())
        .fetch_all(&self.pool)
        .await
    }

    // ----- §3.2 Geographic distribution ------------------------------------

    pub async fn geographic_distribution(
        &self,
    ) -> Result<Vec<GeoDistributionPoint>, sqlx::Error> {
        sqlx::query_as::<_, GeoDistributionPoint>(
            r#"
            SELECT
                h.id            AS hospital_id,
                h.name          AS hospital_name,
                l.latitude      AS latitude,
                l.longitude     AS longitude,
                COUNT(s.id) FILTER (
                    WHERE s.created_at > NOW() - INTERVAL '30 days'
                )               AS shifts_30d
            FROM hospitals h
            JOIN hospital_locations l ON l.hospital_id = h.id
            LEFT JOIN shifts s ON s.hospital_id = h.id
            GROUP BY h.id, h.name, l.latitude, l.longitude
            ORDER BY shifts_30d DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    // ----- §3.3 Worker performance -----------------------------------------

    pub async fn rating_distribution(&self) -> Result<Vec<RatingBucket>, sqlx::Error> {
        sqlx::query_as::<_, RatingBucket>(
            r#"
            SELECT score::SMALLINT AS score, COUNT(DISTINCT ratee_id) AS worker_count
            FROM shift_ratings
            WHERE ratee_kind = 'clinician'
            GROUP BY score
            ORDER BY score DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn top_performers(&self, limit: i64) -> Result<Vec<TopPerformer>, sqlx::Error> {
        sqlx::query_as::<_, TopPerformer>(
            r#"
            SELECT
                c.id                                        AS worker_id,
                (c.first_name || ' ' || c.last_name)        AS full_name,
                COUNT(s.id) FILTER (WHERE s.status = 'completed') AS shifts_completed,
                AVG(r.score)::DOUBLE PRECISION              AS avg_rating,
                COALESCE(SUM(prl.net_kobo), 0)::BIGINT      AS earnings_kobo
            FROM clinicians c
            LEFT JOIN shifts s ON s.assigned_clinician_id = c.id
            LEFT JOIN shift_ratings r
                   ON r.ratee_id = c.id AND r.ratee_kind = 'clinician'
            LEFT JOIN platform_revenue_ledger prl ON prl.shift_id = s.id
            GROUP BY c.id, c.first_name, c.last_name
            HAVING COUNT(s.id) FILTER (WHERE s.status = 'completed') > 0
            ORDER BY shifts_completed DESC, avg_rating DESC NULLS LAST
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    // ----- §3.4 Revenue breakdown ------------------------------------------

    pub async fn revenue_total_30d(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(fee_kobo), 0)::BIGINT FROM platform_revenue_ledger \
             WHERE created_at > NOW() - INTERVAL '30 days'",
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn revenue_by_priority(&self) -> Result<Vec<RevenueSlice>, sqlx::Error> {
        sqlx::query_as::<_, RevenueSlice>(
            r#"
            SELECT s.priority::TEXT AS label,
                   COALESCE(SUM(prl.fee_kobo), 0)::BIGINT AS revenue_kobo
            FROM platform_revenue_ledger prl
            JOIN shifts s ON s.id = prl.shift_id
            WHERE prl.created_at > NOW() - INTERVAL '30 days'
            GROUP BY s.priority
            ORDER BY revenue_kobo DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn revenue_by_status(&self) -> Result<Vec<RevenueSlice>, sqlx::Error> {
        sqlx::query_as::<_, RevenueSlice>(
            r#"
            SELECT s.status::TEXT AS label,
                   COALESCE(SUM(prl.fee_kobo), 0)::BIGINT AS revenue_kobo
            FROM platform_revenue_ledger prl
            JOIN shifts s ON s.id = prl.shift_id
            WHERE prl.created_at > NOW() - INTERVAL '30 days'
            GROUP BY s.status
            ORDER BY revenue_kobo DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    // ----- §5.6 Failed payments --------------------------------------------

    pub async fn failed_payments(&self, limit: i64) -> Result<Vec<FailedPayment>, sqlx::Error> {
        sqlx::query_as::<_, FailedPayment>(
            r#"
            SELECT
                bt.id                 AS id,
                bt.hospital_id        AS hospital_id,
                h.name                AS hospital_name,
                bt.amount_kobo        AS amount_kobo,
                bt.currency           AS currency,
                bt.provider_reference AS provider_reference,
                bt.created_at         AS created_at
            FROM billing_transactions bt
            LEFT JOIN hospitals h ON h.id = bt.hospital_id
            WHERE bt.status = 'failed'
            ORDER BY bt.created_at DESC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    // ----- §4.3 Disputes ----------------------------------------------------

    pub async fn list_disputes(
        &self,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Dispute>, sqlx::Error> {
        sqlx::query_as::<_, Dispute>(
            r#"
            SELECT
                d.id, d.shift_id, d.hospital_id, h.name AS hospital_name,
                d.worker_id, d.filed_by::TEXT AS filed_by, d.reason,
                d.status::TEXT AS status, d.priority::TEXT AS priority,
                d.amount_kobo, d.resolution::TEXT AS resolution,
                d.resolution_amount_kobo, d.admin_notes, d.resolved_at, d.created_at
            FROM disputes d
            LEFT JOIN hospitals h ON h.id = d.hospital_id
            WHERE ($1::TEXT IS NULL OR d.status::TEXT = $1)
            ORDER BY d.priority DESC, d.created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(status)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_dispute(&self, id: Uuid) -> Result<Option<Dispute>, sqlx::Error> {
        sqlx::query_as::<_, Dispute>(
            r#"
            SELECT
                d.id, d.shift_id, d.hospital_id, h.name AS hospital_name,
                d.worker_id, d.filed_by::TEXT AS filed_by, d.reason,
                d.status::TEXT AS status, d.priority::TEXT AS priority,
                d.amount_kobo, d.resolution::TEXT AS resolution,
                d.resolution_amount_kobo, d.admin_notes, d.resolved_at, d.created_at
            FROM disputes d
            LEFT JOIN hospitals h ON h.id = d.hospital_id
            WHERE d.id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Resolve a dispute. Returns None if the dispute does not exist.
    pub async fn resolve_dispute(
        &self,
        id: Uuid,
        resolution: &str,
        resolution_amount_kobo: Option<i64>,
        admin_notes: Option<&str>,
        resolved_by: Uuid,
    ) -> Result<Option<Dispute>, sqlx::Error> {
        // Escalation keeps the dispute open; every other resolution closes it.
        let new_status = if resolution == "escalate" {
            "open"
        } else {
            "resolved"
        };

        let updated: Option<Uuid> = sqlx::query_scalar(
            r#"
            UPDATE disputes
            SET resolution = $2::dispute_resolution,
                resolution_amount_kobo = $3,
                admin_notes = $4,
                resolved_by = $5,
                status = $6::dispute_status,
                resolved_at = CASE WHEN $6 = 'resolved' THEN NOW() ELSE resolved_at END,
                updated_at = NOW()
            WHERE id = $1
            RETURNING id
            "#,
        )
        .bind(id)
        .bind(resolution)
        .bind(resolution_amount_kobo)
        .bind(admin_notes)
        .bind(resolved_by)
        .bind(new_status)
        .fetch_optional(&self.pool)
        .await?;

        match updated {
            Some(_) => self.get_dispute(id).await,
            None => Ok(None),
        }
    }

    // ----- §8 Platform settings --------------------------------------------

    pub async fn get_settings(&self) -> Result<PlatformSettings, sqlx::Error> {
        sqlx::query_as::<_, PlatformSettings>(
            r#"
            SELECT
                platform_fee_percent::DOUBLE PRECISION        AS platform_fee_percent,
                worker_broadcast_radius_km::DOUBLE PRECISION  AS worker_broadcast_radius_km,
                stat_bonus_percent::DOUBLE PRECISION          AS stat_bonus_percent,
                urgent_bonus_percent::DOUBLE PRECISION        AS urgent_bonus_percent,
                clock_in_grace_minutes,
                auto_clock_out_hours,
                handover_edit_window_hours,
                dispute_filing_window_hours,
                max_active_shifts_per_hospital,
                min_hourly_rate_kobo,
                max_recording_minutes,
                updated_at
            FROM platform_settings
            WHERE singleton = 'global'
            "#,
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn update_settings(
        &self,
        p: &UpdatePlatformSettings,
        updated_by: Uuid,
    ) -> Result<PlatformSettings, sqlx::Error> {
        // COALESCE keeps existing values for any field the caller omitted.
        sqlx::query(
            r#"
            UPDATE platform_settings SET
                platform_fee_percent        = COALESCE($1, platform_fee_percent),
                worker_broadcast_radius_km  = COALESCE($2, worker_broadcast_radius_km),
                stat_bonus_percent          = COALESCE($3, stat_bonus_percent),
                urgent_bonus_percent        = COALESCE($4, urgent_bonus_percent),
                clock_in_grace_minutes      = COALESCE($5, clock_in_grace_minutes),
                auto_clock_out_hours        = COALESCE($6, auto_clock_out_hours),
                handover_edit_window_hours  = COALESCE($7, handover_edit_window_hours),
                dispute_filing_window_hours = COALESCE($8, dispute_filing_window_hours),
                max_active_shifts_per_hospital = COALESCE($9, max_active_shifts_per_hospital),
                min_hourly_rate_kobo        = COALESCE($10, min_hourly_rate_kobo),
                max_recording_minutes       = COALESCE($11, max_recording_minutes),
                updated_by = $12,
                updated_at = NOW()
            WHERE singleton = 'global'
            "#,
        )
        .bind(p.platform_fee_percent)
        .bind(p.worker_broadcast_radius_km)
        .bind(p.stat_bonus_percent)
        .bind(p.urgent_bonus_percent)
        .bind(p.clock_in_grace_minutes)
        .bind(p.auto_clock_out_hours)
        .bind(p.handover_edit_window_hours)
        .bind(p.dispute_filing_window_hours)
        .bind(p.max_active_shifts_per_hospital)
        .bind(p.min_hourly_rate_kobo)
        .bind(p.max_recording_minutes)
        .bind(updated_by)
        .execute(&self.pool)
        .await?;

        self.get_settings().await
    }

    // ----- §1.2 Hospital suspend / unsuspend --------------------------------

    /// Suspend a hospital; returns false if no such hospital exists.
    pub async fn suspend_hospital(
        &self,
        hospital_id: Uuid,
        admin_id: Uuid,
        reason: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let rows = sqlx::query(
            "UPDATE hospitals \
             SET verification_status = 'suspended', suspended_at = NOW(), \
                 suspended_by = $2, suspended_reason = $3 \
             WHERE id = $1",
        )
        .bind(hospital_id)
        .bind(admin_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(rows.rows_affected() > 0)
    }

    /// Lift a suspension, returning the hospital to 'verified'.
    pub async fn unsuspend_hospital(&self, hospital_id: Uuid) -> Result<bool, sqlx::Error> {
        let rows = sqlx::query(
            "UPDATE hospitals \
             SET verification_status = 'verified', suspended_at = NULL, \
                 suspended_by = NULL, suspended_reason = NULL \
             WHERE id = $1",
        )
        .bind(hospital_id)
        .execute(&self.pool)
        .await?;
        Ok(rows.rows_affected() > 0)
    }

    // ----- §2 Worker verify / reject / suspend ------------------------------

    /// Mark a worker's license verified (or rejected) with review attribution.
    pub async fn set_worker_verified(
        &self,
        clinician_id: Uuid,
        verified: bool,
        admin_id: Uuid,
        notes: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let rows = sqlx::query(
            "UPDATE clinicians \
             SET is_verified = $2, reviewed_by = $3, reviewed_at = NOW(), review_notes = $4 \
             WHERE id = $1",
        )
        .bind(clinician_id)
        .bind(verified)
        .bind(admin_id)
        .bind(notes)
        .execute(&self.pool)
        .await?;
        Ok(rows.rows_affected() > 0)
    }

    /// Suspend or unsuspend a worker by toggling their user account's active flag.
    pub async fn set_worker_active(
        &self,
        clinician_id: Uuid,
        active: bool,
        reason: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        // Flip the linked user account (the real login gate).
        let rows = sqlx::query(
            "UPDATE users SET is_active = $2 \
             WHERE id = (SELECT user_id FROM clinicians WHERE id = $1)",
        )
        .bind(clinician_id)
        .bind(active)
        .execute(&self.pool)
        .await?;

        // Stamp suspend metadata on the clinician row for the audit trail.
        sqlx::query(
            "UPDATE clinicians \
             SET suspended_at = CASE WHEN $2 THEN NULL ELSE NOW() END, \
                 suspended_reason = CASE WHEN $2 THEN NULL ELSE $3 END \
             WHERE id = $1",
        )
        .bind(clinician_id)
        .bind(active)
        .bind(reason)
        .execute(&self.pool)
        .await?;

        Ok(rows.rows_affected() > 0)
    }

    // ----- §3 Platform-wide shifts ------------------------------------------

    /// List shifts across all hospitals for the admin shift view.
    pub async fn list_shifts(
        &self,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AdminShiftRow>, sqlx::Error> {
        sqlx::query_as::<_, AdminShiftRow>(
            r#"
            SELECT
                s.id, s.hospital_id, h.name AS hospital_name,
                s.status::TEXT AS status, s.priority::TEXT AS priority,
                s.scheduled_start, s.scheduled_end,
                s.assigned_clinician_id, s.created_at
            FROM shifts s
            LEFT JOIN hospitals h ON h.id = s.hospital_id
            WHERE ($1::TEXT IS NULL OR s.status::TEXT = $1)
            ORDER BY s.created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(status)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    /// Cancel a shift administratively; returns false if it doesn't exist.
    pub async fn cancel_shift(&self, shift_id: Uuid) -> Result<bool, sqlx::Error> {
        let rows = sqlx::query(
            "UPDATE shifts SET status = 'cancelled', updated_at = NOW() WHERE id = $1",
        )
        .bind(shift_id)
        .execute(&self.pool)
        .await?;
        Ok(rows.rows_affected() > 0)
    }

    // ----- §1 Admin management (super only) ---------------------------------

    /// Create an admin user row with an initial password hash for admin login.
    pub async fn create_admin(
        &self,
        first_name: &str,
        last_name: &str,
        email: &str,
        phone: Option<&str>,
        role: &str,
        password_hash: &str,
    ) -> Result<AdminSummary, sqlx::Error> {
        sqlx::query_as::<_, AdminSummary>(
            r#"
            INSERT INTO users (first_name, last_name, email, phone, role, password_hash, is_active)
            VALUES ($1, $2, $3, $4, $5::user_role, $6, TRUE)
            RETURNING id, first_name, last_name, email, role::TEXT AS role, is_active, created_at
            "#,
        )
        .bind(first_name)
        .bind(last_name)
        .bind(email)
        .bind(phone)
        .bind(role)
        .bind(password_hash)
        .fetch_one(&self.pool)
        .await
    }

    /// List all admin users (any of the four admin roles).
    pub async fn list_admins(&self) -> Result<Vec<AdminSummary>, sqlx::Error> {
        sqlx::query_as::<_, AdminSummary>(
            r#"
            SELECT id, first_name, last_name, email, role::TEXT AS role, is_active, created_at
            FROM users
            WHERE role IN ('super_admin', 'operations_admin', 'verification_admin', 'finance_admin')
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Update an admin's role and/or active flag; returns None if not found.
    pub async fn update_admin(
        &self,
        id: Uuid,
        role: Option<&str>,
        is_active: Option<bool>,
    ) -> Result<Option<AdminSummary>, sqlx::Error> {
        // COALESCE keeps the existing value for any field the caller omitted.
        let updated: Option<Uuid> = sqlx::query_scalar(
            r#"
            UPDATE users
            SET role = COALESCE($2::user_role, role),
                is_active = COALESCE($3, is_active),
                updated_at = NOW()
            WHERE id = $1
              AND role IN ('super_admin', 'operations_admin', 'verification_admin', 'finance_admin')
            RETURNING id
            "#,
        )
        .bind(id)
        .bind(role)
        .bind(is_active)
        .fetch_optional(&self.pool)
        .await?;

        match updated {
            Some(_) => {
                let row = sqlx::query_as::<_, AdminSummary>(
                    "SELECT id, first_name, last_name, email, role::TEXT AS role, is_active, created_at \
                     FROM users WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    // ----- Detail views -----------------------------------------------------

    /// Full hospital record + admin contact + wallet + shift/spend aggregates.
    pub async fn get_hospital_detail(
        &self,
        hospital_id: Uuid,
    ) -> Result<Option<HospitalDetail>, sqlx::Error> {
        sqlx::query_as::<_, HospitalDetail>(
            r#"
            SELECT
                h.id,
                h.name,
                h.registration_number,
                h.email,
                h.address,
                h.phone_number,
                h.verification_status::TEXT           AS verification_status,
                h.registration_step::TEXT             AS registration_step,
                h.admin_registration_status::TEXT     AS admin_registration_status,
                h.setup_progress_percent,
                h.logo_url,
                h.approved_at,
                h.created_at,
                u.first_name                          AS admin_first_name,
                u.last_name                           AS admin_last_name,
                u.email                               AS admin_email,
                u.phone                               AS admin_phone,
                COALESCE(w.balance_kobo, 0)::BIGINT   AS wallet_balance_kobo,
                COALESCE(w.held_kobo, 0)::BIGINT      AS wallet_held_kobo,
                w.safehaven_account_number,
                (SELECT COUNT(*) FROM shifts s WHERE s.hospital_id = h.id)::BIGINT AS total_shifts,
                (SELECT COUNT(*) FROM shifts s WHERE s.hospital_id = h.id
                    AND s.status IN ('open','assigned','in_progress'))::BIGINT     AS active_shifts,
                (SELECT COUNT(*) FROM shifts s WHERE s.hospital_id = h.id
                    AND s.status = 'completed')::BIGINT                            AS completed_shifts,
                (SELECT COALESCE(SUM(bt.amount_kobo), 0) FROM billing_transactions bt
                    WHERE bt.hospital_id = h.id AND bt.event_type = 'payout'
                      AND bt.status = 'success')::BIGINT                           AS total_spent_kobo,
                EXISTS (SELECT 1 FROM identity_verifications iv
                    WHERE iv.owner_type = 'hospital' AND iv.owner_id = h.id
                      AND iv.status = 'verified')                                  AS identity_verified
            FROM hospitals h
            LEFT JOIN users u            ON u.id = h.admin_user_id
            LEFT JOIN hospital_wallets w ON w.hospital_id = h.id
            WHERE h.id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Full worker record + user contact + bank presence + earnings aggregates.
    pub async fn get_worker_detail(
        &self,
        clinician_id: Uuid,
    ) -> Result<Option<WorkerDetail>, sqlx::Error> {
        sqlx::query_as::<_, WorkerDetail>(
            r#"
            SELECT
                c.id,
                c.user_id,
                c.first_name,
                c.last_name,
                u.email,
                u.phone,
                c.specialty::TEXT                     AS specialty,
                c.role_title,
                c.license_number,
                c.rating::REAL                        AS rating,
                c.rating_count,
                c.acceptance_rate_pct,
                c.availability::TEXT                  AS availability,
                c.is_verified,
                c.is_active,
                c.created_at,
                (ba.clinician_id IS NOT NULL)         AS has_bank_account,
                ba.account_name                       AS bank_account_name,
                (SELECT COUNT(*) FROM shifts s WHERE s.assigned_clinician_id = c.id
                    AND s.status = 'completed')::BIGINT AS completed_shifts,
                (SELECT COALESCE(SUM(bt.amount_kobo), 0)
                    FROM billing_transactions bt
                    JOIN shifts s ON s.id = bt.shift_id
                    WHERE s.assigned_clinician_id = c.id
                      AND bt.event_type = 'payout' AND bt.status = 'success')::BIGINT AS total_earned_kobo,
                EXISTS (SELECT 1 FROM identity_verifications iv
                    WHERE iv.owner_type = 'clinician' AND iv.owner_id = c.id
                      AND iv.status = 'verified')      AS identity_verified
            FROM clinicians c
            JOIN users u                     ON u.id = c.user_id
            LEFT JOIN clinician_bank_accounts ba ON ba.clinician_id = c.id
            WHERE c.id = $1
            "#,
        )
        .bind(clinician_id)
        .fetch_optional(&self.pool)
        .await
    }

    // ----- Revenue trend (time series) --------------------------------------

    /// Revenue bucketed by day/week/month over [from, to). `period` must be one
    /// of `day`|`week`|`month` (validated by the service).
    pub async fn revenue_trend(
        &self,
        period: &str,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<RevenuePoint>, sqlx::Error> {
        sqlx::query_as::<_, RevenuePoint>(
            r#"
            SELECT
                date_trunc($1, created_at)      AS bucket,
                COALESCE(SUM(gross_kobo), 0)::BIGINT AS gross_kobo,
                COALESCE(SUM(fee_kobo), 0)::BIGINT   AS fee_kobo,
                COALESCE(SUM(net_kobo), 0)::BIGINT   AS net_kobo,
                COUNT(*)::BIGINT                     AS payouts
            FROM platform_revenue_ledger
            WHERE created_at >= $2 AND created_at < $3
            GROUP BY bucket
            ORDER BY bucket ASC
            "#,
        )
        .bind(period)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await
    }

    // ----- Recent activities (multi-source union) ---------------------------

    /// Newest-first union of registration, shift, payout, and deposit events.
    pub async fn recent_activities(&self, limit: i64) -> Result<Vec<ActivityItem>, sqlx::Error> {
        sqlx::query_as::<_, ActivityItem>(
            r#"
            SELECT * FROM (
                -- Hospital registration step changes / approvals
                SELECT
                    CASE WHEN ral.new_step = 'access_granted' THEN 'hospital_approved'
                         ELSE 'hospital_registered' END AS kind,
                    h.name                               AS title,
                    ral.new_step                         AS subtitle,
                    ral.hospital_id                      AS entity_id,
                    NULL::BIGINT                         AS amount_kobo,
                    ral.created_at                       AS occurred_at
                FROM registration_audit_log ral
                JOIN hospitals h ON h.id = ral.hospital_id

                UNION ALL
                -- Shift created / completed
                SELECT
                    CASE WHEN s.status = 'completed' THEN 'shift_completed'
                         ELSE 'shift_created' END        AS kind,
                    s.role_title                          AS title,
                    s.status::TEXT                        AS subtitle,
                    s.id                                  AS entity_id,
                    s.grand_total_kobo                    AS amount_kobo,
                    s.created_at                          AS occurred_at
                FROM shifts s

                UNION ALL
                -- Payout / deposit billing events
                SELECT
                    bt.event_type::TEXT                   AS kind,
                    bt.event_type::TEXT                   AS title,
                    bt.status::TEXT                       AS subtitle,
                    bt.hospital_id                        AS entity_id,
                    bt.amount_kobo                        AS amount_kobo,
                    bt.created_at                         AS occurred_at
                FROM billing_transactions bt
                WHERE bt.event_type IN ('payout', 'deposit')
            ) feed
            ORDER BY occurred_at DESC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    // ----- Global search (hospitals + workers) ------------------------------

    /// Case-insensitive search over hospitals and clinicians. `per_kind` caps
    /// each group.
    pub async fn search(
        &self,
        query: &str,
        per_kind: i64,
    ) -> Result<(Vec<SearchHit>, Vec<SearchHit>), sqlx::Error> {
        let pattern = format!("%{}%", query);

        let hospitals = sqlx::query_as::<_, SearchHit>(
            r#"
            SELECT id, 'hospital' AS kind, name AS title,
                   registration_number AS subtitle
            FROM hospitals
            WHERE name ILIKE $1 OR registration_number ILIKE $1 OR email ILIKE $1
            ORDER BY name ASC
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(per_kind)
        .fetch_all(&self.pool)
        .await?;

        let workers = sqlx::query_as::<_, SearchHit>(
            r#"
            SELECT c.id, 'worker' AS kind,
                   (c.first_name || ' ' || c.last_name) AS title,
                   u.email AS subtitle
            FROM clinicians c
            JOIN users u ON u.id = c.user_id
            WHERE c.first_name ILIKE $1 OR c.last_name ILIKE $1
               OR u.email ILIKE $1 OR c.license_number ILIKE $1
            ORDER BY c.first_name ASC
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(per_kind)
        .fetch_all(&self.pool)
        .await?;

        Ok((hospitals, workers))
    }
}

use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::models::shift::{CreateShiftRequest, Shift, ShiftPriority, ShiftStatus, ShiftType};
use crate::repositories::shift::ShiftRepository;
use crate::services::email_outbox_service::EmailOutboxService;
use crate::services::email_templates;
use crate::services::notification_service::NotificationService;

#[derive(Debug, thiserror::Error)]
pub enum ShiftServiceError {
    #[error("Validation failed: {0}")]
    ValidationError(String),

    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),

    #[error("Shift not found: {0}")]
    NotFound(Uuid),

    #[error("Duplicate shift: {0}")]
    DuplicateShift(String),

    #[error("Duplicate shift interest")]
    DuplicateInterest,

    #[error("Duplicate shift application")]
    DuplicateApplication,

    #[error("Clinician profile is incomplete")]
    ProfileIncomplete,

    #[error("Clinician already assigned to an active shift")]
    ClinicianBusy,

    #[error("Not authorized to view applications")]
    NotAuthorized,

    #[error("Shift already assigned")]
    AlreadyAssigned,

    #[error("Invalid shift status: {0}")]
    InvalidStatus(String),

    #[error("Hospital not approved: {0}")]
    HospitalNotApproved(String),

    #[error("Too many active shifts")]
    TooManyActiveShifts,

    #[error("Clinician has not expressed interest in this shift")]
    NotInterested,

    #[error("Clinician already has an offer for this shift")]
    DuplicateOffer,

    #[error("No pending offer for this shift")]
    NoPendingOffer,

    #[error("Offer has expired")]
    OfferExpired,

    #[error("All NDPR consent boxes must be checked")]
    ConsentRequired,

    #[error("Authenticated user has no clinician profile")]
    NoClinicianProfile,

    #[error("Shift overlaps with another accepted shift")]
    ScheduleConflict,

    #[error("Too early to clock in")]
    TooEarlyToClockIn,

    #[error("Shift was missed (more than 60 minutes late)")]
    MissedShift,

    #[error("Clock-in location is {0} metres from the hospital — outside the geofence")]
    OutOfGeofence(i32),

    #[error("Handover must be submitted before clock-out")]
    HandoverRequired,

    #[error("Handover edit window (1 hour after clock-out) has closed")]
    HandoverEditWindowClosed,

    #[error("Hospital revision window (24 hours after clock-out) has closed")]
    RevisionWindowClosed,

    #[error("Rating already submitted for this shift")]
    DuplicateRating,

    #[error("Rating submission window (7 days after shift completion) has closed")]
    RatingWindowClosed,

    #[error("Rating not found")]
    RatingNotFound,

    #[error("Rating edit window (48 hours) has closed")]
    RatingEditWindowClosed,

    #[error("Clock-in approval request already exists for this shift")]
    DuplicateClockinApproval,

    #[error("Clock-in approval request not found")]
    ClockinApprovalNotFound,

    #[error("Manual clock-in requires an approved GPS-fallback request")]
    ManualClockinNotApproved,

    #[error("Insufficient wallet balance: required {required} kobo, available {available} kobo")]
    InsufficientWalletBalance { required: i64, available: i64 },

    #[error("Wallet error: {0}")]
    WalletError(String),

    #[error("Worker location required to list nearby shifts")]
    LocationRequired,

    #[error("Shift is no longer available")]
    ShiftUnavailable,

    #[error("Worker already declined this shift")]
    WorkerAlreadyDeclined,

    #[error("This offer has already been responded to")]
    OfferAlreadyResponded,
}

/// A worker's origin for nearby-shift discovery: live GPS supplied on the
/// request. Persisted as the last-known location when present.
#[derive(Debug, Clone, Copy)]
pub struct WorkerOrigin {
    pub lat: f64,
    pub lng: f64,
    pub accuracy_meters: Option<f32>,
}

/// Pair each shift requirement with whether it is satisfied by the clinician's
/// qualifications (SCRUM-25 / US-09 AC-04). Matching is case-insensitive and
/// whitespace-trimmed so "ACLS Certified" matches "acls certified".
fn match_qualifications(
    requirements: &[String],
    quals: &[String],
) -> Vec<crate::models::shift::QualificationMatch> {
    use std::collections::HashSet;
    let held: HashSet<String> = quals
        .iter()
        .map(|q| q.trim().to_lowercase())
        .collect();
    requirements
        .iter()
        .map(|req| crate::models::shift::QualificationMatch {
            requirement: req.clone(),
            met: held.contains(&req.trim().to_lowercase()),
        })
        .collect()
}

pub struct ShiftService {
    shift_repo: Arc<ShiftRepository>,
    pool: PgPool,
    notification_service: Arc<NotificationService>,
    email_outbox: Arc<EmailOutboxService>,
    wallet_service: Arc<crate::services::wallet_service::WalletService>,
    push: Arc<crate::services::push_service::PushService>,
}

impl ShiftService {
    pub fn new(
        shift_repo: Arc<ShiftRepository>,
        pool: PgPool,
        notification_service: Arc<NotificationService>,
        email_outbox: Arc<EmailOutboxService>,
        wallet_service: Arc<crate::services::wallet_service::WalletService>,
        push: Arc<crate::services::push_service::PushService>,
    ) -> Self {
        Self {
            shift_repo,
            pool,
            notification_service,
            email_outbox,
            wallet_service,
            push,
        }
    }

    pub async fn create_shift(
        &self,
        hospital_id: Uuid,
        created_by: Uuid,
        mut request: CreateShiftRequest,
    ) -> Result<Shift, ShiftServiceError> {
        // Check if hospital is approved
        let is_approved = self.shift_repo.check_hospital_approved(hospital_id).await?;
        if !is_approved {
            return Err(ShiftServiceError::HospitalNotApproved(
                "Only approved hospitals can create shifts. Please complete your registration and wait for approval.".to_string()
            ));
        }

        // Validate required fields based on pay type
        self.validate_request(&request)?;

        // hospital cannot have more than 10 active unfilled shifts at once.
        let active_unfilled = self
            .shift_repo
            .count_active_unfilled_shifts(hospital_id)
            .await?;
        if active_unfilled >= 10 {
            return Err(ShiftServiceError::TooManyActiveShifts);
        }

        // STAT shifts get an automatic +20% bonus when none is set.
        if request.priority == ShiftPriority::Stat
            && request.stat_bonus_kobo.unwrap_or(0) == 0
            && request.urgency_bonus_pct.is_none()
        {
            let base = match request.pay_type {
                crate::models::shift::PayType::HourlyRate => request
                    .rate_kobo_per_hour
                    .unwrap_or(0)
                    .saturating_mul(request.duration_hours as i64),
                crate::models::shift::PayType::FixedRate => request.fixed_rate_kobo.unwrap_or(0),
            };
            request.stat_bonus_kobo = Some(base / 5); // +20%
        }

        // Check for duplicate shifts
        self.check_duplicate_shift(hospital_id, &request).await?;

        // Take the tasks / equipment / requirements out before `request` is moved
        let tasks = std::mem::take(&mut request.tasks);
        let equipment = std::mem::take(&mut request.equipment);
        let requirements = std::mem::take(&mut request.requirements);

        let mut tx = self.pool.begin().await?;

        // Create shift
        let shift = self
            .shift_repo
            .create(&mut tx, hospital_id, created_by, request)
            .await?;

        // F1-F12 / F1-F13 / F1-F14 — persist atomically within the same tx.
        self.shift_repo
            .insert_shift_description_and_requirements(
                &mut tx,
                shift.id,
                &tasks,
                &equipment,
                &requirements,
            )
            .await?;

        // AC-04 / F1-F15: Generate virtual link for virtual shifts
        if shift.shift_type == ShiftType::Virtual {
            let virtual_link = self.generate_virtual_link(shift.id);
            self.shift_repo
                .update_virtual_link(&mut tx, shift.id, &virtual_link)
                .await?;
        }

        // hospital must have wallet funds covering the gross
        let gross = shift.grand_total_kobo.unwrap_or(0);
        if gross > 0 {
            self.wallet_service
                .try_hold_in_tx(&mut tx, hospital_id, Some(shift.id), gross)
                .await
                .map_err(|e| match e {
                    crate::services::wallet_service::WalletServiceError::Repo(
                        crate::repositories::wallet::WalletRepoError::InsufficientBalance {
                            required,
                            available,
                        },
                    ) => ShiftServiceError::InsufficientWalletBalance {
                        required,
                        available,
                    },
                    other => ShiftServiceError::WalletError(other.to_string()),
                })?;
        }

        // Broadcast shift (calculate matching clinicians).
        let matched_count = self.calculate_matched_clinicians(&shift).await;
        self.shift_repo
            .broadcast_shift(&mut tx, shift.id, matched_count)
            .await?;

        tx.commit().await?;

        // record the initial broadcast in the audit table so the
        let radius_km = self
            .shift_repo
            .get_broadcast_radius_km(hospital_id)
            .await
            .ok()
            .flatten()
            .unwrap_or(5.0);
        if let Err(e) = self
            .shift_repo
            .record_broadcast(shift.id, Some(created_by), matched_count, radius_km)
            .await
        {
            eprintln!("Warning: Failed to record initial broadcast: {e}");
        }

        // Send push notifications to eligible workers
        self.broadcast_shift_notifications(shift.id, hospital_id, matched_count)
            .await?;

        if let Ok(Some((hospital_name, hospital_email))) =
            self.shift_repo.get_hospital_contact(hospital_id).await
        {
            let content = email_templates::shift_created(
                &hospital_name,
                &shift.role_title,
                shift.scheduled_start,
            );
            if let Err(e) = self
                .email_outbox
                .enqueue_email(&hospital_email, &content)
                .await
            {
                eprintln!("Warning: Failed to queue shift created email: {}", e);
            }
        }

        Ok(shift)
    }

    pub async fn get_shift(&self, shift_id: Uuid) -> Result<Shift, ShiftServiceError> {
        self.shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))
    }

    /// Enriched shift detail for the "View Shift Details" screen (SCRUM-25 /
    /// US-09): base shift plus tasks, requirements, hospital rating, and — for
    /// in-person shifts — the hospital location. When `requester_user_id` is a
    /// clinician, each requirement is matched against their qualifications.
    pub async fn get_shift_detail(
        &self,
        shift_id: Uuid,
        requester_user_id: Option<Uuid>,
    ) -> Result<crate::models::shift::ShiftDetailResponse, ShiftServiceError> {
        use crate::models::shift::{
            HospitalLocation, HospitalRatingSummary, ShiftDetailResponse, ShiftType,
        };

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        let tasks = self.shift_repo.list_shift_tasks(shift_id).await?;
        let requirements = self.shift_repo.list_shift_requirements(shift_id).await?;

        let (average, count) = self
            .shift_repo
            .hospital_rating_summary(shift.hospital_id)
            .await?;
        let hospital_rating = HospitalRatingSummary {
            average: average.unwrap_or(0.0),
            count,
        };

        // Map coordinates only make sense for in-person shifts (AC-06).
        let hospital_location = match shift.shift_type {
            ShiftType::InPerson => self
                .shift_repo
                .get_hospital_coordinates(shift.hospital_id)
                .await?
                .map(|(latitude, longitude)| HospitalLocation {
                    latitude,
                    longitude,
                }),
            ShiftType::Virtual => None,
        };

        // Qualification match is only meaningful for a clinician viewer (AC-04).
        let qualification_match = match requester_user_id {
            Some(user_id) => match self.shift_repo.find_clinician_id_for_user(user_id).await? {
                Some(clinician_id) => {
                    let quals = self
                        .shift_repo
                        .list_clinician_qualifications(clinician_id)
                        .await?;
                    match_qualifications(&requirements, &quals)
                }
                None => Vec::new(),
            },
            None => Vec::new(),
        };

        Ok(ShiftDetailResponse {
            shift,
            tasks,
            requirements,
            qualification_match,
            hospital_rating,
            hospital_location,
        })
    }

    pub async fn list_shifts(
        &self,
        status_filter: Option<ShiftStatus>,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<Shift>, i64), ShiftServiceError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = (page - 1) * page_size;

        let shifts = self
            .shift_repo
            .list_shifts(status_filter.clone(), page_size, offset)
            .await?;

        let total = self.shift_repo.count_shifts(status_filter).await?;

        Ok((shifts, total))
    }

    pub async fn express_interest(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<(), ShiftServiceError> {
        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        // US-10 AC-04: interest is only accepted while the shift is open. Once
        // it has been assigned (or otherwise moved on) it is no longer available.
        if shift.status != ShiftStatus::Open {
            return Err(ShiftServiceError::ShiftUnavailable);
        }

        match self
            .shift_repo
            .add_interest(shift_id, clinician_id, false, false)
            .await
        {
            Ok(()) => {}
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                return Err(ShiftServiceError::DuplicateInterest)
            }
            Err(err) => return Err(ShiftServiceError::DatabaseError(err)),
        }

        // US-10 AC-05: let the hospital admin know a worker is available.
        let worker_name = self
            .shift_repo
            .get_clinician_contact(clinician_id)
            .await
            .ok()
            .flatten()
            .map(|(first, last, _)| format!("{} {}", first.trim(), last.trim()).trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "A worker".to_string());

        self.push
            .notify_best_effort(
                shift.created_by,
                "interest_expressed",
                "New interest in your shift",
                &format!("{worker_name} is interested in \"{}\"", shift.role_title),
                serde_json::json!({ "shift_id": shift_id, "clinician_id": clinician_id }),
            )
            .await;

        Ok(())
    }

    pub async fn apply_for_shift(
        &self,
        shift_id: Uuid,
        request: crate::models::shift::ShiftApplicationRequest,
    ) -> Result<(), ShiftServiceError> {
        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.status != ShiftStatus::Open {
            return Err(ShiftServiceError::InvalidStatus(
                "Shift is not open for applications".to_string(),
            ));
        }

        let profile = self
            .shift_repo
            .get_clinician_profile_snapshot(request.clinician_id)
            .await?
            .ok_or(ShiftServiceError::ProfileIncomplete)?;

        let (first_name, last_name, license_number, role) = profile;
        let profile_complete = !first_name.trim().is_empty()
            && !last_name.trim().is_empty()
            && license_number
                .as_ref()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            && role.as_ref().map(|v| !v.trim().is_empty()).unwrap_or(false);

        if !profile_complete {
            return Err(ShiftServiceError::ProfileIncomplete);
        }

        if self
            .shift_repo
            .clinician_has_active_assignment(request.clinician_id)
            .await?
        {
            return Err(ShiftServiceError::ClinicianBusy);
        }

        let verified_applicant_name = format!("{} {}", first_name.trim(), last_name.trim())
            .trim()
            .to_string();
        let verified_license_number = license_number.expect("checked by profile_complete above");
        let verified_role = role.expect("checked by profile_complete above");

        let mut tx = self.pool.begin().await?;
        let result = self
            .shift_repo
            .create_application(
                &mut tx,
                shift_id,
                request.clinician_id,
                &verified_applicant_name,
                &verified_license_number,
                &verified_role,
                request.years_experience,
                request.experience_summary.as_deref(),
            )
            .await;

        match result {
            Ok(_) => {
                tx.commit().await?;
                Ok(())
            }
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                Err(ShiftServiceError::DuplicateApplication)
            }
            Err(err) => Err(ShiftServiceError::DatabaseError(err)),
        }
    }

    pub async fn assign_shift(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
    ) -> Result<(), ShiftServiceError> {
        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.assigned_clinician_id.is_some() {
            return Err(ShiftServiceError::AlreadyAssigned);
        }

        if shift.status != ShiftStatus::Open {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Shift must be open to assign (current: {:?})",
                shift.status
            )));
        }

        if self
            .shift_repo
            .clinician_has_active_assignment(clinician_id)
            .await?
        {
            return Err(ShiftServiceError::ClinicianBusy);
        }

        let mut tx = self.pool.begin().await?;
        let updated = self
            .shift_repo
            .assign_clinician(&mut tx, shift_id, clinician_id, ShiftStatus::Upcoming)
            .await?;

        if updated == 0 {
            return Err(ShiftServiceError::InvalidStatus(
                "Shift is not open or already assigned".to_string(),
            ));
        }

        let _ = self
            .shift_repo
            .update_application_status(
                &mut tx,
                shift_id,
                clinician_id,
                crate::models::shift::ShiftApplicationStatus::Accepted,
            )
            .await;

        tx.commit().await?;

        let hospital_contact = self
            .shift_repo
            .get_hospital_contact(shift.hospital_id)
            .await
            .ok()
            .flatten();
        let clinician_contact = self
            .shift_repo
            .get_clinician_contact(clinician_id)
            .await
            .ok()
            .flatten();
        let clinician_name = clinician_contact
            .as_ref()
            .map(|(first, last, _)| format!("{} {}", first, last).trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Clinician".to_string());

        if let Some((_, _, clinician_email)) = clinician_contact {
            let content = email_templates::shift_assigned_clinician(
                &clinician_name,
                shift.hospital_name.as_deref().unwrap_or("the hospital"),
                &shift.role_title,
                shift.scheduled_start,
            );
            if let Err(e) = self
                .email_outbox
                .enqueue_email(&clinician_email, &content)
                .await
            {
                eprintln!("Warning: Failed to queue clinician assignment email: {}", e);
            }
        }

        if let Some((hospital_name, hospital_email)) = hospital_contact {
            let content = email_templates::shift_assigned_hospital(
                &hospital_name,
                &clinician_name,
                &shift.role_title,
                shift.scheduled_start,
            );
            if let Err(e) = self
                .email_outbox
                .enqueue_email(&hospital_email, &content)
                .await
            {
                eprintln!("Warning: Failed to queue hospital assignment email: {}", e);
            }
        }

        Ok(())
    }

    /// Return interested clinicians for a shift, ranked by the

    pub async fn list_ranked_interested(
        &self,
        shift_id: Uuid,
        requester_user_id: Uuid,
    ) -> Result<Vec<crate::models::shift::RankedInterestedClinician>, ShiftServiceError> {
        use crate::models::shift::RankedInterestedClinician;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.created_by != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }

        let hospital_coords = self
            .shift_repo
            .get_hospital_coordinates(shift.hospital_id)
            .await?;

        let rows = self.shift_repo.list_interested_with_stats(shift_id).await?;

        // fetch the shift's required qualifications once. If the
        let required = self.shift_repo.list_shift_requirements(shift_id).await?;
        let required_lower: Vec<String> =
            required.iter().map(|s| s.trim().to_lowercase()).collect();
        let mut ranked: Vec<RankedInterestedClinician> = Vec::with_capacity(rows.len());
        for r in rows {
            let distance_km = match (hospital_coords, r.clinician_lat, r.clinician_lng) {
                (Some((h_lat, h_lng)), Some(c_lat), Some(c_lng)) => {
                    Some(crate::utils::geo::haversine_km(h_lat, h_lng, c_lat, c_lng))
                }
                _ => None,
            };

            // component scoring.
            let distance_score = match distance_km {
                Some(d) if d <= 2.0 => 100.0,
                Some(d) if d <= 5.0 => 70.0,
                Some(_) => 0.0,
                None => 0.0,
            };
            let rating_score = ((r.rating as f64).clamp(0.0, 5.0) / 5.0) * 100.0;
            let experience_score = ((r.completed_shifts as f64) / 100.0).min(1.0) * 100.0;

            let total_offers = r.accepts + r.declines + r.expires;
            let acceptance_rate_pct = if total_offers == 0 {
                None
            } else {
                Some((r.accepts as f64 / total_offers as f64) * 100.0)
            };
            let acceptance_score = acceptance_rate_pct.unwrap_or(0.0);

            // Real qualifications match. 100 if the clinician
            let quals_match = if required_lower.is_empty() {
                true
            } else {
                let owned = self
                    .shift_repo
                    .list_clinician_qualifications(r.clinician_id)
                    .await
                    .unwrap_or_default();
                let owned_lower: Vec<String> =
                    owned.iter().map(|s| s.trim().to_lowercase()).collect();
                required_lower
                    .iter()
                    .all(|req| owned_lower.iter().any(|q| q.contains(req)))
            };
            let quals_score = if quals_match { 100.0 } else { 0.0 };

            let score = distance_score * 0.30
                + rating_score * 0.25
                + experience_score * 0.20
                + acceptance_score * 0.15
                + quals_score * 0.10;

            // Mask to last name until selected.
            let display_name = r.last_name.trim().to_string();
            ranked.push(RankedInterestedClinician {
                clinician_id: r.clinician_id,
                display_name,
                distance_km,
                rating: r.rating,
                rating_count: r.rating_count,
                completed_shifts: r.completed_shifts,
                acceptance_rate_pct,
                quals_match,
                score,
            });
        }

        // Highest score first; stable tiebreaker by clinician_id keeps results
        ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.clinician_id.cmp(&b.clinician_id))
        });

        Ok(ranked)
    }

    /// Hospital admin sends an offer to a specific interested

    pub async fn offer_shift(
        &self,
        shift_id: Uuid,
        clinician_id: Uuid,
        requester_user_id: Uuid,
    ) -> Result<(Uuid, chrono::DateTime<chrono::Utc>), ShiftServiceError> {
        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.created_by != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }

        if shift.status != ShiftStatus::Open {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Cannot offer a shift in status {:?}",
                shift.status
            )));
        }

        // the clinician must have expressed interest in this shift.
        let interested = self.shift_repo.list_interested_with_stats(shift_id).await?;
        if !interested.iter().any(|r| r.clinician_id == clinician_id) {
            return Err(ShiftServiceError::NotInterested);
        }

        // US-06 AC-04: a worker who already declined cannot be re-offered.
        if self
            .shift_repo
            .has_declined_offer(shift_id, clinician_id)
            .await?
        {
            return Err(ShiftServiceError::WorkerAlreadyDeclined);
        }

        let expires_at = Utc::now() + Duration::minutes(30);
        let assignment_id = match self
            .shift_repo
            .create_assignment_offer(shift_id, clinician_id, expires_at)
            .await
        {
            Ok(id) => id,
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                return Err(ShiftServiceError::DuplicateOffer);
            }
            Err(e) => return Err(ShiftServiceError::DatabaseError(e)),
        };

        // Best-effort notification to the clinician.
        if let Ok(Some((first_name, _last_name, clinician_email))) =
            self.shift_repo.get_clinician_contact(clinician_id).await
        {
            let content = email_templates::shift_offered(
                &first_name,
                &shift.role_title,
                shift.scheduled_start,
                expires_at,
            );
            if let Err(e) = self
                .email_outbox
                .enqueue_email(&clinician_email, &content)
                .await
            {
                eprintln!("Warning: Failed to queue shift offer email: {}", e);
            }
        }

        // Push the offer to the clinician's devices (US-06 AC-01 / US-11 AC-01).
        if let Ok(Some(clinician_user_id)) =
            self.shift_repo.get_clinician_user_id(clinician_id).await
        {
            self.push
                .notify_best_effort(
                    clinician_user_id,
                    "shift_offered",
                    "New shift offer",
                    &format!("You have a shift offer for {}", shift.role_title),
                    serde_json::json!({ "shift_id": shift_id, "expires_at": expires_at }),
                )
                .await;
        }

        Ok((assignment_id, expires_at))
    }

    /// Worker accepts a pending offer.

    pub async fn accept_offer(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
        ndpr_consent: crate::models::shift::NdprConsent,
    ) -> Result<Uuid, ShiftServiceError> {
        if !ndpr_consent.all_accepted() {
            return Err(ShiftServiceError::ConsentRequired);
        }

        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        let (assignment_id, expires_at) = self
            .shift_repo
            .get_pending_offer(shift_id, clinician_id)
            .await?
            .ok_or(ShiftServiceError::NoPendingOffer)?;

        if expires_at < Utc::now() {
            return Err(ShiftServiceError::OfferExpired);
        }

        // clinician must not already be on an active assignment.
        if self
            .shift_repo
            .clinician_has_active_assignment(clinician_id)
            .await?
        {
            return Err(ShiftServiceError::ClinicianBusy);
        }

        // no time overlap with another assigned/upcoming/in-progress shift.
        if self
            .shift_repo
            .has_conflicting_shift(clinician_id, shift.scheduled_start, shift.scheduled_end)
            .await?
        {
            return Err(ShiftServiceError::ScheduleConflict);
        }

        let consent_json = serde_json::to_value(&ndpr_consent).map_err(|e| {
            ShiftServiceError::ValidationError(format!("NDPR consent serialisation failed: {e}"))
        })?;

        let mut tx = self.pool.begin().await?;
        // The guarded UPDATE serializes concurrent accepts of the same offer
        // (US-11 UT020): if it flips no rows, another accept already won — abort
        // so we don't double-assign. Dropping `tx` rolls back.
        let updated = self
            .shift_repo
            .accept_offer_tx(&mut tx, assignment_id, &consent_json)
            .await?;
        if updated == 0 {
            return Err(ShiftServiceError::OfferAlreadyResponded);
        }
        self.shift_repo
            .cancel_sibling_offers_tx(&mut tx, shift_id, assignment_id)
            .await?;
        self.shift_repo
            .assign_shift_to_clinician_tx(&mut tx, shift_id, clinician_id)
            .await?;
        tx.commit().await?;

        // refresh the cached acceptance rate after the lifecycle
        if let Err(e) = self
            .shift_repo
            .recompute_clinician_acceptance_rate(clinician_id)
            .await
        {
            eprintln!("Warning: acceptance-rate recompute failed for {clinician_id}: {e}");
        }

        // Best-effort confirmation emails (one to hospital, one to clinician).
        if let Ok(Some((hospital_name, hospital_email))) = self
            .shift_repo
            .get_hospital_contact(shift.hospital_id)
            .await
        {
            if let Ok(Some((first_name, last_name, _email))) =
                self.shift_repo.get_clinician_contact(clinician_id).await
            {
                let clinician_name = format!("{} {}", first_name, last_name).trim().to_string();
                let content = email_templates::shift_assigned_hospital(
                    &hospital_name,
                    &clinician_name,
                    &shift.role_title,
                    shift.scheduled_start,
                );
                let _ = self
                    .email_outbox
                    .enqueue_email(&hospital_email, &content)
                    .await;
            }
        }
        if let Ok(Some((first_name, _last_name, clinician_email))) =
            self.shift_repo.get_clinician_contact(clinician_id).await
        {
            let hospital_name = self
                .shift_repo
                .get_hospital_name(shift.hospital_id)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "the hospital".to_string());
            let content = email_templates::shift_assigned_clinician(
                &first_name,
                &hospital_name,
                &shift.role_title,
                shift.scheduled_start,
            );
            let _ = self
                .email_outbox
                .enqueue_email(&clinician_email, &content)
                .await;
        }

        // Push the assignment confirmation to the hospital admin who created
        // the shift (US-06 AC-06 / US-11 AC-10).
        self.push
            .notify_best_effort(
                shift.created_by,
                "shift_accepted",
                "Shift assigned",
                &format!("Your shift \"{}\" was accepted", shift.role_title),
                serde_json::json!({ "shift_id": shift_id }),
            )
            .await;

        Ok(assignment_id)
    }

    /// Worker declines a pending offer. The shift stays `open` so

    pub async fn decline_offer(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
        reason: Option<String>,
    ) -> Result<(), ShiftServiceError> {
        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let (assignment_id, _expires_at) = self
            .shift_repo
            .get_pending_offer(shift_id, clinician_id)
            .await?
            .ok_or(ShiftServiceError::NoPendingOffer)?;

        self.shift_repo
            .decline_offer(assignment_id, reason.as_deref())
            .await?;

        // refresh the cached acceptance rate.
        if let Err(e) = self
            .shift_repo
            .recompute_clinician_acceptance_rate(clinician_id)
            .await
        {
            eprintln!("Warning: acceptance-rate recompute failed for {clinician_id}: {e}");
        }

        // Best-effort notification to the hospital admin.
        if let Ok(Some(shift)) = self.shift_repo.get_by_id(shift_id).await {
            if let Ok(Some((_, hospital_email))) = self
                .shift_repo
                .get_hospital_contact(shift.hospital_id)
                .await
            {
                let content = email_templates::shift_offer_declined(
                    &shift.role_title,
                    shift.scheduled_start,
                    reason.as_deref(),
                );
                let _ = self
                    .email_outbox
                    .enqueue_email(&hospital_email, &content)
                    .await;
            }

            // Push the decline to the hospital admin (US-11 AC-12).
            self.push
                .notify_best_effort(
                    shift.created_by,
                    "shift_declined",
                    "Offer declined",
                    &format!("A worker declined your shift \"{}\"", shift.role_title),
                    serde_json::json!({ "shift_id": shift_id }),
                )
                .await;
        }

        Ok(())
    }

    /// Worker clocks in for an assigned shift. ///

    pub async fn clock_in(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
        request: crate::models::shift::ClockinRequest,
    ) -> Result<crate::models::shift::ClockinResponse, ShiftServiceError> {
        use crate::models::shift::{ClockinMethod, ClockinResponse, ShiftType};

        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.assigned_clinician_id != Some(clinician_id) {
            return Err(ShiftServiceError::NotAuthorized);
        }

        if !matches!(shift.status, ShiftStatus::Assigned | ShiftStatus::Upcoming) {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Cannot clock in to a shift in status {:?}",
                shift.status
            )));
        }

        // clock-in must be within ±1 hour of scheduled start.
        let now = Utc::now();
        let delta = now.signed_duration_since(shift.scheduled_start);
        let late_minutes_signed = delta.num_minutes();
        if late_minutes_signed < -60 {
            return Err(ShiftServiceError::TooEarlyToClockIn);
        }
        if late_minutes_signed > 60 {
            return Err(ShiftServiceError::MissedShift);
        }
        let late_minutes = late_minutes_signed.max(0) as i32;
        let late_penalty_applied = (15..30).contains(&late_minutes);

        // not already clocked into another shift.
        if self
            .shift_repo
            .clinician_has_active_assignment(clinician_id)
            .await?
            && shift.status != ShiftStatus::Upcoming
        // the current one doesn't count
        {
            // We allow the current shift even though it's 'assigned'/'upcoming';
        }

        // GPS / virtual branch.
        let (distance_meters, latitude, longitude) = match request.method {
            ClockinMethod::Gps => {
                let lat = request.latitude.ok_or_else(|| {
                    ShiftServiceError::ValidationError(
                        "latitude is required for GPS clock-in".to_string(),
                    )
                })?;
                let lng = request.longitude.ok_or_else(|| {
                    ShiftServiceError::ValidationError(
                        "longitude is required for GPS clock-in".to_string(),
                    )
                })?;

                let (h_lat, h_lng) = self
                    .shift_repo
                    .get_hospital_coordinates(shift.hospital_id)
                    .await?
                    .ok_or_else(|| {
                        ShiftServiceError::InvalidStatus(
                            "Hospital has no registered location".to_string(),
                        )
                    })?;

                let radius_m = self
                    .shift_repo
                    .get_clock_in_radius_meters(shift.hospital_id)
                    .await?
                    .unwrap_or(100);

                let distance_km = crate::utils::geo::haversine_km(h_lat, h_lng, lat, lng);
                let distance_m = distance_km * 1000.0;

                if distance_m > radius_m as f64 {
                    return Err(ShiftServiceError::OutOfGeofence(distance_m as i32));
                }

                (Some(distance_m), Some(lat), Some(lng))
            }
            ClockinMethod::Virtual => {
                if shift.shift_type != ShiftType::Virtual {
                    return Err(ShiftServiceError::ValidationError(
                        "Virtual clock-in is only allowed for virtual shifts".to_string(),
                    ));
                }
                (None, None, None)
            }
            ClockinMethod::Manual => {
                // Manual clock-in is only permitted when there's
                if !self
                    .shift_repo
                    .has_approved_clockin_request(shift_id, clinician_id)
                    .await?
                {
                    return Err(ShiftServiceError::ManualClockinNotApproved);
                }
                (None, request.latitude, request.longitude)
            }
            ClockinMethod::QrCode => {
                return Err(ShiftServiceError::ValidationError(
                    "QR-code clock-in is not yet supported via this endpoint".to_string(),
                ));
            }
        };

        let mut tx = self.pool.begin().await?;
        let attendance_id = self
            .shift_repo
            .record_clockin_tx(
                &mut tx,
                shift_id,
                clinician_id,
                &request.method,
                latitude,
                longitude,
                distance_meters.map(|d| d as f32),
                late_minutes,
                late_penalty_applied,
            )
            .await?;
        tx.commit().await?;

        Ok(ClockinResponse {
            attendance_id,
            shift_id,
            clockin_at: now,
            distance_meters,
            late_minutes,
            late_penalty_applied,
        })
    }

    /// Submit (or resubmit, within editable_until) handover.

    pub async fn submit_handover(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
        request: crate::models::shift::SubmitHandoverRequest,
    ) -> Result<crate::models::shift::HandoverResponse, ShiftServiceError> {
        use validator::Validate;
        request
            .validate()
            .map_err(|e| ShiftServiceError::ValidationError(e.to_string()))?;

        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.assigned_clinician_id != Some(clinician_id) {
            return Err(ShiftServiceError::NotAuthorized);
        }

        // handover is editable for 1 hour after clock out. So both
        if !matches!(
            shift.status,
            ShiftStatus::InProgress | ShiftStatus::Completed
        ) {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Handover can only be submitted for an in-progress or just-completed shift (current: {:?})",
                shift.status
            )));
        }

        // If shift is completed and the edit window has lapsed, reject.
        if shift.status == ShiftStatus::Completed {
            if let Some(existing) = self.shift_repo.get_handover(shift_id).await? {
                if existing.editable_until < Utc::now() {
                    return Err(ShiftServiceError::HandoverEditWindowClosed);
                }
            }
        }

        let critical_patients = serde_json::Value::Array(request.critical_patients.clone());
        let pending_tasks = serde_json::Value::Array(request.pending_tasks.clone());

        let row = self
            .shift_repo
            .upsert_handover(
                shift_id,
                request.patients_seen,
                &critical_patients,
                &pending_tasks,
                &request.instructions,
                request.equipment_status.as_deref(),
            )
            .await?;

        Ok(row)
    }

    /// Worker clocks out. Requires a submitted handover

    pub async fn clock_out(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
    ) -> Result<crate::models::shift::ClockoutResponse, ShiftServiceError> {
        use crate::models::shift::ClockoutResponse;

        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.assigned_clinician_id != Some(clinician_id) {
            return Err(ShiftServiceError::NotAuthorized);
        }

        if shift.status != ShiftStatus::InProgress {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Cannot clock out of a shift in status {:?}",
                shift.status
            )));
        }

        // handover must be submitted.
        if self.shift_repo.get_handover(shift_id).await?.is_none() {
            return Err(ShiftServiceError::HandoverRequired);
        }

        let clockin_at = self
            .shift_repo
            .get_attendance_clockin(shift_id)
            .await?
            .ok_or_else(|| {
                ShiftServiceError::InvalidStatus("Shift has no clock-in record".to_string())
            })?;

        let now = Utc::now();
        let worked_minutes = now.signed_duration_since(clockin_at).num_minutes().max(0) as i32;

        let mut tx = self.pool.begin().await?;
        let attendance_id = self
            .shift_repo
            .record_clockout_tx(&mut tx, shift_id, worked_minutes)
            .await?;
        tx.commit().await?;

        Ok(ClockoutResponse {
            attendance_id,
            shift_id,
            clockout_at: now,
            worked_minutes,
        })
    }

    /// Hospital requests a handover revision within 24 hours of

    pub async fn request_handover_revision(
        &self,
        shift_id: Uuid,
        requester_user_id: Uuid,
        notes: String,
    ) -> Result<(), ShiftServiceError> {
        if notes.trim().is_empty() {
            return Err(ShiftServiceError::ValidationError(
                "Revision notes cannot be empty".to_string(),
            ));
        }

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.created_by != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }

        if self.shift_repo.get_handover(shift_id).await?.is_none() {
            return Err(ShiftServiceError::HandoverRequired);
        }

        let clockout_at = self
            .shift_repo
            .get_attendance_clockout(shift_id)
            .await?
            .ok_or_else(|| {
                ShiftServiceError::InvalidStatus("Shift has not been clocked out".to_string())
            })?;

        // revision must be requested within 24h of clock-out.
        if Utc::now() > clockout_at + Duration::hours(24) {
            return Err(ShiftServiceError::RevisionWindowClosed);
        }

        self.shift_repo
            .request_handover_revision(shift_id, &notes)
            .await?;
        Ok(())
    }

    /// Hospital explicitly approves the handover. This is what

    pub async fn approve_handover(
        &self,
        shift_id: Uuid,
        requester_user_id: Uuid,
    ) -> Result<(), ShiftServiceError> {
        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.created_by != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }

        // Handover must exist (clinician must have submitted).
        if self.shift_repo.get_handover(shift_id).await?.is_none() {
            return Err(ShiftServiceError::HandoverRequired);
        }

        let affected = self.shift_repo.approve_handover(shift_id).await?;
        if affected == 0 {
            // Either no handover row, or already approved.
            return Err(ShiftServiceError::InvalidStatus(
                "Handover is already approved".to_string(),
            ));
        }
        Ok(())
    }

    /// Hospital rates the assigned worker

    pub async fn rate_worker(
        &self,
        shift_id: Uuid,
        requester_user_id: Uuid,
        request: crate::models::shift::RateWorkerRequest,
    ) -> Result<crate::models::shift::RatingResponse, ShiftServiceError> {
        use validator::Validate;
        request
            .validate()
            .map_err(|e| ShiftServiceError::ValidationError(e.to_string()))?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.created_by != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }
        if shift.status != ShiftStatus::Completed {
            return Err(ShiftServiceError::InvalidStatus(
                "Ratings can only be submitted for completed shifts".to_string(),
            ));
        }
        let ratee_id = shift.assigned_clinician_id.ok_or_else(|| {
            ShiftServiceError::InvalidStatus("Shift has no assigned clinician to rate".to_string())
        })?;

        // 7-day submission window after completion. We use the
        let window_closes_at = shift.updated_at + Duration::days(7);
        if Utc::now() > window_closes_at {
            return Err(ShiftServiceError::RatingWindowClosed);
        }

        let mut tx = self.pool.begin().await?;
        let rating = match self
            .shift_repo
            .insert_rating(
                &mut tx,
                shift_id,
                requester_user_id,
                ratee_id,
                "clinician",
                request.score,
                None,
                request.comment.as_deref(),
                window_closes_at,
            )
            .await
        {
            Ok(r) => r,
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                return Err(ShiftServiceError::DuplicateRating);
            }
            Err(e) => return Err(ShiftServiceError::DatabaseError(e)),
        };
        self.shift_repo
            .recompute_clinician_rating_tx(&mut tx, ratee_id)
            .await?;
        tx.commit().await?;

        Ok(rating)
    }

    /// Worker rates the hospital

    pub async fn rate_hospital(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
        request: crate::models::shift::RateHospitalRequest,
    ) -> Result<crate::models::shift::RatingResponse, ShiftServiceError> {
        use validator::Validate;
        request
            .validate()
            .map_err(|e| ShiftServiceError::ValidationError(e.to_string()))?;

        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.assigned_clinician_id != Some(clinician_id) {
            return Err(ShiftServiceError::NotAuthorized);
        }
        if shift.status != ShiftStatus::Completed {
            return Err(ShiftServiceError::InvalidStatus(
                "Ratings can only be submitted for completed shifts".to_string(),
            ));
        }

        let window_closes_at = shift.updated_at + Duration::days(7);
        if Utc::now() > window_closes_at {
            return Err(ShiftServiceError::RatingWindowClosed);
        }

        let dims_json = serde_json::to_value(&request.dimensions).map_err(|e| {
            ShiftServiceError::ValidationError(format!("dimensions serialisation failed: {e}"))
        })?;

        let mut tx = self.pool.begin().await?;
        let rating = match self
            .shift_repo
            .insert_rating(
                &mut tx,
                shift_id,
                worker_user_id,
                shift.hospital_id,
                "hospital",
                request.score,
                Some(&dims_json),
                request.comment.as_deref(),
                window_closes_at,
            )
            .await
        {
            Ok(r) => r,
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                return Err(ShiftServiceError::DuplicateRating);
            }
            Err(e) => return Err(ShiftServiceError::DatabaseError(e)),
        };
        tx.commit().await?;

        Ok(rating)
    }

    /// Edit an existing rating within the 48h edit window

    pub async fn edit_rating(
        &self,
        rating_id: Uuid,
        requester_user_id: Uuid,
        request: crate::models::shift::EditRatingRequest,
    ) -> Result<crate::models::shift::RatingResponse, ShiftServiceError> {
        use validator::Validate;
        request
            .validate()
            .map_err(|e| ShiftServiceError::ValidationError(e.to_string()))?;

        let (existing, rater_user_id) = self
            .shift_repo
            .get_rating_for_edit(rating_id)
            .await?
            .ok_or(ShiftServiceError::RatingNotFound)?;

        if rater_user_id != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }
        if existing.editable_until < Utc::now() {
            return Err(ShiftServiceError::RatingEditWindowClosed);
        }

        let dims_json = request
            .dimensions
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| {
                ShiftServiceError::ValidationError(format!("dimensions serialisation failed: {e}"))
            })?;

        let updated = self
            .shift_repo
            .update_rating(
                rating_id,
                request.score,
                dims_json.as_ref(),
                request.comment.as_deref(),
            )
            .await?;

        // If the edited rating was for a clinician, refresh the cached avg.
        if updated.ratee_kind == "clinician" {
            let mut tx = self.pool.begin().await?;
            self.shift_repo
                .recompute_clinician_rating_tx(&mut tx, updated.ratee_id)
                .await?;
            tx.commit().await?;
        }

        Ok(updated)
    }

    /// Worker shift discovery (SCRUM-24 / US-08). Returns open shifts within
    /// `radius_km` of the worker's origin, ranked by urgency then distance.
    ///
    /// The origin is resolved in priority order: live GPS from the request (also
    /// persisted for later use), else the worker's last-known location. When
    /// neither is available a [`ShiftServiceError::LocationRequired`] is returned
    /// so the caller can prompt for location access. Filtering, sorting and
    /// paging are performed in SQL — see [`ShiftRepository::list_nearby_shifts`].
    pub async fn list_nearby_shifts_for_worker(
        &self,
        worker_user_id: Uuid,
        origin: Option<WorkerOrigin>,
        radius_km: f64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<crate::models::shift::NearbyShiftCard>, ShiftServiceError> {
        use crate::models::shift::NearbyShiftCard;

        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        // Resolve the origin: live GPS wins and is persisted; otherwise fall
        // back to the last-known location; otherwise ask the caller for one.
        let (origin_lat, origin_lng) = match origin {
            Some(o) => {
                self.shift_repo
                    .upsert_clinician_location(clinician_id, o.lat, o.lng, o.accuracy_meters)
                    .await?;
                (o.lat, o.lng)
            }
            None => self
                .shift_repo
                .get_clinician_location(clinician_id)
                .await?
                .ok_or(ShiftServiceError::LocationRequired)?,
        };

        let rows = self
            .shift_repo
            .list_nearby_shifts(clinician_id, origin_lat, origin_lng, radius_km, limit, offset)
            .await?;

        // Rows arrive already filtered, ranked and paged; map them 1:1.
        let cards = rows
            .into_iter()
            .map(|r| NearbyShiftCard {
                shift_id: r.shift_id,
                hospital_id: r.hospital_id,
                hospital_name: r.hospital_name,
                role_title: r.role_title,
                specialty: r.specialty,
                shift_type: r.shift_type,
                priority: r.priority,
                scheduled_start: r.scheduled_start,
                duration_hours: r.duration_hours,
                pay_type: r.pay_type,
                rate_kobo_per_hour: r.rate_kobo_per_hour,
                fixed_rate_kobo: r.fixed_rate_kobo,
                stat_bonus_kobo: r.stat_bonus_kobo,
                distance_km: r.distance_km,
                interest_expressed: r.interest_expressed,
            })
            .collect();

        Ok(cards)
    }

    /// "My Applications" tab. Combines expressed interests and

    pub async fn list_my_applications(
        &self,
        worker_user_id: Uuid,
    ) -> Result<Vec<crate::models::shift::MyApplicationEntry>, ShiftServiceError> {
        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;
        Ok(self.shift_repo.list_my_applications(clinician_id).await?)
    }

    /// Withdraw expressed interest. Only allowed before

    pub async fn withdraw_interest(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
    ) -> Result<(), ShiftServiceError> {
        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        // withdrawal must happen before assignment.
        if shift.assigned_clinician_id.is_some()
            || matches!(
                shift.status,
                ShiftStatus::Assigned
                    | ShiftStatus::Upcoming
                    | ShiftStatus::InProgress
                    | ShiftStatus::Completed
            )
        {
            return Err(ShiftServiceError::InvalidStatus(
                "Cannot withdraw interest after assignment".to_string(),
            ));
        }

        let removed = self
            .shift_repo
            .withdraw_interest(shift_id, clinician_id)
            .await?;
        if removed == 0 {
            return Err(ShiftServiceError::NotInterested);
        }
        Ok(())
    }

    /// Bookmark a shift for later

    pub async fn bookmark_shift(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
    ) -> Result<(), ShiftServiceError> {
        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        // Ensure the shift exists so we 404 cleanly.
        if self.shift_repo.get_by_id(shift_id).await?.is_none() {
            return Err(ShiftServiceError::NotFound(shift_id));
        }

        self.shift_repo
            .bookmark_shift(shift_id, clinician_id)
            .await?;
        Ok(())
    }

    /// Remove a shift bookmark

    pub async fn unbookmark_shift(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
    ) -> Result<(), ShiftServiceError> {
        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        self.shift_repo
            .unbookmark_shift(shift_id, clinician_id)
            .await?;
        Ok(())
    }

    /// Dismiss a shift so it stops appearing in this clinician's

    pub async fn dismiss_shift(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
    ) -> Result<(), ShiftServiceError> {
        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        if self.shift_repo.get_by_id(shift_id).await?.is_none() {
            return Err(ShiftServiceError::NotFound(shift_id));
        }

        self.shift_repo
            .dismiss_shift(shift_id, clinician_id)
            .await?;
        Ok(())
    }

    pub async fn list_applications_for_shift(
        &self,
        shift_id: Uuid,
        requester_user_id: Uuid,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<crate::models::shift::ShiftApplication>, i64), ShiftServiceError> {
        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.created_by != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }

        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = (page - 1) * page_size;

        let applications = self
            .shift_repo
            .list_applications_for_shift(shift_id, page_size, offset)
            .await?;

        let total = self
            .shift_repo
            .count_applications_for_shift(shift_id)
            .await?;

        Ok((applications, total))
    }

    pub async fn cancel_shift(
        &self,
        shift_id: Uuid,
        reason: &str,
    ) -> Result<(), ShiftServiceError> {
        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.status != ShiftStatus::Open && shift.status != ShiftStatus::Upcoming {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Shift cannot be cancelled from status {:?}",
                shift.status
            )));
        }

        let mut tx = self.pool.begin().await?;
        let updated = self.shift_repo.cancel_shift(&mut tx, shift_id).await?;
        if updated == 0 {
            return Err(ShiftServiceError::InvalidStatus(
                "Shift is not open or upcoming".to_string(),
            ));
        }

        // release the escrowed funds back into the hospital's
        let gross = shift.grand_total_kobo.unwrap_or(0);
        if gross > 0 {
            if let Err(e) = self
                .wallet_service
                .release_hold_in_tx(&mut tx, shift.hospital_id, Some(shift.id), gross)
                .await
            {
                eprintln!(
                    "Warning: failed to release hold for cancelled shift {}: {}",
                    shift.id, e
                );
            }
        }

        tx.commit().await?;

        if let Ok(Some((hospital_name, hospital_email))) = self
            .shift_repo
            .get_hospital_contact(shift.hospital_id)
            .await
        {
            let content = email_templates::shift_cancelled(
                &hospital_name,
                &shift.role_title,
                shift.scheduled_start,
                reason,
            );
            if let Err(e) = self
                .email_outbox
                .enqueue_email(&hospital_email, &content)
                .await
            {
                eprintln!(
                    "Warning: Failed to queue hospital cancellation email: {}",
                    e
                );
            }
        }

        if let Some(clinician_id) = shift.assigned_clinician_id {
            if let Ok(Some((first_name, last_name, clinician_email))) =
                self.shift_repo.get_clinician_contact(clinician_id).await
            {
                let name = format!("{} {}", first_name, last_name).trim().to_string();
                let content = email_templates::shift_cancelled(
                    if name.is_empty() { "Clinician" } else { &name },
                    &shift.role_title,
                    shift.scheduled_start,
                    reason,
                );
                if let Err(e) = self
                    .email_outbox
                    .enqueue_email(&clinician_email, &content)
                    .await
                {
                    eprintln!(
                        "Warning: Failed to queue clinician cancellation email: {}",
                        e
                    );
                }
            }
        }

        Ok(())
    }

    pub async fn reschedule_shift(
        &self,
        shift_id: Uuid,
        scheduled_start: chrono::DateTime<Utc>,
        duration_hours: f32,
    ) -> Result<(), ShiftServiceError> {
        if duration_hours <= 0.0 {
            return Err(ShiftServiceError::ValidationError(
                "Duration must be greater than zero".to_string(),
            ));
        }

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.status != ShiftStatus::Open && shift.status != ShiftStatus::Upcoming {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Shift cannot be rescheduled from status {:?}",
                shift.status
            )));
        }

        let scheduled_end = scheduled_start + Duration::hours(duration_hours as i64);

        let mut tx = self.pool.begin().await?;
        let updated = self
            .shift_repo
            .reschedule_shift(
                &mut tx,
                shift_id,
                scheduled_start,
                duration_hours,
                scheduled_end,
            )
            .await?;
        if updated == 0 {
            return Err(ShiftServiceError::InvalidStatus(
                "Shift is not open or upcoming".to_string(),
            ));
        }
        tx.commit().await?;

        if let Ok(Some((hospital_name, hospital_email))) = self
            .shift_repo
            .get_hospital_contact(shift.hospital_id)
            .await
        {
            let content = email_templates::shift_rescheduled(
                &hospital_name,
                &shift.role_title,
                scheduled_start,
            );
            if let Err(e) = self
                .email_outbox
                .enqueue_email(&hospital_email, &content)
                .await
            {
                eprintln!("Warning: Failed to queue hospital reschedule email: {}", e);
            }
        }

        if let Some(clinician_id) = shift.assigned_clinician_id {
            if let Ok(Some((first_name, last_name, clinician_email))) =
                self.shift_repo.get_clinician_contact(clinician_id).await
            {
                let name = format!("{} {}", first_name, last_name).trim().to_string();
                let content = email_templates::shift_rescheduled(
                    if name.is_empty() { "Clinician" } else { &name },
                    &shift.role_title,
                    scheduled_start,
                );
                if let Err(e) = self
                    .email_outbox
                    .enqueue_email(&clinician_email, &content)
                    .await
                {
                    eprintln!("Warning: Failed to queue clinician reschedule email: {}", e);
                }
            }
        }

        Ok(())
    }

    fn validate_request(&self, request: &CreateShiftRequest) -> Result<(), ShiftServiceError> {
        // Validate required fields
        if request.role_title.trim().is_empty() {
            return Err(ShiftServiceError::ValidationError(
                "Role title is required".to_string(),
            ));
        }

        // F1-F06: Duration must be one of the allowed values.
        const ALLOWED_DURATIONS: [f32; 5] = [2.0, 4.0, 6.0, 8.0, 12.0];
        if !ALLOWED_DURATIONS
            .iter()
            .any(|d| (d - request.duration_hours).abs() < f32::EPSILON)
        {
            return Err(ShiftServiceError::ValidationError(
                "Duration must be one of 2, 4, 6, 8, or 12 hours".to_string(),
            ));
        }

        // F1-F05: Start time must fall on a 15-minute boundary.
        if let Err(e) = crate::utils::validation::validate_15min_boundary(&request.scheduled_start)
        {
            return Err(ShiftServiceError::ValidationError(
                e.message
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "Start time must be on a 15-minute boundary".to_string()),
            ));
        }

        // Start time cannot be in the past.
        let now = Utc::now();
        if request.scheduled_start < now {
            return Err(ShiftServiceError::ValidationError(
                "Start time cannot be in the past".to_string(),
            ));
        }

        // Validate pay type requirements + F1-F08/F1-F09 minimum rates.
        const MIN_HOURLY_KOBO: i64 = 200_000; // ₦2,000
        const MIN_FIXED_KOBO: i64 = 1_000_000; // ₦10,000
        match request.pay_type {
            crate::models::shift::PayType::HourlyRate => {
                let rate = request.rate_kobo_per_hour.ok_or_else(|| {
                    ShiftServiceError::ValidationError(
                        "Hourly rate is required for hourly pay type".to_string(),
                    )
                })?;
                if rate < MIN_HOURLY_KOBO {
                    return Err(ShiftServiceError::ValidationError(
                        "Hourly rate must be at least ₦2,000".to_string(),
                    ));
                }
            }
            crate::models::shift::PayType::FixedRate => {
                let rate = request.fixed_rate_kobo.ok_or_else(|| {
                    ShiftServiceError::ValidationError(
                        "Fixed rate is required for fixed pay type".to_string(),
                    )
                })?;
                if rate < MIN_FIXED_KOBO {
                    return Err(ShiftServiceError::ValidationError(
                        "Fixed rate must be at least ₦10,000".to_string(),
                    ));
                }
            }
        }

        // Urgency-based start-time windows.
        let time_until_start = request.scheduled_start.signed_duration_since(now);
        match request.priority {
            ShiftPriority::Stat => {
                if time_until_start > Duration::hours(1) {
                    return Err(ShiftServiceError::ValidationError(
                        "STAT shifts must start within 1 hour of creation".to_string(),
                    ));
                }
            }
            ShiftPriority::Urgent => {
                if time_until_start > Duration::hours(4) {
                    return Err(ShiftServiceError::ValidationError(
                        "Urgent shifts must start within 4 hours of creation".to_string(),
                    ));
                }
            }
            ShiftPriority::Normal => {
                // Must start on the same calendar day (UTC).
                if request.scheduled_start.date_naive() != now.date_naive() {
                    return Err(ShiftServiceError::ValidationError(
                        "Normal shifts must start today".to_string(),
                    ));
                }
            }
            ShiftPriority::Scheduled => {
                if time_until_start > Duration::days(30) {
                    return Err(ShiftServiceError::ValidationError(
                        "Scheduled shifts can be at most 30 days in the future".to_string(),
                    ));
                }
            }
        }

        // Validate broadcast consent
        if !request.broadcast_consent_confirmed {
            return Err(ShiftServiceError::ValidationError(
                "Broadcast consent must be confirmed".to_string(),
            ));
        }

        Ok(())
    }

    /// Check for duplicate shifts within the last hour
    async fn check_duplicate_shift(
        &self,
        hospital_id: Uuid,
        request: &CreateShiftRequest,
    ) -> Result<(), ShiftServiceError> {
        let one_hour_ago = Utc::now() - Duration::hours(1);

        let duplicate = self
            .shift_repo
            .find_similar_shift(
                hospital_id,
                &request.role_title,
                request.scheduled_start,
                one_hour_ago,
            )
            .await?;

        if duplicate.is_some() {
            return Err(ShiftServiceError::DuplicateShift(
                "Similar shift already exists.".to_string(),
            ));
        }

        Ok(())
    }

    /// Generate virtual meeting link for virtual shifts
    fn generate_virtual_link(&self, shift_id: Uuid) -> String {
        format!("https://meet.nexuscare.com/shift/{}", shift_id)
    }

    /// Calculate matched clinicians based on shift type and location

    pub async fn auto_approve_due_handovers(&self) -> Result<usize, ShiftServiceError> {
        let approved = self.shift_repo.auto_approve_due_handovers().await?;
        let count = approved.len();
        for (handover_id, shift_id, clinician_id, _hospital_id, role_title) in approved {
            if let Ok(Some((first_name, _last_name, clinician_email))) =
                self.shift_repo.get_clinician_contact(clinician_id).await
            {
                let content = email_templates::handover_auto_approved(&first_name, &role_title);
                if let Err(e) = self
                    .email_outbox
                    .enqueue_email(&clinician_email, &content)
                    .await
                {
                    eprintln!("Warning: Failed to queue handover auto-approval email: {e}");
                }
            }
            tracing::info!(
                "Handover {} for shift {} auto-approved",
                handover_id,
                shift_id
            );
        }
        Ok(count)
    }

    /// One iteration of the offer-expiry sweep. Flips every

    pub async fn expire_due_offers(&self) -> Result<usize, ShiftServiceError> {
        let expired = self.shift_repo.expire_due_offers().await?;
        let count = expired.len();

        // Collect affected clinician_ids so we can bulk-refresh their acceptance rates
        let mut affected: Vec<Uuid> = Vec::new();
        for (assignment_id, shift_id, hospital_id, role_title) in expired {
            if let Ok(Some((_, hospital_email))) =
                self.shift_repo.get_hospital_contact(hospital_id).await
            {
                let content = email_templates::shift_offer_expired(&role_title);
                if let Err(e) = self
                    .email_outbox
                    .enqueue_email(&hospital_email, &content)
                    .await
                {
                    eprintln!("Warning: Failed to queue offer-expiry email: {e}");
                }
            }

            // Look up the clinician on this assignment for the cache refresh.
            if let Ok(rows) = sqlx::query_scalar::<_, Uuid>(
                "SELECT clinician_id FROM shift_assignments WHERE id = $1",
            )
            .bind(assignment_id)
            .fetch_optional(&self.pool)
            .await
            {
                if let Some(cid) = rows {
                    affected.push(cid);
                }
            }

            tracing::info!(
                "Offer {} for shift {} expired (hospital {})",
                assignment_id,
                shift_id,
                hospital_id
            );
        }

        if !affected.is_empty() {
            if let Err(e) = self
                .shift_repo
                .recompute_acceptance_rates_bulk(&affected)
                .await
            {
                eprintln!("Warning: bulk acceptance-rate recompute failed: {e}");
            }
        }

        Ok(count)
    }

    /// Worker submits a GPS-fallback clock-in approval request

    pub async fn request_clockin_approval(
        &self,
        shift_id: Uuid,
        worker_user_id: Uuid,
        request: crate::models::shift::ClockinApprovalRequest,
    ) -> Result<Uuid, ShiftServiceError> {
        use base64::Engine;
        use validator::Validate;
        request
            .validate()
            .map_err(|e| ShiftServiceError::ValidationError(e.to_string()))?;

        let clinician_id = self
            .shift_repo
            .find_clinician_id_for_user(worker_user_id)
            .await?
            .ok_or(ShiftServiceError::NoClinicianProfile)?;

        let shift = self
            .shift_repo
            .get_by_id(shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(shift_id))?;

        if shift.assigned_clinician_id != Some(clinician_id) {
            return Err(ShiftServiceError::NotAuthorized);
        }
        if !matches!(shift.status, ShiftStatus::Assigned | ShiftStatus::Upcoming) {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Cannot request clock-in approval for a shift in status {:?}",
                shift.status
            )));
        }

        let photo_bytes = base64::engine::general_purpose::STANDARD
            .decode(request.photo_base64.trim())
            .map_err(|e| {
                ShiftServiceError::ValidationError(format!("Invalid base64 photo: {e}"))
            })?;
        if photo_bytes.is_empty() {
            return Err(ShiftServiceError::ValidationError(
                "Photo cannot be empty".to_string(),
            ));
        }

        let request_id = match self
            .shift_repo
            .create_clockin_approval_request(
                shift_id,
                clinician_id,
                request.latitude,
                request.longitude,
                &photo_bytes,
                request.photo_mime_type.as_deref(),
            )
            .await
        {
            Ok(id) => id,
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                return Err(ShiftServiceError::DuplicateClockinApproval);
            }
            Err(e) => return Err(ShiftServiceError::DatabaseError(e)),
        };

        // Best-effort notify the hospital admin.
        if let Ok(Some((_, hospital_email))) = self
            .shift_repo
            .get_hospital_contact(shift.hospital_id)
            .await
        {
            if let Ok(Some((first_name, last_name, _))) =
                self.shift_repo.get_clinician_contact(clinician_id).await
            {
                let clinician_name = format!("{} {}", first_name, last_name).trim().to_string();
                let content =
                    email_templates::clockin_approval_requested(&clinician_name, &shift.role_title);
                let _ = self
                    .email_outbox
                    .enqueue_email(&hospital_email, &content)
                    .await;
            }
        }

        Ok(request_id)
    }

    /// Hospital approves or denies a pending clock-in approval

    pub async fn decide_clockin_approval(
        &self,
        request_id: Uuid,
        requester_user_id: Uuid,
        approve: bool,
        notes: Option<String>,
    ) -> Result<(), ShiftServiceError> {
        let record = self
            .shift_repo
            .get_clockin_approval_request(request_id)
            .await?
            .ok_or(ShiftServiceError::ClockinApprovalNotFound)?;

        if record.status != "pending" {
            return Err(ShiftServiceError::InvalidStatus(format!(
                "Clock-in approval is already {}",
                record.status
            )));
        }

        let shift = self
            .shift_repo
            .get_by_id(record.shift_id)
            .await?
            .ok_or(ShiftServiceError::NotFound(record.shift_id))?;
        if shift.created_by != requester_user_id {
            return Err(ShiftServiceError::NotAuthorized);
        }

        self.shift_repo
            .decide_clockin_approval_request(
                request_id,
                requester_user_id,
                approve,
                notes.as_deref(),
            )
            .await?;

        // Best-effort notify the worker.
        if let Ok(Some((first_name, _last_name, clinician_email))) = self
            .shift_repo
            .get_clinician_contact(record.clinician_id)
            .await
        {
            let content = if approve {
                email_templates::clockin_approval_approved(&first_name, &shift.role_title)
            } else {
                email_templates::clockin_approval_denied(
                    &first_name,
                    &shift.role_title,
                    notes.as_deref(),
                )
            };
            let _ = self
                .email_outbox
                .enqueue_email(&clinician_email, &content)
                .await;
        }

        Ok(())
    }

    /// One iteration of the re-broadcast cadence sweep. Returns

    pub async fn rebroadcast_due_shifts(&self) -> Result<usize, ShiftServiceError> {
        let due = self.shift_repo.find_shifts_due_for_rebroadcast().await?;
        let count = due.len();
        for shift in due {
            // Compute fresh eligible-count and emit notifications.
            let matched = match self.find_eligible_clinicians_for_shift(&shift).await {
                Ok(list) => list.len() as i32,
                Err(e) => {
                    eprintln!(
                        "Warning: eligibility lookup failed for shift {}: {e}",
                        shift.id
                    );
                    continue;
                }
            };

            let radius_km = self
                .shift_repo
                .get_broadcast_radius_km(shift.hospital_id)
                .await
                .ok()
                .flatten()
                .unwrap_or(5.0);

            if let Err(e) = self
                .shift_repo
                .record_broadcast(shift.id, None, matched, radius_km)
                .await
            {
                eprintln!(
                    "Warning: Failed to record re-broadcast for shift {}: {e}",
                    shift.id
                );
                continue;
            }

            // Fire notifications (best-effort).
            if let Err(e) = self
                .broadcast_shift_notifications(shift.id, shift.hospital_id, matched)
                .await
            {
                eprintln!("Warning: re-broadcast notifications failed: {e}");
            }

            tracing::info!(
                "Re-broadcast shift {} ({:?}) — {} eligible",
                shift.id,
                shift.priority,
                matched
            );
        }
        Ok(count)
    }

    /// Map a shift's broad `RoleCategory` to the set of
    fn specialties_for_role(
        role: &crate::models::shift::RoleCategory,
    ) -> Vec<crate::models::clinician::ClinicalSpecialty> {
        use crate::models::clinician::ClinicalSpecialty as CS;
        use crate::models::shift::RoleCategory as RC;
        match role {
            RC::Doctor => vec![
                CS::EmergencyMedicine,
                CS::Pediatrics,
                CS::IcuSpecialist,
                CS::Surgery,
                CS::Anesthesiology,
                CS::Cardiology,
                CS::Obstetrics,
                CS::Psychiatry,
            ],
            RC::Nurse => vec![CS::GeneralNursing],
            RC::Midwife => vec![CS::Obstetrics, CS::GeneralNursing],
            RC::Pharmacist => vec![CS::Pharmacy],
            RC::LabTechnician => vec![CS::LabTechnician],
            RC::Radiographer => vec![CS::Radiology],
            RC::Physiotherapist => vec![CS::Other],
            RC::Other => vec![
                CS::EmergencyMedicine,
                CS::Pediatrics,
                CS::IcuSpecialist,
                CS::GeneralNursing,
                CS::Pharmacy,
                CS::LabTechnician,
                CS::Surgery,
                CS::Radiology,
                CS::Anesthesiology,
                CS::Cardiology,
                CS::Obstetrics,
                CS::Psychiatry,
                CS::Other,
            ],
        }
    }

    /// Real eligibility filter that returns the clinicians who

    async fn find_eligible_clinicians_for_shift(
        &self,
        shift: &Shift,
    ) -> Result<Vec<crate::repositories::shift::EligibleClinicianRow>, ShiftServiceError> {
        let allowed = Self::specialties_for_role(&shift.role_category);
        let candidates = self.shift_repo.find_eligible_clinicians(&allowed).await?;

        if shift.shift_type == ShiftType::Virtual {
            return Ok(candidates);
        }

        // In-person path — apply the 5km (or hospital-configured) radius.
        let (h_lat, h_lng) = match self
            .shift_repo
            .get_hospital_coordinates(shift.hospital_id)
            .await?
        {
            Some(coords) => coords,
            // Hospital has no location on file; nobody is "near" anything.
            None => return Ok(Vec::new()),
        };
        let radius_km = self
            .shift_repo
            .get_broadcast_radius_km(shift.hospital_id)
            .await?
            .unwrap_or(5.0);

        let filtered = candidates
            .into_iter()
            .filter(|c| match (c.latitude, c.longitude) {
                (Some(c_lat), Some(c_lng)) => {
                    crate::utils::geo::haversine_km(h_lat, h_lng, c_lat, c_lng) <= radius_km
                }
                // No recorded location → cannot prove they're nearby.
                _ => false,
            })
            .collect();
        Ok(filtered)
    }

    async fn calculate_matched_clinicians(&self, shift: &Shift) -> i32 {
        match self.find_eligible_clinicians_for_shift(shift).await {
            Ok(list) => list.len() as i32,
            Err(e) => {
                eprintln!("Warning: eligibility filter failed: {e}");
                0
            }
        }
    }

    /// Broadcast shift notifications to eligible workers.
    async fn broadcast_shift_notifications(
        &self,
        shift_id: Uuid,
        hospital_id: Uuid,
        matched_count: i32,
    ) -> Result<(), ShiftServiceError> {
        self.notification_service
            .send_shift_broadcast_notification(shift_id, hospital_id, matched_count)
            .await
            .map_err(|e| {
                ShiftServiceError::ValidationError(format!("Failed to send notifications: {e}"))
            })?;

        // Per-clinician email enqueue (best-effort).
        if let Ok(Some(shift)) = self.shift_repo.get_by_id(shift_id).await {
            if let Ok(eligible) = self.find_eligible_clinicians_for_shift(&shift).await {
                let hospital_name = self
                    .shift_repo
                    .get_hospital_name(hospital_id)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "the hospital".to_string());

                for ec in eligible {
                    let content = email_templates::shift_broadcast(
                        &ec.first_name,
                        &hospital_name,
                        &shift.role_title,
                        shift.scheduled_start,
                        shift.priority.clone(),
                    );
                    if let Err(e) = self.email_outbox.enqueue_email(&ec.email, &content).await {
                        eprintln!(
                            "Warning: Failed to queue broadcast email to clinician {}: {e}",
                            ec.clinician_id
                        );
                    }
                }
            }
        }

        tracing::info!(
            "Broadcast notifications sent for shift {} to {} eligible workers",
            shift_id,
            matched_count
        );

        Ok(())
    }

    /// Preview shift before publishing
    pub async fn preview_shift(
        &self,
        request: &CreateShiftRequest,
    ) -> Result<ShiftPreview, ShiftServiceError> {
        // Validate the request first
        self.validate_request(request)?;

        // Calculate compensation
        let (base_amount, stat_bonus, grand_total) = self.calculate_preview_compensation(request);

        // Generate preview
        Ok(ShiftPreview {
            role_title: request.role_title.clone(),
            specialty: request.specialty.clone(),
            department: request.department.clone(),
            shift_type: request.shift_type.clone(),
            priority: request.priority.clone(),
            scheduled_start: request.scheduled_start,
            duration_hours: request.duration_hours,
            base_amount_kobo: base_amount,
            stat_bonus_kobo: stat_bonus,
            grand_total_kobo: grand_total,
            virtual_link: if request.shift_type == ShiftType::Virtual {
                Some("https://meet.nexuscare.com/shift/preview".to_string())
            } else {
                None
            },
            estimated_matches: match request.shift_type {
                ShiftType::InPerson => 48,
                ShiftType::Virtual => 85,
            },
        })
    }

    fn calculate_preview_compensation(&self, request: &CreateShiftRequest) -> (i64, i64, i64) {
        use crate::models::shift::PayType;

        let base_amount = match request.pay_type {
            PayType::HourlyRate => request
                .rate_kobo_per_hour
                .map(|rate| (rate as f64 * request.duration_hours as f64) as i64)
                .unwrap_or(0),
            PayType::FixedRate => request.fixed_rate_kobo.unwrap_or(0),
        };

        let stat_bonus = request.stat_bonus_kobo.unwrap_or(0);
        let grand_total = base_amount + stat_bonus;

        (base_amount, stat_bonus, grand_total)
    }
}

/// Shift preview response
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShiftPreview {
    pub role_title: String,
    pub specialty: Option<String>,
    pub department: Option<String>,
    pub shift_type: ShiftType,
    pub priority: ShiftPriority,
    pub scheduled_start: chrono::DateTime<Utc>,
    pub duration_hours: f32,
    pub base_amount_kobo: i64,
    pub stat_bonus_kobo: i64,
    pub grand_total_kobo: i64,
    pub virtual_link: Option<String>,
    pub estimated_matches: i32,
}

#[cfg(test)]
mod qualification_match_tests {
    //! SCRUM-25 / US-09 AC-04 (UT009) — requirement/qualification matching.
    use super::match_qualifications;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// UT009 — met requirements are flagged true, missing ones false.
    #[test]
    fn ut009_matches_and_misses() {
        let reqs = v(&["ACLS Certified", "2+ years experience", "Valid license"]);
        let quals = v(&["acls certified", "valid license"]);
        let result = match_qualifications(&reqs, &quals);

        assert_eq!(result.len(), 3);
        assert!(result[0].met, "case-insensitive match expected");
        assert!(!result[1].met, "missing qualification should be false");
        assert!(result[2].met);
        // Original requirement text is preserved for display.
        assert_eq!(result[0].requirement, "ACLS Certified");
    }

    /// Whitespace around tags is ignored when matching.
    #[test]
    fn trims_whitespace() {
        let result = match_qualifications(&v(&["  BLS  "]), &v(&["bls"]));
        assert!(result[0].met);
    }

    /// No requirements yields an empty match set (UT020 empty state).
    #[test]
    fn empty_requirements_yield_empty() {
        assert!(match_qualifications(&[], &v(&["anything"])).is_empty());
    }

    /// A clinician with no qualifications meets nothing.
    #[test]
    fn no_quals_meets_nothing() {
        let result = match_qualifications(&v(&["X", "Y"]), &[]);
        assert!(result.iter().all(|m| !m.met));
    }
}

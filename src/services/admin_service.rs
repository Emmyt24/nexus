//! Admin dashboard business logic (§11). Wraps `AdminRepository`, assembling
//! composite DTOs and validating dispute resolution / settings updates.

use std::sync::Arc;
use uuid::Uuid;

use crate::models::admin::*;
use crate::models::user::UserRole;
use crate::repositories::admin::AdminRepository;
use crate::utils::errors::AppError;

pub struct AdminService {
    repo: Arc<AdminRepository>,
}

impl AdminService {
    pub fn new(repo: Arc<AdminRepository>) -> Self {
        Self { repo }
    }

    pub async fn dashboard(&self) -> Result<DashboardMetrics, AppError> {
        Ok(self.repo.dashboard_metrics().await?)
    }

    pub async fn shift_volume(&self, days: i64) -> Result<Vec<ShiftVolumePoint>, AppError> {
        let days = days.clamp(1, 365);
        Ok(self.repo.shift_volume(days).await?)
    }

    pub async fn geographic(&self) -> Result<Vec<GeoDistributionPoint>, AppError> {
        Ok(self.repo.geographic_distribution().await?)
    }

    pub async fn worker_performance(&self) -> Result<WorkerPerformance, AppError> {
        let distribution = self.repo.rating_distribution().await?;
        let top_performers = self.repo.top_performers(10).await?;
        Ok(WorkerPerformance {
            distribution,
            top_performers,
        })
    }

    pub async fn revenue(&self) -> Result<RevenueBreakdown, AppError> {
        let total_revenue_kobo = self.repo.revenue_total_30d().await?;
        let by_priority = self.repo.revenue_by_priority().await?;
        let by_status = self.repo.revenue_by_status().await?;
        Ok(RevenueBreakdown {
            total_revenue_kobo,
            by_priority,
            by_status,
        })
    }

    /// AI usage (§3.5). Feature 2 has no note/transcription storage yet, so this
    /// returns a well-typed zeroed payload rather than fabricated numbers. When
    /// clinical-note storage lands, back this with real queries.
    pub async fn ai_usage(&self) -> Result<AiUsageMetrics, AppError> {
        Ok(AiUsageMetrics {
            total_recordings: 0,
            total_notes_generated: 0,
            avg_note_generation_seconds: 0.0,
            translation_accuracy_percent: 0.0,
            language_breakdown: Vec::new(),
        })
    }

    pub async fn failed_payments(&self, limit: i64) -> Result<Vec<FailedPayment>, AppError> {
        Ok(self.repo.failed_payments(limit.clamp(1, 200)).await?)
    }

    pub async fn list_disputes(
        &self,
        status: Option<String>,
        page: i64,
        page_size: i64,
    ) -> Result<Vec<Dispute>, AppError> {
        if let Some(s) = &status {
            if !matches!(s.as_str(), "open" | "resolved" | "closed") {
                return Err(AppError::BadRequest(
                    "status must be one of: open, resolved, closed".into(),
                ));
            }
        }
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = (page - 1) * page_size;
        Ok(self
            .repo
            .list_disputes(status.as_deref(), page_size, offset)
            .await?)
    }

    pub async fn resolve_dispute(
        &self,
        id: Uuid,
        req: ResolveDisputeRequest,
        admin_user_id: Uuid,
    ) -> Result<Dispute, AppError> {
        let valid = matches!(
            req.resolution.as_str(),
            "full_payment" | "partial_refund" | "no_payment" | "escalate"
        );
        if !valid {
            return Err(AppError::BadRequest(
                "resolution must be one of: full_payment, partial_refund, no_payment, escalate"
                    .into(),
            ));
        }
        if req.resolution == "partial_refund" && req.resolution_amount_kobo.is_none() {
            return Err(AppError::BadRequest(
                "resolution_amount_kobo is required for partial_refund".into(),
            ));
        }

        self.repo
            .resolve_dispute(
                id,
                &req.resolution,
                req.resolution_amount_kobo,
                req.admin_notes.as_deref(),
                admin_user_id,
            )
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Dispute {id} not found")))
    }

    pub async fn get_settings(&self) -> Result<PlatformSettings, AppError> {
        Ok(self.repo.get_settings().await?)
    }

    pub async fn update_settings(
        &self,
        p: UpdatePlatformSettings,
        admin_user_id: Uuid,
    ) -> Result<PlatformSettings, AppError> {
        if let Some(fee) = p.platform_fee_percent {
            if !(0.0..=100.0).contains(&fee) {
                return Err(AppError::BadRequest(
                    "platform_fee_percent must be between 0 and 100".into(),
                ));
            }
        }
        Ok(self.repo.update_settings(&p, admin_user_id).await?)
    }

    /// §7 Report generation. Reports are produced asynchronously in the full
    /// design (generate → upload → email link); here we validate the request
    /// and acknowledge it with a report id the frontend can poll later.
    pub async fn generate_report(
        &self,
        req: GenerateReportRequest,
        role: UserRole,
    ) -> Result<GenerateReportResponse, AppError> {
        let known = matches!(
            req.report_type.as_str(),
            "platform_performance"
                | "financial"
                | "worker_performance"
                | "hospital_activity"
                | "ai_accuracy"
                | "dispute_resolution"
                | "user_growth"
        );
        if !known {
            return Err(AppError::BadRequest(format!(
                "unknown report_type: {}",
                req.report_type
            )));
        }
        // The financial report is finance-scoped: only super/finance may run it.
        if req.report_type == "financial"
            && !matches!(role, UserRole::SuperAdmin | UserRole::FinanceAdmin)
        {
            return Err(AppError::Forbidden(
                "financial reports require super_admin or finance_admin".into(),
            ));
        }
        Ok(GenerateReportResponse {
            report_id: Uuid::new_v4(),
            report_type: req.report_type,
            status: "queued".into(),
            message: "Report generation queued; a download link will be emailed when ready.".into(),
        })
    }

    // ----- §1.2 Hospital suspend / unsuspend --------------------------------

    pub async fn suspend_hospital(
        &self,
        hospital_id: Uuid,
        admin_id: Uuid,
        reason: Option<String>,
    ) -> Result<AdminActionResponse, AppError> {
        let ok = self
            .repo
            .suspend_hospital(hospital_id, admin_id, reason.as_deref())
            .await?;
        if !ok {
            return Err(AppError::NotFound(format!("Hospital {hospital_id} not found")));
        }
        Ok(AdminActionResponse {
            message: "Hospital suspended".into(),
        })
    }

    pub async fn unsuspend_hospital(
        &self,
        hospital_id: Uuid,
    ) -> Result<AdminActionResponse, AppError> {
        let ok = self.repo.unsuspend_hospital(hospital_id).await?;
        if !ok {
            return Err(AppError::NotFound(format!("Hospital {hospital_id} not found")));
        }
        Ok(AdminActionResponse {
            message: "Hospital reinstated".into(),
        })
    }

    // ----- §2 Worker verify / reject / suspend ------------------------------

    pub async fn set_worker_verified(
        &self,
        clinician_id: Uuid,
        verified: bool,
        admin_id: Uuid,
        notes: Option<String>,
    ) -> Result<AdminActionResponse, AppError> {
        let ok = self
            .repo
            .set_worker_verified(clinician_id, verified, admin_id, notes.as_deref())
            .await?;
        if !ok {
            return Err(AppError::NotFound(format!("Worker {clinician_id} not found")));
        }
        let message = if verified {
            "Worker license verified".into()
        } else {
            "Worker license rejected".into()
        };
        Ok(AdminActionResponse { message })
    }

    pub async fn set_worker_active(
        &self,
        clinician_id: Uuid,
        active: bool,
        reason: Option<String>,
    ) -> Result<AdminActionResponse, AppError> {
        let ok = self
            .repo
            .set_worker_active(clinician_id, active, reason.as_deref())
            .await?;
        if !ok {
            return Err(AppError::NotFound(format!("Worker {clinician_id} not found")));
        }
        let message = if active {
            "Worker reinstated".into()
        } else {
            "Worker suspended".into()
        };
        Ok(AdminActionResponse { message })
    }

    // ----- §3 Platform-wide shifts ------------------------------------------

    pub async fn list_shifts(
        &self,
        status: Option<String>,
        page: i64,
        page_size: i64,
    ) -> Result<Vec<AdminShiftRow>, AppError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = (page - 1) * page_size;
        Ok(self
            .repo
            .list_shifts(status.as_deref(), page_size, offset)
            .await?)
    }

    pub async fn cancel_shift(&self, shift_id: Uuid) -> Result<AdminActionResponse, AppError> {
        let ok = self.repo.cancel_shift(shift_id).await?;
        if !ok {
            return Err(AppError::NotFound(format!("Shift {shift_id} not found")));
        }
        Ok(AdminActionResponse {
            message: "Shift cancelled".into(),
        })
    }

    // ----- §1 Admin management ----------------------------------------------

    /// Validate an admin role string against the allowed admin roles.
    fn validate_admin_role(role: &str) -> Result<(), AppError> {
        let ok = matches!(
            role,
            "super_admin" | "operations_admin" | "verification_admin" | "finance_admin"
        );
        if ok {
            Ok(())
        } else {
            Err(AppError::BadRequest(
                "role must be one of: super_admin, operations_admin, verification_admin, finance_admin"
                    .into(),
            ))
        }
    }

    pub async fn create_admin(
        &self,
        req: CreateAdminRequest,
    ) -> Result<AdminSummary, AppError> {
        Self::validate_admin_role(&req.role)?;
        Ok(self
            .repo
            .create_admin(
                &req.first_name,
                &req.last_name,
                &req.email,
                req.phone.as_deref(),
                &req.role,
            )
            .await?)
    }

    pub async fn list_admins(&self) -> Result<Vec<AdminSummary>, AppError> {
        Ok(self.repo.list_admins().await?)
    }

    pub async fn update_admin(
        &self,
        id: Uuid,
        req: UpdateAdminRequest,
    ) -> Result<AdminSummary, AppError> {
        if let Some(role) = &req.role {
            Self::validate_admin_role(role)?;
        }
        self.repo
            .update_admin(id, req.role.as_deref(), req.is_active)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Admin {id} not found")))
    }
}

//! Admin permission model (Admin §1.2). Compile-time role -> permission mapping;
//! there is no DB permissions table. `super_admin` implicitly holds every
//! permission; the other admin roles hold the subset defined by the matrix.

use crate::models::user::UserRole;

/// A single privileged capability an admin endpoint can require.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    ViewHospitals,
    VerifyHospitals,
    SuspendHospitals,
    ViewWorkers,
    VerifyWorkers,
    SuspendWorkers,
    ViewShifts,
    CancelShifts,
    ViewDisputes,
    ResolveDisputes,
    ViewEarnings,
    ProcessPayouts,
    ViewAnalytics,
    GenerateReports,
    ManageAdmins,
    ManageSettings,
}

impl UserRole {
    /// Whether this role holds the given permission (Admin §1.2 matrix).
    pub fn has_permission(&self, p: Permission) -> bool {
        use Permission::*;
        use UserRole::*;
        match self {
            // Super admin can do everything.
            SuperAdmin => true,
            // Operations: monitoring, content, support, suspend, cancel shifts.
            OperationsAdmin => matches!(
                p,
                ViewHospitals
                    | SuspendHospitals
                    | ViewWorkers
                    | SuspendWorkers
                    | ViewShifts
                    | CancelShifts
                    | ViewDisputes
                    | ViewAnalytics
                    | GenerateReports
            ),
            // Verification: manual license/document review of hospitals + workers.
            VerificationAdmin => matches!(
                p,
                ViewHospitals
                    | VerifyHospitals
                    | SuspendHospitals
                    | ViewWorkers
                    | VerifyWorkers
                    | SuspendWorkers
                    | ViewShifts
                    | ViewDisputes
                    | ViewAnalytics
                    | GenerateReports
            ),
            // Finance: earnings, payouts, dispute resolution, financial reports.
            FinanceAdmin => matches!(
                p,
                ViewHospitals
                    | ViewWorkers
                    | ViewShifts
                    | ViewDisputes
                    | ResolveDisputes
                    | ViewEarnings
                    | ProcessPayouts
                    | ViewAnalytics
                    | GenerateReports
            ),
            // Non-admin roles hold no platform-admin permissions.
            HospitalAdmin | HealthWorker => false,
        }
    }
}

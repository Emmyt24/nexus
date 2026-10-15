//! Hospital-scoped search across the entities a hospital owns.
//!
//! Business rules and authorization live here, as everywhere in this codebase;
//! the SQL is in `repositories::search`. Two rules in particular:
//!
//!  * The tenant comes from `Claims`, never from a parameter. A `hospital_id`
//!    query argument on a search endpoint would be an open invitation to read
//!    another hospital's data, and the route guard cannot catch it because the
//!    role would be legitimate.
//!  * Patients are only searched for roles that may read patient data. The
//!    bucket comes back empty rather than erroring for other roles, so a
//!    clinician searching does not get a 403 for the whole request because one
//!    of four buckets was not permitted.

use std::sync::Arc;

use uuid::Uuid;

use crate::models::search::HospitalSearchResults;
use crate::models::user::{Claims, UserRole};
use crate::repositories::search::SearchRepository;
use crate::utils::errors::AppError;

/// Shortest query accepted. One character matches most of a table on a
/// substring search and is never a real search.
const MIN_QUERY_LEN: usize = 2;

/// Per-bucket cap. The four queries are sequential scans, so this bounds the
/// work as well as the response.
const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 50;

pub struct SearchService {
    repo: Arc<SearchRepository>,
}

impl SearchService {
    pub fn new(repo: Arc<SearchRepository>) -> Self {
        Self { repo }
    }

    /// Search within the caller's own hospital.
    ///
    /// `types` optionally narrows which buckets are queried — an unknown name is
    /// an error rather than silently ignored, so a client typo surfaces as a 422
    /// instead of an inexplicably empty bucket.
    pub async fn search(
        &self,
        claims: &Claims,
        query: &str,
        types: Option<&str>,
        limit: Option<i64>,
    ) -> Result<HospitalSearchResults, AppError> {
        let query = query.trim();
        if query.chars().count() < MIN_QUERY_LEN {
            return Err(AppError::Validation(format!(
                "search query must be at least {MIN_QUERY_LEN} characters"
            )));
        }

        let hospital_id = Self::tenant_for(claims)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let wanted = Self::parse_types(types)?;

        let mut results = HospitalSearchResults {
            query: query.to_string(),
            shifts: Vec::new(),
            clinicians: Vec::new(),
            patients: Vec::new(),
            handovers: Vec::new(),
        };

        if wanted.shifts {
            results.shifts = self.repo.search_shifts(hospital_id, query, limit).await?;
        }
        if wanted.clinicians {
            results.clinicians = self
                .repo
                .search_clinicians(hospital_id, query, limit)
                .await?;
        }
        if wanted.patients && Self::may_read_patients(&claims.role) {
            results.patients = self.repo.search_patients(hospital_id, query, limit).await?;
        }
        if wanted.handovers {
            results.handovers = self.repo.search_handovers(hospital_id, query, limit).await?;
        }

        Ok(results)
    }

    /// The hospital whose data this caller may search.
    ///
    /// A platform admin has no `hospital_id`, so there is no tenant to scope to
    /// and nothing sensible to return — `GET /admin/search` is their endpoint.
    /// Refusing here is better than defaulting to "all hospitals", which is how
    /// a search endpoint becomes a cross-tenant dump.
    fn tenant_for(claims: &Claims) -> Result<Uuid, AppError> {
        match claims.role {
            UserRole::HospitalAdmin | UserRole::SuperAdmin => claims
                .hospital_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or_else(|| {
                    AppError::Forbidden(
                        "This search is scoped to a hospital; use /admin/search for \
                         platform-wide results"
                            .to_string(),
                    )
                }),
            _ => Err(AppError::Forbidden(
                "Role may not search hospital records".to_string(),
            )),
        }
    }

    /// Patient rows are clinical data. Only the hospital's own administrators
    /// and a super admin acting within a hospital see them here.
    fn may_read_patients(role: &UserRole) -> bool {
        matches!(role, UserRole::HospitalAdmin | UserRole::SuperAdmin)
    }

    fn parse_types(types: Option<&str>) -> Result<WantedTypes, AppError> {
        let Some(raw) = types else {
            return Ok(WantedTypes::all());
        };
        if raw.trim().is_empty() {
            return Ok(WantedTypes::all());
        }

        let mut wanted = WantedTypes::none();
        for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "shift" | "shifts" => wanted.shifts = true,
                "clinician" | "clinicians" => wanted.clinicians = true,
                "patient" | "patients" => wanted.patients = true,
                "handover" | "handovers" => wanted.handovers = true,
                other => {
                    return Err(AppError::Validation(format!(
                        "unknown search type '{other}'; expected shift, clinician, patient \
                         or handover"
                    )))
                }
            }
        }
        Ok(wanted)
    }
}

#[derive(Debug, Clone, Copy)]
struct WantedTypes {
    shifts: bool,
    clinicians: bool,
    patients: bool,
    handovers: bool,
}

impl WantedTypes {
    fn all() -> Self {
        Self {
            shifts: true,
            clinicians: true,
            patients: true,
            handovers: true,
        }
    }

    fn none() -> Self {
        Self {
            shifts: false,
            clinicians: false,
            patients: false,
            handovers: false,
        }
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
    fn unknown_types_are_rejected() {
        let err = SearchService::parse_types(Some("shift,wallet")).unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[test]
    fn absent_or_empty_types_means_everything() {
        let all = SearchService::parse_types(None).unwrap();
        assert!(all.shifts && all.clinicians && all.patients && all.handovers);
        let all = SearchService::parse_types(Some("  ")).unwrap();
        assert!(all.shifts && all.clinicians && all.patients && all.handovers);
    }

    #[test]
    fn types_accept_singular_and_plural() {
        let w = SearchService::parse_types(Some("shift,clinicians")).unwrap();
        assert!(w.shifts && w.clinicians);
        assert!(!w.patients && !w.handovers);
    }

    #[test]
    fn only_hospital_roles_may_read_patients() {
        assert!(SearchService::may_read_patients(&UserRole::HospitalAdmin));
        assert!(SearchService::may_read_patients(&UserRole::SuperAdmin));
        assert!(!SearchService::may_read_patients(&UserRole::HealthWorker));
        assert!(!SearchService::may_read_patients(&UserRole::OperationsAdmin));
    }

    #[test]
    fn a_health_worker_cannot_search_hospital_records() {
        let err = SearchService::tenant_for(&claims(UserRole::HealthWorker, None)).unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)), "got {err:?}");
    }

    #[test]
    fn a_hospital_admin_without_a_hospital_is_refused_not_defaulted() {
        // The dangerous alternative would be treating "no hospital" as "every
        // hospital", which is how a scoped search becomes a tenant dump.
        let err = SearchService::tenant_for(&claims(UserRole::HospitalAdmin, None)).unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)), "got {err:?}");
    }

    #[test]
    fn a_platform_admin_is_sent_to_the_admin_endpoint() {
        let err =
            SearchService::tenant_for(&claims(UserRole::OperationsAdmin, None)).unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)), "got {err:?}");
    }

    #[test]
    fn a_hospital_admins_tenant_comes_from_the_token() {
        let hospital_id = Uuid::new_v4();
        let got =
            SearchService::tenant_for(&claims(UserRole::HospitalAdmin, Some(hospital_id))).unwrap();
        assert_eq!(got, hospital_id);
    }
}

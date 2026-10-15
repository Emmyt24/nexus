//! Shapes for the hospital-scoped search surface.
//!
//! Kept separate from `models::admin`'s `SearchHit`/`SearchResults` on purpose.
//! Those serve `GET /admin/search`, which is platform-wide and returns a
//! clinician's email and licence number — projecting that into a hospital-facing
//! response would disclose the whole clinician roster across tenants. Reusing
//! the type would make that one careless `SELECT` away, so the hospital hit
//! carries a deliberately narrower set of fields.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// One match. `kind` is redundant with the bucket it arrives in, and is included
/// anyway so a client that flattens the buckets for a combined list does not
/// have to track provenance.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, FromRow)]
pub struct HospitalSearchHit {
    pub id: Uuid,
    /// `shift`, `clinician`, `patient` or `handover`.
    pub kind: String,
    pub title: String,
    pub subtitle: Option<String>,
    /// The shift a handover belongs to, so the client can link through. NULL for
    /// the other kinds.
    pub shift_id: Option<Uuid>,
    /// What the result is ordered by within its bucket — creation or event time.
    pub occurred_at: Option<DateTime<Utc>>,
}

/// Results grouped by entity.
///
/// Grouped rather than one ranked list because the four queries have no shared
/// relevance score: an `ILIKE` match on a shift title is not comparable to one
/// on a patient name, and inventing a cross-entity ranking would be guesswork
/// dressed as precision. The client decides how to interleave them.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HospitalSearchResults {
    /// The trimmed query that was run.
    pub query: String,
    pub shifts: Vec<HospitalSearchHit>,
    /// Only clinicians with a working relationship to this hospital.
    pub clinicians: Vec<HospitalSearchHit>,
    /// Empty unless the caller's role may read patient data.
    pub patients: Vec<HospitalSearchHit>,
    pub handovers: Vec<HospitalSearchHit>,
}

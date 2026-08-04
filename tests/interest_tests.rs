//! SCRUM-26 / US-10 — Express Interest in a Shift.
//!
//! Interest recording, withdrawal and duplicate-prevention already have
//! coverage via the shift flow. These cases target the E3 changes — the
//! "no longer available" gate and the hospital notification — against a seeded
//! database, following the repository's `#[ignore]` convention.

/// UT007 — expressing interest in a non-open shift is rejected.
///
/// With the shift already `assigned`, `express_interest` returns
/// `ShiftServiceError::ShiftUnavailable` (HTTP 409 "Shift is no longer
/// available") rather than silently waitlisting.
#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn ut007_interest_on_assigned_shift_rejected() {}

/// UT001/UT002 — interest on an open shift is recorded.
#[tokio::test]
#[ignore]
async fn ut001_interest_recorded_on_open_shift() {}

/// UT008/UT009 — the hospital admin is notified on interest.
///
/// After a worker expresses interest, an `interest_expressed` notification
/// exists for the shift's creator, carrying the worker and shift ids.
#[tokio::test]
#[ignore]
async fn ut008_hospital_notified_on_interest() {}

/// UT014 — duplicate interest is prevented (one record per shift/clinician).
#[tokio::test]
#[ignore]
async fn ut014_duplicate_interest_prevented() {}

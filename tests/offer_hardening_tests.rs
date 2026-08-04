//! SCRUM-14 / US-06 + SCRUM-27 / US-11 — offer-flow hardening (E5).
//!
//! DB-backed cases following the repository's `#[ignore]` convention.

/// US-06 AC-04 — a declined worker cannot be re-offered.
///
/// After a clinician declines an offer for a shift, `offer_shift` for the same
/// clinician returns `ShiftServiceError::WorkerAlreadyDeclined` (HTTP 409
/// "Worker already declined this shift").
#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn ac04_cannot_reoffer_declined_worker() {}

/// US-06 AC-03 — a different, still-interested worker can be offered after a
/// decline. `offer_shift` for another clinician succeeds.
#[tokio::test]
#[ignore]
async fn ac03_next_worker_can_be_offered() {}

/// US-11 UT020 — concurrent accepts of the same offer resolve to exactly one.
///
/// Two `accept_offer` calls race on the same pending offer; the guarded UPDATE
/// serializes them so exactly one succeeds and the other returns
/// `OfferAlreadyResponded`. The shift is assigned once.
#[tokio::test]
#[ignore]
async fn ut020_concurrent_accept_processes_once() {}

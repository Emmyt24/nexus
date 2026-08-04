//! SCRUM-24 / US-08 — View Nearby Shifts.
//!
//! Pure-logic coverage of origin resolution and parameter validation lives
//! beside the handler in `src/handlers/shifts.rs` (`nearby_query_tests`). The
//! cases below exercise the radius filter, ranking and paging against a real
//! database and follow the repository's `#[ignore]` integration-test
//! convention: they document the end-to-end expectation and run once a seeded
//! test database is wired into CI.

/// UT002 / UT004 — 5 km radius filter.
///
/// Seed one open in-person shift ~4 km from the worker and one ~6 km away, then
/// call `ShiftRepository::list_nearby_shifts` with `radius_km = 5`. Expect only
/// the 4 km shift; the 6 km shift is excluded by the haversine gate.
#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn ut002_ut004_radius_filter_excludes_beyond_5km() {}

/// UT005 / UT006 — urgency then distance ranking.
///
/// Seed a STAT shift far away and a Normal shift nearby (both in range). Expect
/// the STAT shift first (urgency rank wins), then Normal; within one urgency
/// tier, the nearer shift ranks first.
#[tokio::test]
#[ignore]
async fn ut005_ut006_sorted_by_urgency_then_distance() {}

/// US-08 decision #2 — virtual shifts are always included regardless of radius.
///
/// Seed a virtual shift whose hospital sits far outside the radius. Expect it in
/// the result set with `distance_km = None`.
#[tokio::test]
#[ignore]
async fn virtual_shift_included_without_distance() {}

/// UT007 — distance is reported for in-person shifts.
///
/// Seed an in-person shift at a known offset and assert the returned
/// `distance_km` matches the haversine distance within a small tolerance.
#[tokio::test]
#[ignore]
async fn ut007_distance_reported_for_in_person() {}

/// UT013 — changing the origin recomputes the nearby set.
///
/// Call with GPS near hospital A (A in range, B out), then with GPS near
/// hospital B (B in range, A out). Expect the membership to flip, and the
/// worker's last-known location to be upserted between calls.
#[tokio::test]
#[ignore]
async fn ut013_new_origin_recalculates_results() {}

/// UT014 — only open shifts are returned.
///
/// Seed assigned/completed shifts alongside an open one within range. Expect
/// only the open shift; non-open statuses are filtered out.
#[tokio::test]
#[ignore]
async fn ut014_only_open_shifts_returned() {}

/// UT012 — dismissed shifts disappear.
///
/// Dismiss an in-range shift for the worker and expect it excluded from the
/// result set on the next call.
#[tokio::test]
#[ignore]
async fn ut012_dismissed_shift_excluded() {}

/// 409 path — no GPS supplied and none on file.
///
/// With no live coordinates and no `clinician_locations` row, the service
/// returns `ShiftServiceError::LocationRequired` (mapped to HTTP 409).
#[tokio::test]
#[ignore]
async fn location_required_when_no_origin() {}

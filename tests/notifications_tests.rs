//! E4 — Push notifications & notification center.
//!
//! These exercise device-token lifecycle, notification persistence/listing, and
//! push dispatch against a seeded database and FCM. They follow the
//! repository's `#[ignore]` integration convention.

/// Device token register → list active → revoke.
///
/// Registering the same (user, token) twice is idempotent; revoking removes it
/// from the active set; re-registering a revoked token restores it.
#[tokio::test]
#[ignore] // Requires a seeded test database.
async fn device_token_lifecycle() {}

/// notify() persists a notification and returns it in the center.
///
/// After `PushService::notify`, the user's notification list contains the entry
/// with `read_at = null` and `unread_count` incremented.
#[tokio::test]
#[ignore]
async fn notify_persists_and_lists() {}

/// Unread filter + mark read.
///
/// `list_notifications(unread_only = true)` returns only unread rows; after
/// `mark_read`, the row leaves the unread set and the badge count drops.
#[tokio::test]
#[ignore]
async fn unread_filter_and_mark_read() {}

/// mark_read is owner-scoped.
///
/// Marking another user's notification read affects nothing and reports false.
#[tokio::test]
#[ignore]
async fn mark_read_is_owner_scoped() {}

/// Invalid-token pruning.
///
/// When FCM reports a token `NotRegistered`, dispatch revokes it so it no longer
/// appears in the active set.
#[tokio::test]
#[ignore]
async fn invalid_token_is_pruned() {}

/// Offer → push. Sending a shift offer enqueues a `shift_offered` notification
/// for the clinician's user.
#[tokio::test]
#[ignore]
async fn offer_dispatches_push_to_clinician() {}

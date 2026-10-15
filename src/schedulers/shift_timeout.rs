//! Shift-timeout sweep — closes shifts whose scheduled end has passed.
//!
//! Without this a shift that nobody closed out stays `in_progress` (or
//! `assigned`) forever, holding the hospital's escrow in `held_kobo` and
//! counting against `clinician_has_active_assignment`, which blocks the
//! clinician from ever being assigned again. `cancel_shift` cannot help: it
//! refuses anything past `upcoming`.
//!
//! Same shape as `OfferExpiryScheduler` deliberately: one interval, skip missed
//! ticks, discard the first, log per tick. The two statements behind
//! `close_timed_out_shifts` claim their rows atomically, so unlike the
//! select-then-act schedulers this one is safe if more than one instance runs.

use std::sync::Arc;
use std::time::Duration;

use crate::services::shift_service::ShiftService;

pub struct ShiftTimeoutScheduler {
    service: Arc<ShiftService>,
    poll_secs: u64,
}

impl ShiftTimeoutScheduler {
    pub fn new(service: Arc<ShiftService>) -> Self {
        let poll_secs = std::env::var("SHIFT_TIMEOUT_POLL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        Self { service, poll_secs }
    }

    pub async fn run(self) {
        let mut interval = tokio::time::interval(Duration::from_secs(self.poll_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;

        loop {
            interval.tick().await;
            match self.service.close_timed_out_shifts().await {
                Ok(0) => {}
                Ok(n) => tracing::info!("Shift-timeout scheduler closed {n} shift(s)"),
                Err(e) => tracing::error!("Shift-timeout scheduler tick failed: {e}"),
            }
        }
    }
}

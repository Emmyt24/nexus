// ! Video-consultation reconciliation sweep.
// !
// ! Webhook idempotency is recorded *before* the event is processed, so a crash
// ! mid-flight leaves the dedupe row behind and LiveKit's retry is swallowed as
// ! a duplicate — the event is then lost forever. This sweep derives state from
// ! LiveKit and the database rather than from webhooks, which is what closes
// ! that hole. It is also the only place clock-out is ever automated.

use std::sync::Arc;
use std::time::Duration;

use crate::services::video_service::VideoService;

pub struct VideoSessionReconciler {
    service: Arc<VideoService>,
    poll_secs: u64,
}

impl VideoSessionReconciler {
    pub fn new(service: Arc<VideoService>) -> Self {
        let poll_secs = std::env::var("VIDEO_RECONCILE_POLL_SECS")
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
            match self.service.reconcile_sessions().await {
                Ok(report) if report.is_empty() => {}
                Ok(report) => tracing::info!(
                    "Video reconciler recovered {} join(s), ended {} session(s), \
                     clocked out {} worker(s), sent {} handover reminder(s)",
                    report.joins_recovered,
                    report.sessions_ended,
                    report.clock_outs,
                    report.handover_reminders
                ),
                Err(e) => tracing::error!("Video reconciler tick failed: {e}"),
            }
        }
    }
}

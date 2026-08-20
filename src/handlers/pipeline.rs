use std::convert::Infallible;

use axum::{
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::routes::AppState;
use crate::utils::{errors::AppError, extract_claims};

/// GET /api/v1/pipeline/events
#[utoipa::path(
    get,
    path = "/api/v1/pipeline/events",
    responses(
        (status = 200, description = "text/event-stream of PipelineEvent — prediction_completed / prediction_failed")
    ),
    tag = "patients",
    summary = "Stream ML pipeline events for the caller's hospital",
    description = "Server-Sent Events stream. Each connection is filtered to the caller's own hospital_id (from the JWT) so hospitals never see each other's patient events. Kept alive with periodic pings."
)]
pub async fn pipeline_events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let claims = extract_claims(&headers)?;
    let hospital_id: Uuid = claims
        .hospital_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| {
            AppError::Forbidden("No hospital associated with this account".to_string())
        })?;

    let rx = state.pipeline_events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |msg| async move {
        match msg {
            Ok(event) if event.hospital_id() == hospital_id => {
                let payload = serde_json::to_string(&event).unwrap_or_default();
                Some(Ok(Event::default().event(event.kind()).data(payload)))
            }
            // Not this hospital's event, or the receiver lagged and dropped
            // some messages — either way, just skip it and keep streaming.
            Ok(_) | Err(_) => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

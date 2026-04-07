use std::time::Duration;

use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::rest::AppState;

pub async fn sse_events(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let rx     = state.bus_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| {
        match item {
            Ok(event) => {
                let payload = serde_json::json!({
                    "mac":          event.device_id.to_string(),
                    "online":       event.state.online,
                    "updated_at_ms": event.state.updated_at_ms,
                    "power":        event.state.power,
                    "brightness":   event.state.brightness,
                    "color_temp_k": event.state.color_temp_k,
                    "rgb":          event.state.rgb.map(|(r, g, b)| [r, g, b]),
                });
                let data = serde_json::to_string(&payload).unwrap_or_default();
                Some(Ok::<Event, std::convert::Infallible>(
                    Event::default().event("state_changed").data(data),
                ))
            }
            Err(_) => None, // lagged — skip
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("heartbeat"),
    )
}

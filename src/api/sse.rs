//! SSE handler for event streaming
//!
//! Single Responsibility: Stream server events (device + plugin) to authenticated clients.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::Response;
use futures::stream::select_all;
use futures::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::app::AppState;
use crate::device::types::DeviceEvent;
use crate::plugins::PluginEvent;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum ServerEvent {
    Device(DeviceEvent),
    Plugin(PluginEvent),
}

pub async fn sse_events(
    State(state): State<Arc<AppState>>,
) -> Result<Response, (axum::http::StatusCode, String)> {
    let device_rx = state.broadcaster.subscribe();
    let device_stream: futures::stream::BoxStream<'_, ServerEvent> = Box::pin(
        BroadcastStream::new(device_rx)
            .filter_map(|r| async move { r.ok().map(ServerEvent::Device) }),
    );

    let plugin_rx = state.plugin_events.subscribe();
    let plugin_stream: futures::stream::BoxStream<'_, ServerEvent> = Box::pin(
        BroadcastStream::new(plugin_rx)
            .filter_map(|r| async move { r.ok().map(ServerEvent::Plugin) }),
    );

    let streams = select_all(vec![device_stream, plugin_stream]);

    let body_stream = streams.filter_map(|event| async move {
        let json = serde_json::to_string(&event).ok()?;
        let sse = format!("data: {}\n\n", json);
        Some(Ok::<_, Infallible>(sse))
    });

    let body = axum::body::Body::from_stream(body_stream);

    #[allow(clippy::expect_used)]
    let response = Response::builder()
        .status(200)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .expect("static response builder cannot fail");

    Ok(response)
}

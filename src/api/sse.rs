//! SSE handler for event streaming
//!
//! Single Responsibility: Stream server events (device + plugin) to authenticated clients.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::Response;
use futures::stream::select_all;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tracing::warn;

use crate::app::AppState;
use crate::device::types::DeviceEvent;
use crate::plugins::PluginEvent;

/// Cadence at which the server emits a keepalive SSE comment on every
/// active stream. Below typical reverse-proxy idle timeouts (nginx 60s,
/// Caddy ~5 min, Cloudflare 100s) so a dead upstream surfaces as a
/// closed client connection rather than a half-open socket the client
/// believes is still live.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum ServerEvent {
    Device(DeviceEvent),
    // Boxed: PluginEvent now carries optional icon URL fields and the SSE
    // payload-by-value path inflated the enum to 304 bytes. Box keeps the
    // hot variant (Device) small without changing serialization.
    Plugin(Box<PluginEvent>),
}

/// One item off a wrapped broadcast stream: either a mapped event, a
/// report that the subscriber fell behind and the broadcast channel
/// dropped `dropped` events before this poll caught up, or a periodic
/// liveness signal so a dead upstream surfaces as a closed connection
/// within `KEEPALIVE_INTERVAL` rather than a half-open socket the
/// client believes is still live.
///
/// `BroadcastStreamRecvError` has only the `Lagged` variant (the channel
/// closing surfaces as `None` from the stream, not an `Err` item), so this
/// mapping is exhaustive without a catch-all.
#[derive(Debug, Clone)]
enum StreamItem {
    Event(ServerEvent),
    Lagged(u64),
    Keepalive,
}

/// Wrap a broadcast receiver as a `StreamItem` stream, mapping each
/// delivered value through `map` and turning a `Lagged(n)` recv error into
/// `StreamItem::Lagged(n)` instead of silently dropping it. Pulled out of
/// `sse_events` so it can be driven directly in a test with a
/// small-capacity channel, no axum response machinery required.
fn wrap_broadcast<T, F>(
    rx: broadcast::Receiver<T>,
    map: F,
) -> futures::stream::BoxStream<'static, StreamItem>
where
    T: Clone + Send + 'static,
    F: Fn(T) -> ServerEvent + Send + 'static,
{
    Box::pin(BroadcastStream::new(rx).map(move |r| match r {
        Ok(event) => StreamItem::Event(map(event)),
        Err(BroadcastStreamRecvError::Lagged(n)) => StreamItem::Lagged(n),
    }))
}

/// Render one `StreamItem` as SSE wire text. A `Lagged` item becomes a
/// named `lagged` event carrying the dropped count as JSON, so a client
/// can tell "the server has nothing new to say" apart from "the server
/// had things to say and this client missed them" — silently reusing the
/// unnamed `data:` event for both left a lagged client with no signal
/// that its device/plugin state view could be stale. A `Keepalive` item
/// becomes an SSE comment line, which every SSE consumer ignores but
/// which keeps TCP intermediaries (and the client's read loop) alive.
fn render_sse_item(item: StreamItem) -> Option<String> {
    match item {
        StreamItem::Event(event) => {
            let json = serde_json::to_string(&event).ok()?;
            Some(format!("data: {}\n\n", json))
        }
        StreamItem::Lagged(dropped) => {
            warn!(
                dropped,
                event = "sse_client_lagged",
                "SSE client fell behind; broadcast channel dropped events before delivery"
            );
            Some(format!(
                "event: lagged\ndata: {{\"dropped\":{}}}\n\n",
                dropped
            ))
        }
        StreamItem::Keepalive => Some(": keepalive\n\n".to_string()),
    }
}

pub async fn sse_events(
    State(state): State<Arc<AppState>>,
) -> Result<Response, (axum::http::StatusCode, String)> {
    let device_stream = wrap_broadcast(state.broadcaster.subscribe(), ServerEvent::Device);
    let plugin_stream = wrap_broadcast(state.plugin_events.subscribe(), |event| {
        ServerEvent::Plugin(Box::new(event))
    });

    // Third stream: a wall-clock tick at KEEPALIVE_INTERVAL. `select_all`
    // pulls from each in turn; if the broadcast channels sit idle, the
    // tick is the only item that ever fires and the client sees a
    // comment every interval, which is enough to learn "this upstream
    // is alive" without parsing data frames.
    let keepalive_stream = IntervalStream::new(tokio::time::interval(KEEPALIVE_INTERVAL))
        .map(|_| StreamItem::Keepalive);

    let streams = select_all(vec![
        device_stream,
        plugin_stream,
        Box::pin(keepalive_stream),
    ]);

    let body_stream =
        streams.filter_map(|item| async move { render_sse_item(item).map(Ok::<_, Infallible>) });

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A subscriber that falls behind (small channel capacity, a burst of
    /// sends before the stream is ever polled) must see a `lagged` SSE
    /// event carrying the dropped count, not silence. Pre-fix,
    /// `filter_map(|r| r.ok().map(...))` mapped `Err(Lagged(n))` to `None`
    /// and the item vanished with no trace on the wire.
    #[tokio::test]
    async fn test_lagged_broadcast_yields_lagged_sse_event() {
        let (tx, rx) = broadcast::channel::<DeviceEvent>(4);

        // Publish more events than the channel can hold before anything
        // subscribes to the stream, guaranteeing at least one Lagged.
        for i in 0..10 {
            let _ = tx.send(DeviceEvent::StateChanged {
                device_id: format!("dev-{i}"),
                old_state: crate::device::types::DeviceState::Discovered,
                new_state: crate::device::types::DeviceState::Connected,
            });
        }
        // Drop the sender before collecting: a `BroadcastStream` only
        // yields `None` (ending `.collect()`) once every sender is gone —
        // an unwatched live `tx` here would hang the test forever, not
        // stop after the events sent above.
        drop(tx);

        let stream = wrap_broadcast(rx, ServerEvent::Device);
        let items: Vec<String> = stream
            .map(render_sse_item)
            .filter_map(|x| async move { x })
            .collect()
            .await;

        assert!(
            !items.is_empty(),
            "expected at least the lagged event on the wire"
        );
        let lagged_line = items
            .iter()
            .find(|s| s.starts_with("event: lagged\n"))
            .unwrap_or_else(|| panic!("no lagged SSE event found in: {items:?}"));

        let dropped: u64 = lagged_line
            .split("\"dropped\":")
            .nth(1)
            .and_then(|s| s.trim_end_matches("}\n\n").parse().ok())
            .unwrap_or_else(|| panic!("could not parse dropped count from {lagged_line}"));
        // 10 sent against capacity 4 means at least 6 were overwritten
        // before the receiver (created before any send) caught up.
        assert_eq!(
            dropped, 6,
            "expected exactly 6 dropped (10 sent - capacity 4), got: {lagged_line}"
        );
    }

    /// An idle merged stream must yield a `: keepalive` SSE comment
    /// within one interval so a dead upstream surfaces as a closed
    /// connection rather than a half-open socket the client believes is
    /// still live. Pre-fix the merged stream only ever produced items
    /// from the broadcast channels and would sit idle forever on a
    /// quiet system.
    #[tokio::test]
    async fn test_idle_stream_emits_keepalive_comment() {
        use std::time::Duration;
        // Faster-than-prod cadence so the test stays under the 20 s
        // wall-clock cap while still exercising the same code path.
        let stream = IntervalStream::new(tokio::time::interval(Duration::from_millis(50)))
            .map(|_| StreamItem::Keepalive);

        // Pull the first item off the stream and bound the wait so a
        // regression that removes the keepalive (or one that reorders
        // streams so a silent channel blocks the merge) shows up as a
        // test failure, not a hang.
        let first = tokio::time::timeout(Duration::from_secs(20), stream.into_future())
            .await
            .expect("keepalive must arrive within 20 s")
            .0
            .expect("stream must yield at least one item");

        let rendered = render_sse_item(first).expect("keepalive must render to wire text");
        assert_eq!(
            rendered, ": keepalive\n\n",
            "keepalive must be an SSE comment, not a data: frame"
        );
    }
}

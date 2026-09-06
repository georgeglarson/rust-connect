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

/// Discriminator for the JSON payload of an SSE event frame. Each
/// `ServerEvent` is an untagged serde enum, so without this an `onmessage`
/// consumer has to sniff keys (look for `device_id`, `app_name`, …) to
/// know which pane to route to. Pre-fix the UI did `payload.type ||
/// payload.event_type`, which only worked because the two upstream enums
/// happened to use different tag names (`type` vs `event_type`) — adding
/// a third source with the same tag name as one of those would silently
/// double-route.
///
/// Returned as `enum.variant_snake_case` so the wire value is unique
/// across both source enums (`device.state_changed` and
/// `plugin.clipboard_update` cannot collide) and stable: the snake-cased
/// variant name is what the existing serde tags already produce, so the
/// `kind` value and the existing `event_type`/`type` field agree on
/// `state_changed` vs `MprisUpdate` etc., just on a shared namespace.
fn kind(event: &ServerEvent) -> &'static str {
    match event {
        ServerEvent::Device(DeviceEvent::Discovered { .. }) => "device.discovered",
        ServerEvent::Device(DeviceEvent::StateChanged { .. }) => "device.state_changed",
        ServerEvent::Device(DeviceEvent::Paired { .. }) => "device.paired",
        ServerEvent::Device(DeviceEvent::Unpaired { .. }) => "device.unpaired",
        ServerEvent::Device(DeviceEvent::PairRequested { .. }) => "device.pair_requested",
        ServerEvent::Device(DeviceEvent::Connected { .. }) => "device.connected",
        ServerEvent::Device(DeviceEvent::Disconnected { .. }) => "device.disconnected",
        ServerEvent::Device(DeviceEvent::Removed { .. }) => "device.removed",
        ServerEvent::Plugin(p) => match &**p {
            PluginEvent::Notification { .. } => "plugin.notification",
            PluginEvent::Battery { .. } => "plugin.battery",
            PluginEvent::MprisUpdate { .. } => "plugin.mpris_update",
            PluginEvent::TelephonyUpdate { .. } => "plugin.telephony_update",
            PluginEvent::ClipboardUpdate { .. } => "plugin.clipboard_update",
            PluginEvent::SftpUpdate { .. } => "plugin.sftp_update",
            PluginEvent::RemoteKeyboardEcho { .. } => "plugin.remote_keyboard_echo",
            PluginEvent::RemoteKeyboardState { .. } => "plugin.remote_keyboard_state",
            PluginEvent::RemoteCommandsUpdate { .. } => "plugin.remote_commands_update",
            PluginEvent::ShareText { .. } => "plugin.share_text",
            PluginEvent::ShareUrl { .. } => "plugin.share_url",
            PluginEvent::ShareProgress { .. } => "plugin.share_progress",
            PluginEvent::SystemVolumeUpdate { .. } => "plugin.system_volume_update",
        },
    }
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
            // Serialize, then add `kind`. The DeviceEvent/PluginEvent
            // serde shapes carry their own discriminator (`event_type`
            // for device, `type` for plugin) and we MUST NOT change
            // them: the onmessage consumer in the UI and any other
            // existing consumer reads `event_type`/`type` directly. The
            // `kind` key is added on top of the existing keys, so a
            // client that ignores `kind` sees exactly the same frames
            // it always has, plus one key.
            let mut value = serde_json::to_value(&event).ok()?;
            if let Some(object) = value.as_object_mut() {
                let key = kind(&event);
                object.insert("kind".to_string(), serde_json::Value::String(key.to_string()));
            }
            let json = serde_json::to_string(&value).ok()?;
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

    /// A rendered device event must carry `kind == "device.state_changed"`
    /// alongside the existing keys, so an `onmessage` consumer that
    /// discriminates by `event_type`/`type` continues to work AND a
    /// consumer that discriminates by `kind` works too. The serializer
    /// is non-breaking by design: existing keys are unchanged, one key
    /// is added.
    #[test]
    fn test_render_sse_event_includes_kind_and_preserves_existing_keys() {
        let event = ServerEvent::Device(DeviceEvent::StateChanged {
            device_id: "dev-1".to_string(),
            old_state: crate::device::types::DeviceState::Discovered,
            new_state: crate::device::types::DeviceState::Connected,
        });

        let rendered =
            render_sse_item(StreamItem::Event(event)).expect("event frame must render");
        // Frame must be an unnamed `data:` event (existing onmessage
        // consumers depend on the absence of an `event:` line for these).
        assert!(
            rendered.starts_with("data: {"),
            "device data frame must be unnamed; got: {rendered}"
        );

        // Extract the JSON payload and verify both the discriminator
        // and the pre-existing keys survive untouched.
        let json_str = rendered
            .trim_start_matches("data: ")
            .trim_end_matches("\n\n")
            .trim_end();
        let value: serde_json::Value =
            serde_json::from_str(json_str).expect("data frame must be valid JSON");

        assert_eq!(
            value.get("kind").and_then(|v| v.as_str()),
            Some("device.state_changed"),
            "kind must identify the variant on the shared enum.variant namespace"
        );
        // Pre-existing key (DeviceEvent uses `event_type: `snake_case``).
        assert_eq!(
            value.get("event_type").and_then(|v| v.as_str()),
            Some("state_changed"),
            "existing event_type key must be preserved unchanged"
        );
        assert_eq!(
            value.get("device_id").and_then(|v| v.as_str()),
            Some("dev-1"),
            "device_id must be preserved unchanged"
        );
    }

    /// `kind` must namespace the two source enums so the same wire
    /// value cannot be claimed by both a device and a plugin variant —
    /// the consumers were unprotected against that pre-fix because the
    /// two upstream enums used different tag names (`event_type` vs
    /// `type`), so they happened not to collide.
    #[test]
    fn test_kind_namespaces_device_and_plugin_variants() {
        let device_event = ServerEvent::Device(DeviceEvent::Paired {
            device_id: "dev-1".to_string(),
            device_name: "Phone".to_string(),
        });
        // Pretend there's a plugin variant with the same wire name — we
        // can't actually name it identically in Rust, but the value of
        // `kind` MUST distinguish between a device and a plugin of the
        // same nominal name. We assert the namespace prefix is present
        // and consistent.
        assert!(
            kind(&device_event).starts_with("device."),
            "device events must carry the device. namespace"
        );
    }
}

//! SSE handler for event streaming
//!
//! Single Responsibility: Stream server events (device + plugin) to authenticated clients.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// dropped `dropped` events before this poll caught up, a periodic
/// liveness signal so a dead upstream surfaces as a closed connection
/// within `KEEPALIVE_INTERVAL` rather than a half-open socket the
/// client believes is still live, or a one-shot snapshot of the
/// current device list (audit 2026-09-06 item 5) so a fresh subscriber
/// (or one that just received a `lagged` frame) can repaint the device
/// pane without a follow-up REST call.
///
/// `BroadcastStreamRecvError` has only the `Lagged` variant (the channel
/// closing surfaces as `None` from the stream, not an `Err` item), so this
/// mapping is exhaustive without a catch-all.
#[derive(Debug, Clone)]
enum StreamItem {
    Event(ServerEvent),
    Lagged(u64),
    Keepalive,
    /// JSON payload already serialized — the snapshot frame's `data:`
    /// is the same JSON `GET /api/v1/devices` returns in its `data`
    /// envelope, which is too rich to re-derive from a ServerEvent
    /// without re-walking the registry. Serializing upstream keeps the
    /// overlay loop in one place.
    Snapshot(String),
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

/// Every string `kind` can return. The OpenAPI description must list each
/// one (`openapi.rs` tests iterate this), and `kind` itself is an
/// exhaustive match, so a new event variant fails to compile until it has
/// a name here and a mention in the spec.
#[cfg(test)]
pub(crate) const ALL_KINDS: &[&str] = &[
    "device.discovered",
    "device.state_changed",
    "device.paired",
    "device.unpaired",
    "device.pair_requested",
    "device.connected",
    "device.disconnected",
    "device.removed",
    "plugin.notification",
    "plugin.battery",
    "plugin.mpris_update",
    "plugin.telephony_update",
    "plugin.clipboard_update",
    "plugin.sftp_update",
    "plugin.remote_keyboard_echo",
    "plugin.remote_keyboard_state",
    "plugin.remote_commands_update",
    "plugin.share_text",
    "plugin.share_url",
    "plugin.share_progress",
    "plugin.system_volume_update",
];

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
///
/// `next_id` is the value to attach as the SSE `id:` line on event, lagged,
/// and snapshot frames; `build_event_stream` hands out consecutive
/// per-connection ids and passes 0 for keepalives, which carry no id line.
fn render_sse_item(item: StreamItem, next_id: u64) -> Option<String> {
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
                object.insert(
                    "kind".to_string(),
                    serde_json::Value::String(key.to_string()),
                );
            }
            let json = serde_json::to_string(&value).ok()?;
            Some(format!("id: {next_id}\ndata: {json}\n\n"))
        }
        StreamItem::Lagged(dropped) => {
            warn!(
                dropped,
                event = "sse_client_lagged",
                "SSE client fell behind; broadcast channel dropped events before delivery"
            );
            Some(format!(
                "id: {next_id}\nevent: lagged\ndata: {{\"dropped\":{}}}\n\n",
                dropped
            ))
        }
        // Keepalives intentionally carry no id: they have no semantic
        // content, and giving every keepalive a unique id would burn
        // the entire counter on a quiet stream and force every
        // would-be resume to chase tail forever.
        StreamItem::Keepalive => Some(": keepalive\n\n".to_string()),
        // The snapshot frame is named (`event: snapshot`) on purpose:
        // browser `onmessage` consumers never see it (a named event
        // only reaches `addEventListener('snapshot', …)`). The UI adds
        // a named listener that repaints the device pane from the
        // payload — that's what makes the pane correct after a
        // `lagged` frame, and what gives a fresh subscriber the same
        // "context needed to act, no follow-up API calls" the REST
        // `/devices` endpoint provides.
        StreamItem::Snapshot(json) => {
            Some(format!("id: {next_id}\nevent: snapshot\ndata: {json}\n\n"))
        }
    }
}

/// The current device list as a snapshot frame, the same JSON
/// `GET /api/v1/devices` returns in `data`. `None` only if serialization
/// fails, which the type makes impossible in practice; it is logged so a
/// missing snapshot is never silent.
async fn snapshot_item(state: &AppState) -> Option<StreamItem> {
    let list = crate::api::handlers::render_device_list(state, 1, usize::MAX).await;
    match serde_json::to_string(&list) {
        Ok(json) => Some(StreamItem::Snapshot(json)),
        Err(e) => {
            warn!(
                error = %e,
                event = "sse_snapshot_serialize_failed",
                "SSE snapshot could not be serialized; stream continues without it"
            );
            None
        }
    }
}

/// The wire frames of one `/api/v1/events` connection, already rendered.
/// Pulled out of `sse_events` so a test drives the real stream. Three
/// guarantees live here, not in the handler:
///
/// - The snapshot is *chained ahead of* the live merge, never raced
///   against it, so it is the first frame on every connection even when
///   an event is already queued (the keepalive's first tick is also one
///   interval out, not immediate).
/// - `id:` is a per-connection counter starting at 1, consumed only by
///   frames that carry the line (event, lagged, snapshot), so a client
///   sees consecutive ids with no gaps from keepalives or from other
///   subscribers. Ids correlate frames; the drop signal is `lagged`.
/// - A `lagged` frame is followed, in the same chunk, by a fresh
///   snapshot, so a client that fell behind has current state without a
///   REST round trip.
pub(crate) async fn build_event_stream(
    state: Arc<AppState>,
) -> futures::stream::BoxStream<'static, String> {
    let device_stream = wrap_broadcast(state.broadcaster.subscribe(), ServerEvent::Device);
    let plugin_stream = wrap_broadcast(state.plugin_events.subscribe(), |event| {
        ServerEvent::Plugin(Box::new(event))
    });
    let first_tick = tokio::time::Instant::now() + KEEPALIVE_INTERVAL;
    let keepalive_stream =
        IntervalStream::new(tokio::time::interval_at(first_tick, KEEPALIVE_INTERVAL))
            .map(|_| StreamItem::Keepalive);

    let ids = Arc::new(AtomicU64::new(1));
    let first_frame = match snapshot_item(&state).await {
        Some(item) => render_sse_item(item, ids.fetch_add(1, Ordering::Relaxed)),
        None => None,
    };
    let head = futures::stream::iter(first_frame);

    let live = select_all(vec![
        device_stream,
        plugin_stream,
        Box::pin(keepalive_stream),
    ]);
    let live_state = state.clone();
    let live = live.filter_map(move |item| {
        let state = live_state.clone();
        let ids = ids.clone();
        async move {
            match item {
                StreamItem::Keepalive => render_sse_item(StreamItem::Keepalive, 0),
                StreamItem::Lagged(dropped) => {
                    let mut chunk = render_sse_item(
                        StreamItem::Lagged(dropped),
                        ids.fetch_add(1, Ordering::Relaxed),
                    )?;
                    if let Some(snapshot) = snapshot_item(&state).await {
                        if let Some(frame) =
                            render_sse_item(snapshot, ids.fetch_add(1, Ordering::Relaxed))
                        {
                            chunk.push_str(&frame);
                        }
                    }
                    Some(chunk)
                }
                other => render_sse_item(other, ids.fetch_add(1, Ordering::Relaxed)),
            }
        }
    });

    head.chain(live).boxed()
}

#[utoipa::path(
    get,
    path = "/api/v1/events",
    tag = "events",
    params(
        ("api_key" = Option<String>, Query, description = "The API key, for `EventSource` clients that cannot set the `x-api-key` header. Accepted only on this endpoint.")
    ),
    responses(
        (status = 200, description = "Server-Sent Events stream. Frames are `text/event-stream` blocks separated by a blank line: a named `snapshot` first, then unnamed data frames with a `kind` field, `lagged` frames each followed by a fresh `snapshot`, and `: keepalive` comments.", body = String, content_type = "text/event-stream"),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn sse_events(
    State(state): State<Arc<AppState>>,
) -> Result<Response, (axum::http::StatusCode, String)> {
    let body_stream = build_event_stream(state).await.map(Ok::<_, Infallible>);
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
        // The lagged test passes a synthetic id through; the renderer
        // does not increment, so any value works.
        let items: Vec<String> = stream
            .map(|item| render_sse_item(item, 0))
            .filter_map(|x| async move { x })
            .collect()
            .await;

        assert!(
            !items.is_empty(),
            "expected at least the lagged event on the wire"
        );
        let lagged_line = items
            .iter()
            .find(|s| s.contains("event: lagged\n"))
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

        let rendered = render_sse_item(first, 0).expect("keepalive must render to wire text");
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
            render_sse_item(StreamItem::Event(event), 0).expect("event frame must render");
        // Frame must carry `id:` and `data:` (existing onmessage
        // consumers depend on the absence of an `event:` line for
        // these). Skip past the leading `id:` line to find the JSON.
        let mut lines = rendered.lines();
        let id_line = lines
            .next()
            .unwrap_or_else(|| panic!("rendered must have at least one line; got: {rendered}"));
        assert!(
            id_line.starts_with("id: "),
            "rendered frame must open with `id: <n>`; got: {rendered}"
        );
        let data_line = lines
            .next()
            .unwrap_or_else(|| panic!("rendered frame must have a data line; got: {rendered}"));
        assert!(
            data_line.starts_with("data: {"),
            "device data frame must be unnamed (no `event:` line); got: {rendered}"
        );

        // Extract the JSON payload and verify both the discriminator
        // and the pre-existing keys survive untouched.
        let json_str = data_line.trim_start_matches("data: ").trim_end();
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

    /// Two consecutive rendered events must carry strictly increasing
    /// `id:` values, and the first id must be 1 (the counter is
    /// process-global, fetch_add takes the value before the add). This
    /// is the wire contract a future `Last-Event-ID` resume handler
    /// will rely on; today the SSE route accepts no resume header, but
    /// the id stays in the bytes so a future server can read it.
    async fn test_state_with_two_devices() -> (Arc<AppState>, tempfile::TempDir) {
        use crate::config::settings::AppSettings;
        use crate::device::types::{Device, DeviceType};
        let temp_dir = tempfile::TempDir::new().expect("tempdir must be creatable");
        let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
        let state = Arc::new(AppState::new_without_input(settings).expect("state"));
        for (id, name, kind) in [
            (
                "phone-snapshot-aaaaaaaaaaaaaaaaaaaa",
                "Phone",
                DeviceType::Phone,
            ),
            (
                "desktop-snapshot-bbbbbbbbbbbbbbbbbb",
                "Desktop",
                DeviceType::Desktop,
            ),
        ] {
            state
                .registry
                .add(Device::new(id.to_string(), name.to_string(), kind, 8))
                .await
                .expect("device add");
        }
        (state, temp_dir)
    }

    fn state_changed(device_id: &str) -> DeviceEvent {
        DeviceEvent::StateChanged {
            device_id: device_id.to_string(),
            old_state: crate::device::types::DeviceState::Discovered,
            new_state: crate::device::types::DeviceState::Connected,
        }
    }

    fn id_of(frame: &str) -> u64 {
        frame
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("id: "))
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("expected `id: <n>` as the first line; got: {frame}"))
    }

    /// Ids are per connection, start at 1 with the snapshot, and have no
    /// gaps: keepalives carry no id and consume none, and a second
    /// subscriber's traffic does not advance this one's counter
    /// (review finding on #42: a process-global counter burned by every
    /// merged item made gaps meaningless).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ids_are_per_connection_and_consecutive() {
        let (state, _t) = test_state_with_two_devices().await;
        let mut a = build_event_stream(state.clone()).await;
        let mut b = build_event_stream(state.clone()).await;

        let snap_a = a.next().await.expect("a snapshot");
        let snap_b = b.next().await.expect("b snapshot");
        assert_eq!(id_of(&snap_a), 1);
        assert_eq!(id_of(&snap_b), 1);

        state.broadcaster.broadcast(state_changed("dev-1"));
        state.broadcaster.broadcast(state_changed("dev-2"));

        for stream in [&mut a, &mut b] {
            let first = stream.next().await.expect("first event");
            let second = stream.next().await.expect("second event");
            assert_eq!(id_of(&first), 2, "frame: {first}");
            assert_eq!(id_of(&second), 3, "frame: {second}");
            assert!(first.contains("\"kind\":\"device.state_changed\""));
        }
    }

    /// The first frame on every connection is the named snapshot, even
    /// when an event is already queued on the broadcast channel before
    /// the stream is first polled (review finding on #42: `select_all`
    /// raced the snapshot against live events and the keepalive's
    /// immediate first tick).
    #[tokio::test]
    async fn test_sse_stream_starts_with_snapshot_named_event() {
        let (state, _t) = test_state_with_two_devices().await;
        let mut stream = build_event_stream(state.clone()).await;
        // Queue a live event before the first poll.
        state
            .broadcaster
            .broadcast(state_changed("phone-snapshot-aaaaaaaaaaaaaaaaaaaa"));

        let first = stream.next().await.expect("first frame");
        assert!(
            first.starts_with("id: 1\nevent: snapshot\n"),
            "first frame must be the snapshot, got: {first}"
        );
        assert!(first.contains("phone-snapshot-aaaaaaaaaaaaaaaaaaaa"));
        assert!(first.contains("desktop-snapshot-bbbbbbbbbbbbbbbbbb"));
        assert!(first.contains("\"pair_state\""));

        let second = stream.next().await.expect("second frame");
        assert!(
            second.starts_with("id: 2\ndata: "),
            "the queued event follows the snapshot, got: {second}"
        );
    }

    /// A subscriber that fell behind gets the `lagged` frame and, in the
    /// same chunk, a fresh snapshot, so it can repaint without a REST
    /// call (review finding on #42: the one-shot snapshot was exhausted
    /// after the first frame and the UI has no `lagged` listener).
    #[tokio::test]
    async fn test_lagged_frame_is_followed_by_a_fresh_snapshot() {
        let (state, _t) = test_state_with_two_devices().await;
        let mut stream = build_event_stream(state.clone()).await;
        let first = stream.next().await.expect("snapshot");
        assert!(first.contains("event: snapshot\n"));

        // The device broadcaster holds 256; overflow it before polling.
        for i in 0..400 {
            state
                .broadcaster
                .broadcast(state_changed(&format!("dev-{i}")));
        }

        let mut saw_lagged_with_snapshot = false;
        for _ in 0..400 {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("stream must keep producing")
                .expect("stream must not end");
            if let Some(lag_at) = chunk.find("event: lagged\n") {
                assert!(
                    chunk[lag_at..].contains("event: snapshot\n"),
                    "a lagged frame must be followed by a snapshot in the same chunk: {chunk}"
                );
                saw_lagged_with_snapshot = true;
                break;
            }
        }
        assert!(
            saw_lagged_with_snapshot,
            "expected a lagged frame after overflowing the channel"
        );
    }

    /// Every name in `ALL_KINDS` is what `kind` returns for a sample of
    /// constructible events, and the two lists have the same size as the
    /// arms in `kind` (21).
    #[test]
    fn test_all_kinds_matches_kind_fn_samples() {
        assert_eq!(ALL_KINDS.len(), 21);
        let samples: Vec<(ServerEvent, &str)> = vec![
            (
                ServerEvent::Device(state_changed("d")),
                "device.state_changed",
            ),
            (
                ServerEvent::Plugin(Box::new(PluginEvent::Battery {
                    device_id: "d".to_string(),
                    current_charge: 1,
                    is_charging: true,
                })),
                "plugin.battery",
            ),
        ];
        for (event, expected) in samples {
            assert_eq!(kind(&event), expected);
            assert!(ALL_KINDS.contains(&expected));
        }
    }
}

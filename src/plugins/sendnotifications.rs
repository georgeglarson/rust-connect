//! Send Notifications plugin
//!
//! Single Responsibility: Monitor local desktop notifications via D-Bus and
//! send them to connected devices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use tracing::{error, info, warn};
use zbus::Connection;

use crate::plugins::events::PluginEventBroadcaster;
use crate::plugins::plugin::Plugin;
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

/// Body of an inbound `kdeconnect.notification.request`.
///
/// `cancel` is the id of a notification the peer dismissed, as a **string**.
/// kdeconnect-kde writes it with
/// `np.set<QString>(QStringLiteral("cancel"), internalId)`
/// (plugins/notifications/notificationsplugin.cpp:143) and kdeconnect-android
/// reads it with `np.getString("cancel")`
/// (.../plugins/notifications/NotificationsPlugin.kt:529). It is never a bool.
///
/// `request` is the resend-everything flag (notificationsplugin.cpp:29,
/// ReceiveNotificationsPlugin.kt:39-41).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NotificationRequest {
    #[serde(default)]
    pub request: Option<bool>,
    #[serde(default, deserialize_with = "cancel_notification_id")]
    pub cancel: Option<String>,
}

/// Accepts `cancel` only as a non-empty string, yielding `None` for anything
/// else instead of failing the packet. rust-connect used to emit
/// `"cancel": false` on its own on-connect packet, so a peer on an older build
/// can still put a bool here; one malformed field should not cost us the packet.
fn cancel_notification_id<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(id) if !id.is_empty() => Some(id),
        _ => None,
    })
}

pub struct SendNotificationsPlugin {
    _plugin_events: Arc<PluginEventBroadcaster>,
    pairing_handler: Arc<crate::protocol::pairing::PairingHandler>,
    connection_manager: Option<Arc<crate::protocol::ConnectionManager>>,
    watcher_started: Arc<AtomicBool>,
}

impl SendNotificationsPlugin {
    pub fn new(
        _plugin_events: Arc<PluginEventBroadcaster>,
        pairing_handler: Arc<crate::protocol::pairing::PairingHandler>,
    ) -> Self {
        Self {
            _plugin_events,
            pairing_handler,
            connection_manager: None,
            watcher_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_connection_manager(mut self, cm: Arc<crate::protocol::ConnectionManager>) -> Self {
        self.connection_manager = Some(cm);
        self
    }

    pub fn enable_session_backend(&self) {
        self.try_start_watcher();
    }

    fn try_start_watcher(&self) {
        if self.watcher_started.load(Ordering::SeqCst) {
            return;
        }
        let Some(cm) = self.connection_manager.clone() else {
            return;
        };

        if tokio::runtime::Handle::try_current().is_err() {
            warn!(
                event = "sendnotifications_watcher_no_runtime",
                "No tokio runtime in scope; sendnotifications watcher not started"
            );
            return;
        }

        let watcher_started = self.watcher_started.clone();
        let pairing_handler = self.pairing_handler.clone();
        tokio::spawn(async move {
            if let Err(e) = monitor_notifications(cm, pairing_handler).await {
                error!(
                    error = %e,
                    event = "sendnotifications_watcher_failed",
                    "Notification monitor failed"
                );
                watcher_started.store(false, Ordering::SeqCst);
            }
        });
        self.watcher_started.store(true, Ordering::SeqCst);
    }
}

/// Build the `kdeconnect.notification` body for one desktop notification.
///
/// Field shapes are kdeconnect-kde's
/// plugins/sendnotifications/dbusnotificationslistener.cpp:317-329:
/// - `ticker` is the summary, then `": "` + the body when a body is present
///   (:301-304). Android renders ONLY the ticker
///   (kdeconnect-android .../receivenotifications/ReceiveNotificationsPlugin.kt:80,82,88
///   read `ticker` for content text, ticker and big-text; nothing in that file
///   reads `text`), so a body left out of the ticker never reaches the phone.
/// - `isClearable` is `timeout == -1` (:322): the notification that never
///   expires on its own is the one the user can dismiss.
/// - `text` is set only when the body is non-empty (:327-329).
///
/// Pure so the wire shape is testable without a session bus.
fn build_notification_body(
    id: u32,
    app_name: &str,
    summary: &str,
    text: &str,
    expire_timeout: i32,
) -> serde_json::Value {
    let ticker = if text.is_empty() {
        summary.to_string()
    } else {
        format!("{summary}: {text}")
    };

    let mut body = serde_json::json!({
        "id": id.to_string(),
        "appName": app_name,
        "ticker": ticker,
        "title": summary,
        "isClearable": expire_timeout == -1,
        "silent": false
    });

    if !text.is_empty() {
        body["text"] = serde_json::Value::String(text.to_string());
    }

    body
}

async fn monitor_notifications(
    cm: Arc<crate::protocol::ConnectionManager>,
    pairing_handler: Arc<crate::protocol::pairing::PairingHandler>,
) -> Result<()> {
    // create a private/dedicated connection for monitoring
    let conn = Connection::session().await?;

    // Become a monitor
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus.Monitoring"),
        "BecomeMonitor",
        &(
            vec!["type='method_call',interface='org.freedesktop.Notifications',member='Notify'"],
            0u32,
        ),
    )
    .await?;

    let mut stream = zbus::MessageStream::from(conn);
    let mut fake_id_counter: u32 = 1000;

    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "Error reading from monitor stream");
                continue;
            }
        };

        let header = msg.header();
        if header.interface().map(|i| i.as_str()) == Some("org.freedesktop.Notifications")
            && header.member().map(|m| m.as_str()) == Some("Notify")
        {
            let body = msg.body();
            // DBus signature: (susssasa{sv}i)
            if let Ok((
                app_name,
                replaces_id,
                _app_icon,
                summary,
                text,
                _actions,
                hints,
                _expire_timeout,
            )) = body.deserialize::<(
                String,
                u32,
                String,
                String,
                String,
                Vec<String>,
                std::collections::HashMap<String, zbus::zvariant::Value>,
                i32,
            )>() {
                // Drop notifications that rust-connect created from a remote device, to avoid echoing them back
                if hints.contains_key("x-kdeconnect-source-device") {
                    continue;
                }

                // Upstream drops these before doing anything else
                // (dbusnotificationslistener.cpp:295-297): with no summary
                // there is nothing for the phone to render.
                if summary.is_empty() {
                    continue;
                }

                // Just use replaces_id if > 0, otherwise invent one
                let id = if replaces_id > 0 {
                    replaces_id
                } else {
                    fake_id_counter = fake_id_counter.wrapping_add(1);
                    fake_id_counter
                };

                let payload =
                    build_notification_body(id, &app_name, &summary, &text, _expire_timeout);

                let packet = Packet::new("kdeconnect.notification".to_string(), payload);

                for device_id in cm.connected_device_ids().await {
                    if !pairing_handler.is_paired(&device_id).await {
                        continue;
                    }
                    if let Err(e) = cm.send_packet(&device_id, &packet).await {
                        warn!(
                            device_id = %device_id,
                            error = %e,
                            event = "sendnotifications_send_failed",
                            "Failed to send notification packet"
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

#[async_trait::async_trait]
impl Plugin for SendNotificationsPlugin {
    fn name(&self) -> &str {
        "sendnotifications"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.notification.request".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.notification".to_string()]
    }

    // Task 1.7: this plugin has no swappable Option<Arc<dyn Backend>> the
    // way clipboard/mpris/systemvolume/pausemusic/screensaver_inhibit do —
    // watcher_started IS the availability signal, set true only once
    // try_start_watcher has a connection_manager AND a tokio runtime, and
    // reset false if the spawned D-Bus monitor task fails.
    fn is_backend_available(&self) -> bool {
        self.watcher_started.load(Ordering::SeqCst)
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type != "kdeconnect.notification.request" {
            return Ok(None);
        }
        let req: NotificationRequest = packet.body_as("notification request")?;

        if let Some(dismissed_id) = req.cancel.as_deref() {
            // The peer dismissed a notification we sent it. kdeconnect-android
            // answers this by closing its local copy —
            // `service.cancelNotification(dismissedId)`,
            // .../plugins/notifications/NotificationsPlugin.kt:528-533.
            //
            // We deliberately stop at logging. Two reasons, both checked:
            //
            //  1. No upstream desktop implements the close, and no upstream
            //     client sends the packet. kdeconnect-kde's
            //     SendNotificationsPlugin declares this type incoming
            //     (plugins/sendnotifications/kdeconnect_sendnotifications.json)
            //     and has no receivePacket override at all
            //     (sendnotificationsplugin.cpp, 43 lines). The only upstream
            //     code that could send us one, Android's
            //     ReceiveNotificationsPlugin, puts nothing but `request: true`
            //     on it (ReceiveNotificationsPlugin.kt:37-43).
            //
            //  2. The ids we transmit are not org.freedesktop.Notifications
            //     server ids, so closing by them would close the wrong popup.
            //     `monitor_notifications` monitors Notify METHOD CALLS (see
            //     BecomeMonitor above); it never sees the reply carrying the
            //     server-assigned id, so when `replaces_id` is 0 it invents one
            //     from `fake_id_counter`. Those invented numbers collide with
            //     real ids belonging to unrelated notifications.
            //
            // Logging the id makes a peer that does start sending these visible
            // before anyone writes the close path.
            info!(
                device_id = %device_id,
                notification_id = %dismissed_id,
                event = "notification_cancel_received",
                "Peer dismissed a notification we sent; no local popup handle to close"
            );
        } else if req.request.unwrap_or(false) {
            // kdeconnect-kde does not re-send on request either; its
            // sendnotifications plugin has no receive path.
            info!(
                device_id = %device_id,
                event = "notification_resend_requested",
                "Peer asked for current notifications; not implemented, matching kdeconnect-kde"
            );
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn field<'a>(body: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        body.get(key).and_then(|v| v.as_str())
    }

    /// Loads the upstream-derived fixture literal at
    /// tests/fixtures/upstream-wire/sendnotifications/outgoing.json (cited
    /// against kdeconnect-kde dbusnotificationslistener.cpp:317-329) and
    /// asserts the rust plugin's body matches it field-for-field.
    #[tokio::test]
    async fn test_ticker_carries_summary_and_body() {
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/sendnotifications/outgoing.json"),
            )
            .expect("sendnotifications/outgoing.json"),
        )
        .expect("sendnotifications/outgoing.json parses");

        let body = build_notification_body(42, "Signal", "Camille", "on my way", 5000);
        assert_eq!(body, expected);
        assert_eq!(field(&body, "ticker"), Some("Camille: on my way"));
        assert_eq!(field(&body, "title"), Some("Camille"));
        assert_eq!(field(&body, "text"), Some("on my way"));
    }

    #[tokio::test]
    async fn test_ticker_is_summary_alone_when_body_empty() {
        let body = build_notification_body(42, "Signal", "Camille", "", 5000);
        assert_eq!(field(&body, "ticker"), Some("Camille"));
    }

    /// Upstream only sets `text` when the body is non-empty
    /// (dbusnotificationslistener.cpp:327-329).
    #[tokio::test]
    async fn test_text_omitted_when_body_empty() {
        let body = build_notification_body(42, "Signal", "Camille", "", 5000);
        assert!(body.get("text").is_none());
    }

    /// isClearable is `timeout == -1` (dbusnotificationslistener.cpp:322):
    /// a notification that never expires on its own is the dismissible one.
    #[tokio::test]
    async fn test_is_clearable_true_only_for_never_expiring() {
        let never = build_notification_body(1, "app", "s", "b", -1);
        assert_eq!(
            never.get("isClearable").and_then(|v| v.as_bool()),
            Some(true)
        );

        let timed = build_notification_body(1, "app", "s", "b", 5000);
        assert_eq!(
            timed.get("isClearable").and_then(|v| v.as_bool()),
            Some(false)
        );

        let server_default = build_notification_body(1, "app", "s", "b", 0);
        assert_eq!(
            server_default.get("isClearable").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    /// `id` goes out as a STRING (dbusnotificationslistener.cpp:319 wraps it
    /// in QString::number); Android parses it back with toInt
    /// (ReceiveNotificationsPlugin.kt:91-92).
    #[tokio::test]
    async fn test_id_is_a_string() {
        let body = build_notification_body(4242, "app", "s", "b", -1);
        assert_eq!(field(&body, "id"), Some("4242"));
    }

    #[tokio::test]
    async fn test_app_name_and_silent_flag() {
        let body = build_notification_body(1, "Thunderbird", "s", "b", -1);
        assert_eq!(field(&body, "appName"), Some("Thunderbird"));
        assert_eq!(body.get("silent").and_then(|v| v.as_bool()), Some(false));
    }
}

#[cfg(test)]
mod notification_request_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::protocol::crypto::CertificateManager;
    use crate::protocol::pairing::PairingHandler;

    fn make_plugin() -> (SendNotificationsPlugin, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
        cert_manager.init().expect("cert init");
        let plugin = SendNotificationsPlugin::new(
            Arc::new(PluginEventBroadcaster::new(16, "plugin")),
            Arc::new(PairingHandler::new(cert_manager)),
        );
        (plugin, temp_dir)
    }

    /// Task 1.7: is_backend_available must reflect watcher_started, not
    /// the Plugin trait's default `true` — this plugin has no injectable
    /// backend mock (unlike clipboard/mpris/systemvolume/pausemusic/
    /// screensaver_inhibit), so the field is driven directly rather than
    /// starting the real watcher (which touches a live D-Bus connection
    /// this test must not depend on).
    #[test]
    fn test_is_backend_available_reflects_watcher_started() {
        let (plugin, _temp) = make_plugin();
        assert!(!plugin.is_backend_available());

        plugin.watcher_started.store(true, Ordering::SeqCst);
        assert!(plugin.is_backend_available());
    }

    /// `cancel` carries a notification id as a STRING. kdeconnect-kde writes it
    /// with `np.set<QString>(QStringLiteral("cancel"), internalId)`
    /// (plugins/notifications/notificationsplugin.cpp:143) and kdeconnect-android
    /// reads it with `np.getString("cancel")`
    /// (.../plugins/notifications/NotificationsPlugin.kt:529). The id below is
    /// the upstream-derived fixture literal at
    /// tests/fixtures/upstream-wire/sendnotifications/cancel_string.json.
    #[test]
    fn test_cancel_parses_as_string_notification_id() {
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/sendnotifications/cancel_string.json"),
            )
            .expect("sendnotifications/cancel_string.json"),
        )
        .expect("sendnotifications/cancel_string.json parses");
        let packet = Packet::new("kdeconnect.notification.request".to_string(), body);
        let req: NotificationRequest = packet.body_as("notification request").unwrap();
        assert_eq!(
            req.cancel.as_deref(),
            Some("0|com.sec.android.daemonapp|5|null|10203")
        );
        assert_eq!(req.request, None);
    }

    /// The resend-everything flag. kdeconnect-kde notificationsplugin.cpp:29 and
    /// ReceiveNotificationsPlugin.kt:39-41 both send exactly this body. Fixture
    /// at tests/fixtures/upstream-wire/sendnotifications/request_flag.json.
    #[test]
    fn test_request_flag_parses_with_no_cancel() {
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/sendnotifications/request_flag.json"),
            )
            .expect("sendnotifications/request_flag.json"),
        )
        .expect("sendnotifications/request_flag.json parses");
        let packet = Packet::new("kdeconnect.notification.request".to_string(), body);
        let req: NotificationRequest = packet.body_as("notification request").unwrap();
        assert_eq!(req.request, Some(true));
        assert_eq!(req.cancel, None);
    }

    /// rust-connect used to put `"cancel": false` on its own on-connect packet.
    /// A peer running that build is still out there, so a bool under this key
    /// must be ignored rather than failing the whole packet parse.
    #[test]
    fn test_legacy_bool_cancel_is_ignored_not_a_parse_error() {
        let packet = Packet::new(
            "kdeconnect.notification.request".to_string(),
            serde_json::json!({ "request": true, "cancel": false }),
        );
        let req: NotificationRequest = packet
            .body_as("notification request")
            .expect("a bool cancel must not fail the packet");
        assert_eq!(req.cancel, None, "a bool is not a notification id");
        assert_eq!(req.request, Some(true));
    }

    /// An empty string is not a notification id either. Forwarding it anywhere
    /// would be worse than dropping it.
    #[test]
    fn test_empty_cancel_is_not_an_id() {
        let packet = Packet::new(
            "kdeconnect.notification.request".to_string(),
            serde_json::json!({ "cancel": "" }),
        );
        let req: NotificationRequest = packet.body_as("notification request").unwrap();
        assert_eq!(req.cancel, None);
    }

    /// This plugin is the sole receiver of the packet type after the ownership
    /// split — kdeconnect-kde plugins/sendnotifications/kdeconnect_sendnotifications.json
    /// `X-KdeConnect-SupportedPacketType: ["kdeconnect.notification.request"]`.
    #[tokio::test]
    async fn test_owns_the_request_capability() {
        let (plugin, _temp) = make_plugin();
        assert_eq!(
            plugin.incoming_capabilities(),
            vec!["kdeconnect.notification.request".to_string()]
        );
    }

    /// A real cancel is accepted and handled without error. We log it and stop;
    /// see the decision note in `handle_packet` for why nothing is closed.
    #[tokio::test]
    async fn test_handle_cancel_packet_is_accepted() {
        let (plugin, _temp) = make_plugin();
        let packet = Packet::new(
            "kdeconnect.notification.request".to_string(),
            serde_json::json!({ "cancel": "0|org.thoughtcrime.securesms|42|null|10123" }),
        );
        assert!(matches!(
            plugin
                .handle_packet("devabcdef0123456789abcdef01234567", packet)
                .await,
            Ok(None)
        ));
    }
}

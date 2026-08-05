//! Pausemusic plugin
//!
//! Single Responsibility: Pause local media playback while the paired phone
//! is in a call, and resume it after. Listens to `kdeconnect.telephony`
//! packets (fan-out alongside the telephony plugin) — upstream has no
//! dedicated pausemusic packet type (kdeconnect-kde
//! plugins/pausemusic/kdeconnect_pausemusic.json: X-KdeConnect-SupportedPacketType
//! is kdeconnect.telephony).
//!
//! Wire semantics (upstream-verified):
//! - The phone sends `{"event": "ringing"}` on an incoming call and
//!   `{"event": "talking"}` when it is answered (kdeconnect-android
//!   plugins/telephony/TelephonyPlugin.kt:105,109). When the call ends it
//!   RESENDS the last event with `"isCancel": "true"` — as a JSON STRING
//!   (TelephonyPlugin.kt:114-115), which GSConnect reads as truthy
//!   (src/service/plugins/telephony.js:147) and kdeconnect-kde's
//!   QVariant conversion also accepts. We accept bool true and
//!   string "true" (case-insensitive); anything else is not a cancel.
//! - Pause trigger: ringing OR talking, not cancelled — kdeconnect-kde's
//!   default (conditionTalking=false pauses as soon as it rings,
//!   pausemusicplugin.cpp:29-38). Other events (missedCall, …) are ignored.
//! - Pause action: iterate org.mpris.MediaPlayer2.* on the session bus,
//!   SKIP kdeconnect's own remote-control interfaces
//!   (org.mpris.MediaPlayer2.kdeconnect.*, pausemusicplugin.cpp:62-64),
//!   and for every player whose PlaybackStatus is "Playing": Pause() when
//!   CanPause, else Stop() (pausemusicplugin.cpp:66-77).
//! - Resume action (cancel, upstream actionResume default true): Play()
//!   exactly the interfaces WE paused, then forget them
//!   (pausemusicplugin.cpp:97-105). Players the user paused themselves are
//!   never resumed.
//!
//! Fixed upstream DEFAULTS, no config surface: pause-on-ring
//! (conditionTalking=false), actionPause=true, actionResume=true.
//! NOT implemented: actionMute (upstream default false) — muting PulseAudio
//!   sinks has no infra in this codebase (systemvolume is a state-tracking
//!   shell) and a default-off knob doesn't justify shelling out to pactl.
//! NOT resumed on disconnect: upstream's per-device plugin instance is
//! destroyed on disconnect, losing pausedSources — players stay paused.
//! Our on_disconnected clears the list without resuming to match.
//!
//! KNOWN UPSTREAM LIMITATION (panel R1-R5, disposed per round): with TWO
//! paired phones in simultaneous calls, the first cancel resumes media
//! while the second call is still active — device B's pause saw nothing
//! Playing (A already paused it), so B holds no claim. kdeconnect-kde's
//! per-device plugin instances behave IDENTICALLY (A's plugin paused,
//! A's cancel resumes), so this matches the reference implementation; a
//! cross-device active-call refcount would be a deliberate deviation,
//! available if ever wanted (single-phone deployment today).

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use tracing::{debug, info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::mpris::{is_ignored_service, MPRIS_SERVICE_PREFIX};
use super::plugin::Plugin;

/// The media-control seam. The real impl talks MPRIS over the session bus;
/// tests drive a mock. Mirrors MprisPlugin's backend split: `connect()` is
/// the fallible step, enabled only from the production entry point.
#[async_trait::async_trait]
pub(crate) trait MediaPauseBackend: Send + Sync {
    /// Pause (or Stop when unpausable) every Playing MPRIS player, skipping
    /// kdeconnect's own interfaces. Returns the bus names acted on, for a
    /// later resume.
    async fn pause_playing(&self) -> Vec<String>;
    /// Resume (Play) exactly these bus names, best-effort.
    async fn resume(&self, services: &[String]);
}

/// `isCancel` on the wire: Android sends the STRING "true"
/// (TelephonyPlugin.kt:115), a v8+ peer may send a real bool.
fn is_cancel(body: &serde_json::Value) -> bool {
    match body.get("isCancel") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

pub struct PausemusicPlugin {
    backend: StdRwLock<Option<Arc<dyn MediaPauseBackend>>>,
    /// device_id → bus names we paused for that device's call. Per-device
    /// because upstream's plugin instance is per-device: one phone's cancel
    /// must not resume (or forget) what another phone's call paused.
    paused: StdRwLock<HashMap<String, Vec<String>>>,
    /// Serializes the pause and cancel critical sections. Without it a
    /// cancel handled while `pause_playing` is still awaiting sees an empty
    /// list and no-ops, after which the pause records anyway — players
    /// stuck paused with no resume ever coming (cubic, PR #7). Phone call
    /// events are rare, so one lock for all devices costs nothing.
    call_lock: tokio::sync::Mutex<()>,
}

impl Default for PausemusicPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl PausemusicPlugin {
    pub fn new() -> Self {
        Self {
            backend: StdRwLock::new(None),
            paused: StdRwLock::new(HashMap::new()),
            call_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Connect the real session-bus backend. Called ONLY from the
    /// production entry point (bootstrap.rs create_state) — tests inject a
    /// mock with `with_backend`. Degrades with a log event when no session
    /// bus is reachable, mousepad/clipboard-style.
    pub async fn enable_session_backend(&self) {
        match ZbusPauseBackend::connect().await {
            Ok(backend) => {
                info!(
                    event = "pausemusic_backend_ready",
                    "Session MPRIS pause backend enabled"
                );
                self.set_backend(Arc::new(backend));
            }
            Err(e) => {
                warn!(
                    error = %e,
                    event = "pausemusic_backend_unavailable",
                    "No session D-Bus for pausemusic. Call pause/resume degraded to a no-op."
                );
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_backend(self, backend: Arc<dyn MediaPauseBackend>) -> Self {
        self.set_backend(backend);
        self
    }

    fn set_backend(&self, backend: Arc<dyn MediaPauseBackend>) {
        // Poison-tolerant, same pattern as MprisPlugin::set_backend.
        *self.backend.write().unwrap_or_else(|e| e.into_inner()) = Some(backend);
    }

    fn backend(&self) -> Option<Arc<dyn MediaPauseBackend>> {
        self.backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Bus names currently recorded as paused-by-us for a device.
    #[cfg(test)]
    pub(crate) fn paused_for(&self, device_id: &str) -> Vec<String> {
        self.paused
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Plugin for PausemusicPlugin {
    fn name(&self) -> &str {
        "pausemusic"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        // kdeconnect-kde kdeconnect_pausemusic.json
        // X-KdeConnect-SupportedPacketType — shared with the telephony
        // plugin (the router fans out to both).
        vec!["kdeconnect.telephony".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // kdeconnect_pausemusic.json X-KdeConnect-OutgoingPacketType: [].
        vec![]
    }

    fn on_disconnected(&self, device_id: &str) {
        // Upstream loses pausedSources when the per-device plugin is
        // destroyed — no resume, players stay paused. Match that.
        // Poison-tolerant like every other lock site in this file.
        self.paused
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let body = &packet.body;
        let event = body.get("event").and_then(|v| v.as_str()).unwrap_or("");

        // Pause trigger is ringing/talking ONLY (pausemusicplugin.cpp:29-38);
        // a cancel RESENDS one of those events, so the filter runs first and
        // never drops a real cancel.
        if event != "ringing" && event != "talking" {
            debug!(
                device_id = %device_id,
                event_type = %event,
                event = "pausemusic_event_ignored",
                "Telephony event is not a call trigger, ignoring"
            );
            return Ok(None);
        }

        if is_cancel(body) {
            // Call ended: resume exactly what we paused for this device
            // (pausemusicplugin.cpp:97-105 — autoResume default true).
            // Serialized against the pause branch: a cancel that arrives
            // mid-pause waits for the record, then resumes it.
            let _guard = self.call_lock.lock().await;
            let services = self
                .paused
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(device_id)
                .unwrap_or_default();
            if services.is_empty() {
                debug!(
                    device_id = %device_id,
                    event = "pausemusic_resume_noop",
                    "Call ended but nothing was paused by us"
                );
                return Ok(None);
            }
            match self.backend() {
                Some(backend) => {
                    info!(
                        device_id = %device_id,
                        players = ?services,
                        event = "pausemusic_resume",
                        "Call ended, resuming players we paused"
                    );
                    backend.resume(&services).await;
                }
                None => {
                    debug!(
                        device_id = %device_id,
                        event = "pausemusic_no_backend",
                        "Call ended but no session backend; players stay paused"
                    );
                }
            }
            return Ok(None);
        }

        // Call started: pause everything Playing, remember what we acted on.
        // Serialized against the cancel branch (see call_lock).
        let _guard = self.call_lock.lock().await;
        match self.backend() {
            Some(backend) => {
                let acted = backend.pause_playing().await;
                if acted.is_empty() {
                    debug!(
                        device_id = %device_id,
                        event_type = %event,
                        event = "pausemusic_nothing_playing",
                        "Call started but no MPRIS player is Playing"
                    );
                } else {
                    info!(
                        device_id = %device_id,
                        event_type = %event,
                        players = ?acted,
                        event = "pausemusic_paused",
                        "Call started, paused Playing MPRIS players"
                    );
                    let mut paused = self.paused.write().unwrap_or_else(|e| e.into_inner());
                    let entry = paused.entry(device_id.to_string()).or_default();
                    for service in acted {
                        if !entry.contains(&service) {
                            entry.push(service);
                        }
                    }
                }
            }
            None => {
                debug!(
                    device_id = %device_id,
                    event_type = %event,
                    event = "pausemusic_no_backend",
                    "Call started but no session backend, nothing paused"
                );
            }
        }

        Ok(None)
    }
}

/// zbus session-bus backend. Enumerates org.mpris.MediaPlayer2.* itself
/// (org.freedesktop.DBus ListNames) instead of sharing the mpris plugin's
/// tracked player set: upstream's pausemusic iterates the bus registry on
/// each event too (pausemusicplugin.cpp:56-59), and the two plugins are
/// independent there as well.
pub(crate) struct ZbusPauseBackend {
    conn: zbus::Connection,
}

impl ZbusPauseBackend {
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::session().await.map_err(|e| {
            crate::utils::errors::Error::Internal(format!("cannot connect to session D-Bus: {e}"))
        })?;
        Ok(Self { conn })
    }
}

#[async_trait::async_trait]
impl MediaPauseBackend for ZbusPauseBackend {
    async fn pause_playing(&self) -> Vec<String> {
        use super::mpris::zbus_backend::MediaPlayer2PlayerProxy;

        let dbus = match zbus::fdo::DBusProxy::new(&self.conn).await {
            Ok(proxy) => proxy,
            Err(e) => {
                warn!(error = %e, event = "pausemusic_dbus_proxy_failed", "Cannot build D-Bus proxy");
                return Vec::new();
            }
        };
        let names = match dbus.list_names().await {
            Ok(names) => names,
            Err(e) => {
                warn!(error = %e, event = "pausemusic_list_names_failed", "Cannot list bus names for MPRIS pause");
                return Vec::new();
            }
        };

        let mut acted = Vec::new();
        for name in names {
            let service = name.as_str();
            if !service.starts_with(MPRIS_SERVICE_PREFIX) || is_ignored_service(service) {
                continue;
            }
            let Ok(proxy) = MediaPlayer2PlayerProxy::new(&self.conn, service).await else {
                continue;
            };
            let Ok(status) = proxy.playback_status().await else {
                continue;
            };
            if status != "Playing" {
                continue;
            }
            // pausemusicplugin.cpp:71-76: Pause when possible, Stop
            // otherwise. A FAILED CanPause query is neither — skip the
            // player rather than guess at the more disruptive Stop
            // (Sourcery, PR #6 review).
            let can_pause = match proxy.can_pause().await {
                Ok(can_pause) => can_pause,
                Err(e) => {
                    debug!(
                        error = %e,
                        service = %service,
                        event = "pausemusic_can_pause_query_failed",
                        "CanPause query failed, skipping player"
                    );
                    continue;
                }
            };
            let ok = if can_pause {
                proxy.pause().await.is_ok()
            } else {
                proxy.stop().await.is_ok()
            };
            if ok {
                acted.push(service.to_string());
            }
        }
        acted
    }

    async fn resume(&self, services: &[String]) {
        use super::mpris::zbus_backend::MediaPlayer2PlayerProxy;

        for service in services {
            // pausemusicplugin.cpp:100-103: Play() unconditionally, and the
            // player may already be gone — best-effort.
            if let Ok(proxy) = MediaPlayer2PlayerProxy::new(&self.conn, service.as_str()).await {
                let _ = proxy.play().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    struct MockBackend {
        playing: Vec<String>,
        pause_calls: StdRwLock<usize>,
        resumed: StdRwLock<Vec<Vec<String>>>,
    }

    impl MockBackend {
        fn new(playing: &[&str]) -> Self {
            Self {
                playing: playing.iter().map(|s| s.to_string()).collect(),
                pause_calls: StdRwLock::new(0),
                resumed: StdRwLock::new(Vec::new()),
            }
        }

        fn pause_call_count(&self) -> usize {
            *self.pause_calls.read().unwrap()
        }

        fn resumed(&self) -> Vec<Vec<String>> {
            self.resumed.read().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl MediaPauseBackend for MockBackend {
        async fn pause_playing(&self) -> Vec<String> {
            *self.pause_calls.write().unwrap() += 1;
            self.playing.clone()
        }

        async fn resume(&self, services: &[String]) {
            self.resumed.write().unwrap().push(services.to_vec());
        }
    }

    fn telephony_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.telephony".to_string(), body)
    }

    #[tokio::test]
    async fn test_pausemusic_plugin_name_and_capabilities() {
        let plugin = PausemusicPlugin::new();
        assert_eq!(plugin.name(), "pausemusic");
        assert_eq!(
            plugin.incoming_capabilities(),
            vec!["kdeconnect.telephony".to_string()]
        );
        assert!(plugin.outgoing_capabilities().is_empty());
    }

    #[tokio::test]
    async fn test_ringing_pauses_playing_players() {
        let backend = Arc::new(MockBackend::new(&[
            "org.mpris.MediaPlayer2.brave",
            "org.mpris.MediaPlayer2.spotify",
        ]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
            .unwrap();
        assert_eq!(backend.pause_call_count(), 1);
        assert_eq!(
            plugin.paused_for("device1"),
            vec![
                "org.mpris.MediaPlayer2.brave".to_string(),
                "org.mpris.MediaPlayer2.spotify".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_talking_also_pauses_and_accumulates() {
        // Upstream pauses on ringing AND talking (conditionTalking=false
        // default); a player that starts between the two events gets caught
        // by the second pause and merged into the same resume list.
        let backend = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "talking" })),
            )
            .await
            .unwrap();
        assert_eq!(backend.pause_call_count(), 2);
        // Same player twice: deduped in the resume list.
        assert_eq!(
            plugin.paused_for("device1"),
            vec!["org.mpris.MediaPlayer2.brave".to_string()]
        );
    }

    #[tokio::test]
    async fn test_non_call_events_ignored() {
        let backend = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        for event in ["missedCall", "sms", ""] {
            plugin
                .handle_packet(
                    "device1",
                    telephony_packet(serde_json::json!({ "event": event })),
                )
                .await
                .unwrap();
        }
        assert_eq!(backend.pause_call_count(), 0);
        assert!(plugin.paused_for("device1").is_empty());
    }

    #[tokio::test]
    async fn test_cancel_bool_resumes_only_what_we_paused() {
        let backend = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing", "isCancel": true })),
            )
            .await
            .unwrap();
        assert_eq!(
            backend.resumed(),
            vec![vec!["org.mpris.MediaPlayer2.brave".to_string()]]
        );
        // Resume list is consumed: a second cancel is a no-op.
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing", "isCancel": true })),
            )
            .await
            .unwrap();
        assert_eq!(backend.resumed().len(), 1);
    }

    #[tokio::test]
    async fn test_cancel_string_true_resumes_exact_android_wire_shape() {
        // EXACT body the phone sends when a call ends (TelephonyPlugin.kt:
        // 114-115): the LAST event resent with isCancel as a JSON STRING.
        let backend = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "talking" })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "talking", "isCancel": "true" })),
            )
            .await
            .unwrap();
        assert_eq!(backend.resumed().len(), 1);
    }

    #[tokio::test]
    async fn test_is_cancel_parsing() {
        assert!(is_cancel(&serde_json::json!({ "isCancel": true })));
        assert!(is_cancel(&serde_json::json!({ "isCancel": "true" })));
        assert!(is_cancel(&serde_json::json!({ "isCancel": "TRUE" })));
        assert!(!is_cancel(&serde_json::json!({ "isCancel": false })));
        assert!(!is_cancel(&serde_json::json!({ "isCancel": "false" })));
        assert!(!is_cancel(&serde_json::json!({ "isCancel": 1 })));
        assert!(!is_cancel(&serde_json::json!({})));
    }

    #[tokio::test]
    async fn test_cancel_without_prior_pause_is_noop() {
        let backend = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing", "isCancel": true })),
            )
            .await
            .unwrap();
        assert!(backend.resumed().is_empty());
    }

    #[tokio::test]
    async fn test_cancel_is_per_device() {
        // Upstream plugin instances are per-device: device2's call ending
        // must not resume or forget what device1's call paused.
        let backend = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device2",
                telephony_packet(serde_json::json!({ "event": "ringing", "isCancel": true })),
            )
            .await
            .unwrap();
        assert!(backend.resumed().is_empty());
        assert_eq!(
            plugin.paused_for("device1"),
            vec!["org.mpris.MediaPlayer2.brave".to_string()]
        );
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_without_resuming() {
        // Upstream loses pausedSources on plugin teardown — no resume.
        let backend = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let plugin = PausemusicPlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
            .unwrap();
        plugin.on_disconnected("device1");
        assert!(plugin.paused_for("device1").is_empty());
        assert!(backend.resumed().is_empty());
    }

    #[tokio::test]
    async fn test_no_backend_degrades_cleanly() {
        let plugin = PausemusicPlugin::new();
        for body in [
            serde_json::json!({ "event": "ringing" }),
            serde_json::json!({ "event": "ringing", "isCancel": true }),
        ] {
            assert!(plugin
                .handle_packet("device1", telephony_packet(body))
                .await
                .is_ok());
        }
    }

    /// Blocks inside pause_playing until released; counts entries.
    struct SlowMockBackend {
        started: StdRwLock<usize>,
        gate: tokio::sync::Notify,
        playing: Vec<String>,
        resumed: StdRwLock<Vec<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl MediaPauseBackend for SlowMockBackend {
        async fn pause_playing(&self) -> Vec<String> {
            *self.started.write().unwrap() += 1;
            self.gate.notified().await;
            self.playing.clone()
        }

        async fn resume(&self, services: &[String]) {
            self.resumed.write().unwrap().push(services.to_vec());
        }
    }

    #[tokio::test]
    async fn test_cancel_during_pause_waits_then_resumes() {
        // Red before the call_lock: a cancel handled while pause_playing
        // was still awaiting saw an empty list and no-oped, then the pause
        // recorded — players stuck paused with no resume ever coming.
        let backend = Arc::new(SlowMockBackend {
            started: StdRwLock::new(0),
            gate: tokio::sync::Notify::new(),
            playing: vec!["org.mpris.MediaPlayer2.brave".to_string()],
            resumed: StdRwLock::new(Vec::new()),
        });
        let plugin = Arc::new(PausemusicPlugin::new().with_backend(backend.clone()));

        let p1 = plugin.clone();
        let pause_task = tokio::spawn(async move {
            p1.handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
        });
        // Wait until the pause is inside the (blocked) backend call.
        for _ in 0..80 {
            if *backend.started.read().unwrap() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(*backend.started.read().unwrap(), 1);

        // The cancel must NOT complete while the pause is in flight.
        let p2 = plugin.clone();
        let cancel_task = tokio::spawn(async move {
            p2.handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing", "isCancel": true })),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            backend.resumed.read().unwrap().is_empty(),
            "cancel ran during the in-flight pause and no-oped"
        );

        // Release the pause; the cancel then resumes exactly what was paused.
        backend.gate.notify_one();
        pause_task.await.unwrap().unwrap();
        cancel_task.await.unwrap().unwrap();
        assert_eq!(
            backend.resumed.read().unwrap().clone(),
            vec![vec!["org.mpris.MediaPlayer2.brave".to_string()]]
        );
        assert!(plugin.paused_for("device1").is_empty());
    }
}

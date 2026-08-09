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
//! (conditionTalking=false), actionPause=true, actionResume=true,
//! actionMute=false (`ACTION_MUTE` below — Task 1.6 Backend B, vk #1010).
//! actionMute now HAS a real backend (src/plugins/systemvolume/backend.rs's
//! `VolumeBackend`, the same pactl-based provider the systemvolume plugin
//! uses): `mute_for`/`unmute_for` mute every currently-unmuted sink on call
//! start and restore exactly the ones we muted on cancel, mirroring
//! pausemusicplugin.cpp:48-57 (mute) and :85-97 (unmute + unconditional
//! bookkeeping clear). The mechanism is unconditional and fully tested;
//! only the `handle_packet` call sites are gated by `ACTION_MUTE`, which
//! stays hardcoded to upstream's own shipped default (off) because this
//! codebase has no per-plugin config surface to let a user turn it on —
//! adding one here, with nothing yet able to read it, is exactly the
//! Task-1.7-class dead-knob the plan warns against. Flipping the constant
//! (or wiring a real config surface, Task 1.7) is the entire activation
//! path.
//! NOT resumed on disconnect: upstream's per-device plugin instance is
//! destroyed on disconnect, losing pausedSources — players stay paused.
//! Our on_disconnected clears the list without resuming to match.
//!
//! KNOWN UPSTREAM LIMITATION (reviewed, deliberately kept): with TWO
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
use super::systemvolume::backend::{self, VolumeBackend};

/// Whether the mute action fires at all. Fixed to upstream's own shipped
/// default — see the module doc's "Fixed upstream DEFAULTS" paragraph for
/// why this is a hardcoded constant rather than a config field.
const ACTION_MUTE: bool = false;

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
    /// System-volume backend for the mute action (Task 1.6 Backend B). A
    /// separate connection from `backend`'s MPRIS one, same "each plugin
    /// owns its own backend" convention `ZbusPauseBackend`'s own doc
    /// describes (mirrors the mpris plugin's independent connection too).
    volume_backend: StdRwLock<Option<Arc<dyn VolumeBackend>>>,
    /// device_id → sink names we muted for that device's call. Mirrors
    /// `paused`'s per-device shape and reasoning exactly.
    muted: StdRwLock<HashMap<String, Vec<String>>>,
    /// Serializes the pause and cancel critical sections. Without it a
    /// cancel handled while `pause_playing` is still awaiting sees an empty
    /// list and no-ops, after which the pause records anyway — players
    /// stuck paused with no resume ever coming. Phone call
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
            volume_backend: StdRwLock::new(None),
            muted: StdRwLock::new(HashMap::new()),
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

        // System-volume mute backend (Task 1.6 Backend B). Reuses the
        // systemvolume plugin's own pactl detection rather than
        // duplicating it — the detection logic (pactl on PATH + answers
        // get-default-sink) has one home; the connection itself stays
        // separate per plugin, same as the MPRIS backend above.
        //
        // Gated on ACTION_MUTE (PR #11 review): detect() runs pactl
        // synchronously, and while the mute action is hardcoded off the
        // backend is unreachable from handle_packet — probing anyway
        // spends a subprocess at every enable and can log a misleading
        // "mute degraded" warning for an action that is disabled, not
        // degraded. Tests bypass this gate via set_volume_backend
        // directly, so the mechanism stays fully covered.
        if ACTION_MUTE {
            match backend::detect() {
                Some(volume_backend) => {
                    info!(
                        event = "pausemusic_volume_backend_ready",
                        "System-volume mute backend enabled"
                    );
                    self.set_volume_backend(Arc::new(volume_backend));
                }
                None => {
                    warn!(
                        event = "pausemusic_volume_backend_unavailable",
                        "No pactl backend for pausemusic mute. Mute action degraded to a no-op."
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_backend(self, backend: Arc<dyn MediaPauseBackend>) -> Self {
        self.set_backend(backend);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_volume_backend(self, backend: Arc<dyn VolumeBackend>) -> Self {
        self.set_volume_backend(backend);
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

    fn set_volume_backend(&self, backend: Arc<dyn VolumeBackend>) {
        *self
            .volume_backend
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(backend);
    }

    fn volume_backend(&self) -> Option<Arc<dyn VolumeBackend>> {
        self.volume_backend
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

    /// Sink names currently recorded as muted-by-us for a device.
    #[cfg(test)]
    pub(crate) fn muted_for(&self, device_id: &str) -> Vec<String> {
        self.muted
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Mutes every currently-unmuted sink and records their names for
    /// `unmute_for` — mirrors pausemusicplugin.cpp:48-57 (`if
    /// (!sink->isMuted()) { sink->setMuted(true); mutedSinks.insert(...);
    /// }`). Best-effort like the pause path: a missing backend or a
    /// `list_sinks`/`set_muted` failure logs and degrades, never errors
    /// the packet handler.
    ///
    /// Unconditional on purpose — NOT gated by `ACTION_MUTE` itself, so
    /// tests can exercise the mechanism directly regardless of the
    /// production default. `handle_packet`'s call sites are the only
    /// `ACTION_MUTE` gate.
    async fn mute_for(&self, device_id: &str) {
        let Some(backend) = self.volume_backend() else {
            debug!(
                device_id = %device_id,
                event = "pausemusic_no_volume_backend",
                "Call started but no volume backend; nothing muted"
            );
            return;
        };
        let sinks = match backend.list_sinks().await {
            Ok(sinks) => sinks,
            Err(e) => {
                warn!(
                    device_id = %device_id,
                    error = %e,
                    event = "pausemusic_list_sinks_failed",
                    "Cannot list sinks for mute"
                );
                return;
            }
        };

        let mut muted_names = Vec::new();
        for sink in sinks {
            if sink.muted == Some(true) {
                continue; // already muted — pausemusicplugin.cpp:52
            }
            if backend.set_muted(&sink.name, true).await.is_ok() {
                muted_names.push(sink.name);
            }
        }

        if muted_names.is_empty() {
            return;
        }
        info!(
            device_id = %device_id,
            sinks = ?muted_names,
            event = "pausemusic_muted",
            "Call started, muted system audio sinks"
        );
        let mut muted = self.muted.write().unwrap_or_else(|e| e.into_inner());
        let entry = muted.entry(device_id.to_string()).or_default();
        for name in muted_names {
            if !entry.contains(&name) {
                entry.push(name);
            }
        }
    }

    /// Unmutes exactly the sinks WE muted for this device's call, then
    /// forgets them regardless of whether the backend calls succeeded —
    /// mirrors pausemusicplugin.cpp:85-97: the unmute happens (autoResume
    /// is hardcoded true here, same as the pause/resume path above having
    /// no separate toggle), and `mutedSinks.clear()` runs unconditionally
    /// either way. A second call with nothing recorded is a no-op — no
    /// double-restore.
    async fn unmute_for(&self, device_id: &str) {
        let sinks = self
            .muted
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id)
            .unwrap_or_default();
        if sinks.is_empty() {
            return;
        }
        match self.volume_backend() {
            Some(backend) => {
                info!(
                    device_id = %device_id,
                    sinks = ?sinks,
                    event = "pausemusic_unmuted",
                    "Call ended, unmuting sinks we muted"
                );
                for name in &sinks {
                    let _ = backend.set_muted(name, false).await;
                }
            }
            None => {
                debug!(
                    device_id = %device_id,
                    event = "pausemusic_no_volume_backend",
                    "Call ended but no volume backend; sinks stay muted"
                );
            }
        }
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

            // Mute and pause are independent legs, matching upstream's two
            // separate conditionals inside the same branch
            // (pausemusicplugin.cpp:85-97 vs :99-107) — unmute must run
            // even when nothing was paused.
            if ACTION_MUTE {
                self.unmute_for(device_id).await;
            }

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

        // Mute is an independent leg from pause (pausemusicplugin.cpp:47-57
        // vs :59-82, both inside the same pauseConditionFulfilled branch).
        if ACTION_MUTE {
            self.mute_for(device_id).await;
        }

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
            // player rather than guess at the more disruptive Stop.
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
    use crate::plugins::systemvolume::backend::{LocalSinkState, MockBackend as VolumeMockBackend};

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

    /// Fixture: tests/fixtures/upstream-wire/pausemusic/telephony_talking_cancel_string.json
    ///   EXACT body the phone sends when a call ends (TelephonyPlugin.kt:
    ///   114-115): the LAST event resent with isCancel as a JSON STRING.
    #[tokio::test]
    async fn test_cancel_string_true_resumes_exact_android_wire_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/pausemusic/telephony_talking_cancel_string.json");
        let cancel_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read pausemusic cancel fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

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
            .handle_packet("device1", telephony_packet(cancel_body))
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

    // -----------------------------------------------------------------
    // Mute state machine (Task 1.6 Backend B, vk #1010). `mute_for` /
    // `unmute_for` are unconditional — not gated by ACTION_MUTE
    // themselves, only handle_packet's call sites are — so these drive
    // the mechanism directly, matching how `is_cancel` above is tested
    // as a pure function rather than only through handle_packet.
    // -----------------------------------------------------------------

    fn sink(name: &str, muted: bool) -> LocalSinkState {
        LocalSinkState {
            name: name.to_string(),
            muted: Some(muted),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_mute_for_mutes_unmuted_sinks_and_skips_already_muted() {
        // pausemusicplugin.cpp:50-56: iterate every sink, mute + record
        // only the ones that were NOT already muted.
        let volume = VolumeMockBackend::new();
        volume.with_sinks(vec![sink("speakers", false), sink("headset", true)]);
        let plugin = PausemusicPlugin::new().with_volume_backend(volume.clone());

        plugin.mute_for("device1").await;

        assert_eq!(plugin.muted_for("device1"), vec!["speakers".to_string()]);
        assert!(
            volume.calls().iter().any(|c| c.contains("speakers")),
            "the unmuted sink must have been acted on: {:?}",
            volume.calls()
        );
        assert!(
            !volume.calls().iter().any(|c| c.contains("headset")),
            "the already-muted sink must not be touched: {:?}",
            volume.calls()
        );
    }

    #[tokio::test]
    async fn test_unmute_for_restores_exactly_what_we_muted_then_forgets() {
        let volume = VolumeMockBackend::new();
        volume.with_sinks(vec![sink("speakers", false)]);
        let plugin = PausemusicPlugin::new().with_volume_backend(volume.clone());

        plugin.mute_for("device1").await;
        assert_eq!(plugin.muted_for("device1"), vec!["speakers".to_string()]);

        plugin.unmute_for("device1").await;

        assert!(
            plugin.muted_for("device1").is_empty(),
            "restored sinks must be forgotten"
        );
        let sinks = volume.list_sinks().await.unwrap();
        let speakers = sinks.iter().find(|s| s.name == "speakers").unwrap();
        assert_eq!(
            speakers.muted,
            Some(false),
            "the mock's recorded sink state must reflect the restore"
        );
    }

    #[tokio::test]
    async fn test_unmute_without_prior_mute_is_noop() {
        let volume = VolumeMockBackend::new();
        volume.with_sinks(vec![sink("speakers", false)]);
        let plugin = PausemusicPlugin::new().with_volume_backend(volume.clone());

        plugin.unmute_for("device1").await;

        assert!(plugin.muted_for("device1").is_empty());
        assert!(
            volume.calls().is_empty(),
            "no backend call when nothing was recorded as muted"
        );
    }

    #[tokio::test]
    async fn test_double_unmute_does_not_restore_twice() {
        // No double-restore: the second unmute_for for the same device
        // must be a pure no-op — the record was already consumed.
        let volume = VolumeMockBackend::new();
        volume.with_sinks(vec![sink("speakers", false)]);
        let plugin = PausemusicPlugin::new().with_volume_backend(volume.clone());

        plugin.mute_for("device1").await;
        plugin.unmute_for("device1").await;
        let calls_after_first_unmute = volume.calls().len();

        plugin.unmute_for("device1").await;

        assert_eq!(
            volume.calls().len(),
            calls_after_first_unmute,
            "a second unmute_for must not issue any further backend calls"
        );
        assert!(plugin.muted_for("device1").is_empty());
    }

    #[tokio::test]
    async fn test_mute_and_pause_interact_independently() {
        // Mute and pause are independent legs (pausemusicplugin.cpp:47-57
        // vs :59-82, both inside the SAME pauseConditionFulfilled branch):
        // driving both for the same call must populate and restore both
        // records without either disturbing the other.
        let media = Arc::new(MockBackend::new(&["org.mpris.MediaPlayer2.brave"]));
        let volume = VolumeMockBackend::new();
        volume.with_sinks(vec![sink("speakers", false)]);
        let plugin = PausemusicPlugin::new()
            .with_backend(media.clone())
            .with_volume_backend(volume.clone());

        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
            .unwrap();
        plugin.mute_for("device1").await;

        assert_eq!(
            plugin.paused_for("device1"),
            vec!["org.mpris.MediaPlayer2.brave".to_string()]
        );
        assert_eq!(plugin.muted_for("device1"), vec!["speakers".to_string()]);

        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing", "isCancel": true })),
            )
            .await
            .unwrap();
        plugin.unmute_for("device1").await;

        assert!(plugin.paused_for("device1").is_empty());
        assert!(plugin.muted_for("device1").is_empty());
        assert_eq!(
            media.resumed(),
            vec![vec!["org.mpris.MediaPlayer2.brave".to_string()]]
        );
    }

    #[tokio::test]
    async fn test_mute_disabled_by_default_matches_upstream() {
        // Pins ACTION_MUTE's real production value: with a working volume
        // backend attached, a real ringing event through handle_packet
        // must NOT touch it at all — upstream's own actionMute default is
        // false (pausemusicplugin.cpp:43), and this codebase has no
        // config surface to turn it on (see module doc).
        let volume = VolumeMockBackend::new();
        volume.with_sinks(vec![sink("speakers", false)]);
        let plugin = PausemusicPlugin::new().with_volume_backend(volume.clone());

        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing" })),
            )
            .await
            .unwrap();

        assert!(plugin.muted_for("device1").is_empty());
        assert!(
            volume.calls().is_empty(),
            "ACTION_MUTE=false must keep handle_packet from ever touching the volume backend"
        );
    }

    #[tokio::test]
    async fn test_no_volume_backend_degrades_cleanly() {
        let plugin = PausemusicPlugin::new();
        plugin.mute_for("device1").await;
        assert!(plugin.muted_for("device1").is_empty());
        plugin.unmute_for("device1").await; // no panic with nothing recorded
    }
}

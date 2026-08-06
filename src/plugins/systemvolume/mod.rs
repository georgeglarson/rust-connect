//! SystemVolume plugin
//!
//! Single Responsibility: both halves of the systemvolume wire:
//!
//! 1. **Controller side** (was the entire plugin before this commit):
//!    track a DESKTOP peer's audio sinks from `kdeconnect.systemvolume`
//!    packets and ask for the list on connect. This is upstream's
//!    "remotesystemvolume" role.
//!
//! 2. **Provider side** (new): expose THIS desktop's audio sinks from a
//!    PulseAudio/PipeWire session to a paired phone. The phone sends
//!    `kdeconnect.systemvolume.request` and we answer with
//!    `kdeconnect.systemvolume` containing a `sinkList`; live changes
//!    observed via `pactl subscribe` are pushed as per-sink deltas
//!    using the same shape upstream publishes
//!    (systemvolumeplugin-pulse.cpp:69-88).
//!
//! Wire shape, kdeconnect-kde plugins/systemvolume/systemvolumeplugin-pulse.cpp:
//! - Full state arrives as a `sinkList` ARRAY of sink objects (:90-104),
//!   which upstream consumers CLEAR and rebuild from
//!   (kdeconnect-android .../systemvolume/SystemVolumePlugin.kt:33-42).
//! - Deltas arrive as single-field packets keyed by `name`: `volume`
//!   (:71-72), `muted` (:78-79), `enabled` (:85-86). A delta naming a
//!   sink we have never seen is ignored, matching
//!   SystemVolumePlugin.kt:53-55.
//! - `volume` is an INTEGER on an absolute scale whose ceiling is the
//!   sink's `maxVolume` (PulseAudioQt::normalVolume() == 65536, :94;
//!   Sink.kt:27 reads it with getInt). NOT a 0.0-1.0 fraction.
//!
//! The phone's perspective (kdeconnect-android SystemVolumePlugin.kt):
//! - `kdeconnect.systemvolume` packets with `sinkList` rebuild the map
//!   (:33-42).
//! - `kdeconnect.systemvolume.request` packets carry `name` + optional
//!   `volume`/`muted`/`enabled` (the producer side: sendVolume/sendMute/
//!   sendEnable at :70-89).
//!
//! Capability honesty: the provider side depends on a backend
//! (PulseAudio/PipeWire). When no backend is wired, the plugin's
//! `incoming_capabilities()` and `outgoing_capabilities()` reflect the
//! controller side only (`kdeconnect.systemvolume` in, `.request` out).
//! Once a backend is attached, the provider side adds
//! `kdeconnect.systemvolume.request` to incoming and
//! `kdeconnect.systemvolume` to outgoing. The tripwire test
//! (`tests/rust_capability_inventory.rs`) compares the no-backend shape
//! against the committed fixture; the caps-conditional test in this
//! module verifies the with-backend shape.

pub mod backend;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

use tracing::{info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::events::PluginEventBroadcaster;
use super::plugin::Plugin;

use backend::{detect as detect_pactl, LocalSinkState, SubscribeEvent, VolumeBackend};

#[cfg(test)]
use backend::MockBackend;

/// One PulseAudio sink as the desktop peer describes it.
///
/// Keys match the object kdeconnect-kde puts in `sinkList`
/// (systemvolumeplugin-pulse.cpp:90-95) and that kdeconnect-android
/// reads in .../systemvolume/Sink.kt:26-31.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SinkState {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Absolute, integer, ceiling is `max_volume` (pulse.cpp:71,94).
    #[serde(default)]
    pub volume: Option<i64>,
    #[serde(default)]
    pub max_volume: Option<i64>,
    #[serde(default)]
    pub muted: Option<bool>,
    /// This sink is the default output (pulse.cpp:85,95; Sink.kt:31).
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl From<LocalSinkState> for SinkState {
    fn from(s: LocalSinkState) -> Self {
        SinkState {
            name: s.name,
            description: s.description,
            volume: s.volume,
            max_volume: s.max_volume,
            muted: s.muted,
            enabled: s.enabled,
        }
    }
}

pub struct SystemVolumePlugin {
    /// Controller-side sink map: device_id -> sink name -> sink state.
    /// Owned by the controller side; the provider side keeps its own
    /// `local_sinks` snapshot.
    sinks: RwLock<HashMap<String, HashMap<String, SinkState>>>,

    /// Provider-side snapshot of THIS desktop's sinks. Single source of
    /// truth for /api/v1/systemvolume/sinks and for the wire payload
    /// when the phone asks. Refreshed on subscribe events and on
    /// requestSinks. Shared with the supervisor task, which mirrors
    /// every pushed state into it.
    local_sinks: Arc<RwLock<HashMap<String, SinkState>>>,
    /// Provider default sink name (the one currently marked enabled).
    default_sink: Arc<RwLock<Option<String>>>,
    /// Backend seam; `None` until `enable_session_backend()` succeeds.
    /// All provider behavior is gated on this being set.
    backend: RwLock<Option<Arc<dyn VolumeBackend>>>,
    /// Connection manager — the fan-out path for pushes to paired
    /// devices on local sink changes (clipboard/mpris pattern).
    connection_manager: RwLock<Option<Arc<crate::protocol::ConnectionManager>>>,
    /// Device registry — the supervisor's peer sync reads peer
    /// capabilities from it to decide which side we play per peer:
    /// push sinkList to consumers (peer incoming has
    /// `kdeconnect.systemvolume`), request sinks only from providers
    /// (peer outgoing has it).
    device_registry: RwLock<Option<Arc<crate::device::DeviceRegistry>>>,
    /// Plugin event broadcaster — surface local sink changes on the
    /// SSE event stream so the UI/CLI can react.
    plugin_events: Arc<PluginEventBroadcaster>,
    /// Subscribe started flag. Idempotent across rewire calls
    /// (clipboard.rs pattern).
    watcher_started: AtomicBool,
    /// Cached result of the latest backend availability check; the
    /// tripwire compares this against the committed fixture.
    backend_available: AtomicBool,
}

impl Default for SystemVolumePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemVolumePlugin {
    pub fn new() -> Self {
        Self {
            sinks: RwLock::new(HashMap::new()),
            local_sinks: Arc::new(RwLock::new(HashMap::new())),
            default_sink: Arc::new(RwLock::new(None)),
            backend: RwLock::new(None),
            connection_manager: RwLock::new(None),
            device_registry: RwLock::new(None),
            plugin_events: Arc::new(PluginEventBroadcaster::new(16, "plugin")),
            watcher_started: AtomicBool::new(false),
            backend_available: AtomicBool::new(false),
        }
    }

    /// Wire the connection manager + plugin events broadcaster. Called
    /// from `loader.rs` for the production path.
    pub fn with_connection_manager(
        mut self,
        connection_manager: Arc<crate::protocol::ConnectionManager>,
        plugin_events: Arc<PluginEventBroadcaster>,
    ) -> Self {
        *self
            .connection_manager
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(connection_manager);
        self.plugin_events = plugin_events;
        self
    }

    /// Wire the device registry for the capability-gated peer sync.
    /// Called from bootstrap after the plugin is Arc'd (the loader's
    /// builder chain does not have the registry).
    pub fn with_device_registry(&self, registry: Arc<crate::device::DeviceRegistry>) {
        *self
            .device_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(registry);
    }

    /// Inject a backend (tests use a recording mock; production uses
    /// the pactl detect path).
    pub async fn set_backend(&self, backend: Arc<dyn VolumeBackend>) {
        let available = backend.is_available();
        *self.backend.write().unwrap_or_else(|e| e.into_inner()) = Some(backend);
        self.backend_available.store(available, Ordering::SeqCst);
        self.try_start_watcher().await;
    }

    /// Connect the real local pactl backend. Called ONLY from the
    /// production entry point (bootstrap.rs create_state) — never from
    /// AppState::new, which the test suite exercises against the
    /// developer's live session. Degrades with a log event when pactl
    /// is missing or the PA daemon is unreachable.
    pub async fn enable_session_backend(&self) {
        match detect_pactl() {
            Some(backend) => {
                let name = backend.name().to_string();
                let initial = backend.list_sinks().await;
                match initial {
                    Ok(initial) => {
                        let count = initial.len();
                        info!(
                            event = "systemvolume_backend_ready",
                            backend = %name,
                            sinks = count,
                            "Local system-volume backend enabled"
                        );
                        self.apply_local_sinks(initial);
                        self.recompute_default();
                        self.set_backend(Arc::new(backend)).await;
                    }
                    Err(e) => {
                        warn!(
                            backend = %name,
                            error = %e,
                            event = "systemvolume_backend_no_sinks",
                            "pactl backend detected but list_sinks failed; degraded"
                        );
                    }
                }
            }
            None => {
                warn!(
                    event = "systemvolume_backend_unavailable",
                    "No usable PulseAudio/PipeWire backend (pactl missing or daemon unreachable). \
                     Provider side of systemvolume will not advertise."
                );
            }
        }
    }

    pub fn backend(&self) -> Option<Arc<dyn VolumeBackend>> {
        self.backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Start the subscribe watcher once both backend and connection
    /// manager are present. Idempotent (clipboard.rs pattern).
    async fn try_start_watcher(&self) {
        if self.watcher_started.load(Ordering::SeqCst) {
            return;
        }
        let (Some(cm), Some(backend)) = (
            self.connection_manager
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            self.backend(),
        ) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubscribeEvent>();
        if let Err(e) = backend.start_subscribe(tx).await {
            warn!(
                error = %e,
                event = "systemvolume_subscribe_unavailable",
                "Could not start pactl subscribe"
            );
            return;
        }
        self.watcher_started.store(true, Ordering::SeqCst);

        let local_sinks = self.local_sinks_handle();
        let default_sink = self.default_sink_handle();
        let live_sinks = self.local_sinks.clone();
        let live_default = self.default_sink.clone();
        let registry_for_loop = self
            .device_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let backend_for_loop = backend.clone();
        let plugin_events = self.plugin_events.clone();
        let cm_for_loop = cm.clone();

        // Supervised subscribe loop. Exponential backoff on unexpected
        // exit (clipboard.rs WATCHER_INITIAL_BACKOFF / MAX_BACKOFF /
        // HEALTHY_AFTER pattern). A 5s tick drives the capability-gated
        // peer sync so newly connected consumers get their sinkList
        // without waiting for a sink event.
        tokio::spawn(async move {
            const INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);
            const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
            const HEALTHY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
            let mut backoff = INITIAL_BACKOFF;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut synced: std::collections::HashSet<String> = Default::default();
            loop {
                let started = std::time::Instant::now();
                let exited = loop {
                    tokio::select! {
                        ev = rx.recv() => {
                            match ev {
                                Some(ev) => {
                                    if let Some(new_list) = handle_subscribe_event(
                                        &ev,
                                        &backend_for_loop,
                                        &local_sinks,
                                        &default_sink,
                                    )
                                    .await
                                    {
                                        push_local_state(
                                            &new_list,
                                            &local_sinks,
                                            &default_sink,
                                            &live_sinks,
                                            &live_default,
                                            &plugin_events,
                                            &cm_for_loop,
                                        )
                                        .await;
                                    }
                                }
                                None => break true,
                            }
                        }
                        _ = tick.tick() => {}
                    }
                    sync_peers(&registry_for_loop, &cm_for_loop, &live_sinks, &mut synced).await;
                };
                let _ = exited;
                if started.elapsed() >= HEALTHY_AFTER {
                    backoff = INITIAL_BACKOFF;
                }
                tracing::warn!(
                    backoff_ms = backoff.as_millis() as u64,
                    event = "systemvolume_subscribe_restart",
                    "pactl subscribe exited; restarting after backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                // Re-arm the subscription. The previous sender is
                // gone; we build a fresh channel and pass a new
                // sender in.
                let (new_tx, new_rx) = tokio::sync::mpsc::unbounded_channel::<SubscribeEvent>();
                if let Err(e) = backend_for_loop.start_subscribe(new_tx).await {
                    tracing::warn!(
                        error = %e,
                        event = "systemvolume_subscribe_respawn_failed",
                        "could not respawn pactl subscribe"
                    );
                }
                rx = new_rx;
            }
        });
    }

    /// Test-visible handle to the local-sinks map. Returns a clone of
    /// the sink state for `name`.
    pub fn get_local_sink(&self, name: &str) -> Option<SinkState> {
        self.local_sinks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// All known local sinks, sorted by name so callers get a stable
    /// order out of the map.
    pub fn get_local_sinks(&self) -> Vec<SinkState> {
        let guard = self.local_sinks.read().unwrap_or_else(|e| e.into_inner());
        let mut sinks: Vec<SinkState> = guard.values().cloned().collect();
        sinks.sort_by(|a, b| a.name.cmp(&b.name));
        sinks
    }

    /// Default sink name, if any.
    pub fn get_default_sink(&self) -> Option<String> {
        self.default_sink
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// All known sinks for a (remote) device, sorted by name so callers
    /// get a stable order out of the map. Controller side.
    pub fn get_sinks(&self, device_id: &str) -> Vec<SinkState> {
        let guard = self.sinks.read().unwrap_or_else(|e| e.into_inner());
        let mut sinks: Vec<SinkState> = guard
            .get(device_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        sinks.sort_by(|a, b| a.name.cmp(&b.name));
        sinks
    }

    pub fn get_sink(&self, device_id: &str, name: &str) -> Option<SinkState> {
        self.sinks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .and_then(|m| m.get(name))
            .cloned()
    }

    /// Apply a fresh local-sinks list to the cached state. Preserves
    /// `description`/`max_volume` for sinks we already know about
    /// (pactl sometimes returns them empty for entries that survived
    /// a refresh).
    fn apply_local_sinks(&self, sinks: Vec<LocalSinkState>) {
        let mut state = self.local_sinks.write().unwrap_or_else(|e| e.into_inner());
        let new_names: std::collections::HashSet<String> =
            sinks.iter().map(|s| s.name.clone()).collect();
        // Drop sinks that disappeared.
        state.retain(|name, _| new_names.contains(name));
        for s in sinks {
            let mut existing = state.get(&s.name).cloned().unwrap_or_default();
            if s.description.is_some() {
                existing.description = s.description.clone();
            }
            if s.max_volume.is_some() {
                existing.max_volume = s.max_volume;
            }
            if s.volume.is_some() {
                existing.volume = s.volume;
            }
            if s.muted.is_some() {
                existing.muted = s.muted;
            }
            // enabled carries the default-sink flag from mark_default;
            // recompute_default reads it back from this state, so it must
            // be merged here or the default is never discovered.
            if s.enabled.is_some() {
                existing.enabled = s.enabled;
            }
            existing.name = s.name.clone();
            state.insert(s.name.clone(), existing);
        }
    }

    /// Update the default-sink marker after a fresh list.
    fn recompute_default(&self) {
        let state = self.local_sinks.read().unwrap_or_else(|e| e.into_inner());
        let mut default = self.default_sink.write().unwrap_or_else(|e| e.into_inner());
        let new_default = state
            .values()
            .find(|s| s.enabled == Some(true))
            .map(|s| s.name.clone());
        *default = new_default;
    }

    /// Snapshot-based handles for the supervisor task. The supervisor
    /// holds its own copy so a refresh on the wire doesn't poison its
    /// diff.
    fn local_sinks_handle(&self) -> Arc<RwLock<HashMap<String, SinkState>>> {
        let snapshot = self
            .local_sinks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Arc::new(RwLock::new(snapshot))
    }

    fn default_sink_handle(&self) -> Arc<RwLock<Option<String>>> {
        Arc::new(RwLock::new(
            self.default_sink
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        ))
    }

    /// Test-visible: simulate a backend-availability flip.
    pub fn set_backend_available_for_test(&self, available: bool) {
        self.backend_available.store(available, Ordering::SeqCst);
    }

    /// Test-visible: force the watcher to never start (so tests can
    /// exercise the request path without a real subscribe process).
    pub fn disable_watcher_for_test(&self) {
        self.watcher_started.store(false, Ordering::SeqCst);
    }

    /// Test-visible: bootstrap local sinks for tests.
    pub fn with_local_sinks_for_test(&self, sinks: Vec<SinkState>) {
        {
            let mut state = self.local_sinks.write().unwrap_or_else(|e| e.into_inner());
            for s in sinks {
                state.insert(s.name.clone(), s);
            }
        }
        self.recompute_default();
    }
}

#[async_trait::async_trait]
impl Plugin for SystemVolumePlugin {
    fn name(&self) -> &str {
        "systemvolume"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        // Controller side is always present: we consume the desktop
        // peer's `kdeconnect.systemvolume` packets. The provider side
        // adds `kdeconnect.systemvolume.request` only when a backend
        // is wired and reports available — this is the honesty rule
        // the tripwire + the spec cover.
        let mut caps = vec!["kdeconnect.systemvolume".to_string()];
        if self.backend_available.load(Ordering::SeqCst) {
            caps.push("kdeconnect.systemvolume.request".to_string());
        }
        caps
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // Controller side: send `kdeconnect.systemvolume.request` to a
        // peer desktop so it sends us its sink list. Provider side:
        // push `kdeconnect.systemvolume` to paired phones when local
        // sinks change, AND respond to a requestSinks with the same
        // packet type.
        let mut caps = vec!["kdeconnect.systemvolume.request".to_string()];
        if self.backend_available.load(Ordering::SeqCst) {
            caps.push("kdeconnect.systemvolume".to_string());
        }
        caps
    }

    fn is_backend_available(&self) -> bool {
        self.backend_available.load(Ordering::SeqCst)
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        // Intentionally empty. The Android app never sends requestSinks:
        // it renders whatever sinkList the desktop pushes (upstream
        // kdeconnect-kde pushes on connect). The handshake lives in the
        // supervisor's capability-gated peer sync (sync_peers), which
        // pushes sinkList to consumers and requests sinks only from
        // providers — so a phone never receives requestSinks spam and a
        // non-provider peer never gets a sinkList it cannot use.
        Vec::new()
    }

    fn on_disconnected(&self, device_id: &str) {
        self.sinks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        match packet.packet_type.as_str() {
            "kdeconnect.systemvolume" => self.handle_controller_state(device_id, packet).await,
            "kdeconnect.systemvolume.request" => {
                self.handle_provider_request(device_id, packet).await
            }
            _ => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Controller side (existing behavior, preserved)
// ---------------------------------------------------------------------------

impl SystemVolumePlugin {
    async fn handle_controller_state(
        &self,
        device_id: &str,
        packet: Packet,
    ) -> Result<Option<Vec<Packet>>> {
        let body = &packet.body;

        // Full state. Upstream clears its map before refilling
        // (SystemVolumePlugin.kt:33-42), so this replaces rather than
        // merges.
        if let Some(list) = body.get("sinkList").and_then(|v| v.as_array()) {
            let mut parsed: HashMap<String, SinkState> = HashMap::new();
            for entry in list {
                match serde_json::from_value::<SinkState>(entry.clone()) {
                    Ok(sink) if !sink.name.is_empty() => {
                        parsed.insert(sink.name.clone(), sink);
                    }
                    Ok(_) => {
                        warn!(
                            device_id = %device_id,
                            event = "systemvolume_sink_unnamed",
                            "Dropping sinkList entry with no name"
                        );
                    }
                    Err(e) => {
                        warn!(
                            device_id = %device_id,
                            error = %e,
                            event = "systemvolume_sink_parse_failed",
                            "Dropping malformed sinkList entry"
                        );
                    }
                }
            }
            info!(
                device_id = %device_id,
                sinks = parsed.len(),
                event = "systemvolume_sink_list",
                "Received sink list"
            );
            self.sinks
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(device_id.to_string(), parsed);
            return Ok(None);
        }

        // Otherwise a per-sink delta keyed by `name` (pulse.cpp:72,79,86).
        let Some(name) = body.get("name").and_then(|v| v.as_str()) else {
            warn!(
                device_id = %device_id,
                event = "systemvolume_update_unkeyed",
                "systemvolume packet has neither sinkList nor name, ignoring"
            );
            return Ok(None);
        };

        let mut guard = self.sinks.write().unwrap_or_else(|e| e.into_inner());
        // Upstream ignores a delta for an unknown sink: SystemVolumePlugin.kt:
        // 53-55 looks the name up and does nothing when it is absent.
        let Some(sink) = guard.get_mut(device_id).and_then(|m| m.get_mut(name)) else {
            warn!(
                device_id = %device_id,
                sink = %name,
                event = "systemvolume_unknown_sink",
                "Update for a sink not in the last sinkList, ignoring"
            );
            return Ok(None);
        };

        if let Some(volume) = body.get("volume").and_then(|v| v.as_i64()) {
            sink.volume = Some(volume);
        }
        if let Some(muted) = body.get("muted").and_then(|v| v.as_bool()) {
            sink.muted = Some(muted);
        }
        if let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) {
            sink.enabled = Some(enabled);
        }

        info!(
            device_id = %device_id,
            sink = %name,
            volume = ?sink.volume,
            muted = ?sink.muted,
            enabled = ?sink.enabled,
            event = "systemvolume_update",
            "Received system volume update"
        );

        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Provider side (new)
// ---------------------------------------------------------------------------

impl SystemVolumePlugin {
    /// Handle an incoming `kdeconnect.systemvolume.request` from a paired
    /// phone. Two shapes, both upstream-defined:
    ///
    /// 1. `{"requestSinks": true}` — phone wants the full sink list.
    ///    We refresh from the backend and reply with a `sinkList`
    ///    packet (systemvolumeplugin-pulse.cpp:36-37).
    /// 2. `{"name": "...", "volume": N | "muted": bool | "enabled":
    ///    bool}` — phone wants to control one of our sinks. We apply
    ///    the change via the backend (pulse.cpp:41-54). Note: when
    ///    the backend doesn't have the sink, we log and return
    ///    without error (matches upstream, which silently no-ops via
    ///    the same `sinksMap.value(name)` lookup at pulse.cpp:41).
    async fn handle_provider_request(
        &self,
        device_id: &str,
        packet: Packet,
    ) -> Result<Option<Vec<Packet>>> {
        let Some(backend) = self.backend() else {
            warn!(
                device_id = %device_id,
                event = "systemvolume_request_no_backend",
                "systemvolume.request received but no backend wired"
            );
            return Ok(None);
        };

        let body = &packet.body;

        // requestSinks path
        if body.get("requestSinks").and_then(|v| v.as_bool()) == Some(true) {
            let sinks = match backend.list_sinks().await {
                Ok(sinks) => sinks,
                Err(e) => {
                    warn!(
                        device_id = %device_id,
                        error = %e,
                        event = "systemvolume_list_failed",
                        "list_sinks failed during requestSinks"
                    );
                    return Ok(None);
                }
            };
            self.apply_local_sinks(sinks.clone());
            self.recompute_default();
            info!(
                device_id = %device_id,
                sinks = sinks.len(),
                event = "systemvolume_request_sinks",
                "Answering requestSinks with sinkList"
            );
            return Ok(Some(vec![self.sink_list_packet(&sinks)]));
        }

        // Per-sink control path. Pulse.cpp:43-54: `volume` ALSO
        // clears mute (setMuted(false)), and `enabled` is a
        // default-sink bit.
        let Some(name) = body.get("name").and_then(|v| v.as_str()) else {
            warn!(
                device_id = %device_id,
                event = "systemvolume_request_unkeyed",
                "systemvolume.request packet has no `name`, ignoring"
            );
            return Ok(None);
        };

        // Upstream applies volume FIRST (pulse.cpp:43-46), then
        // muted (49), then enabled (51). We mirror.
        if let Some(volume) = body.get("volume").and_then(|v| v.as_i64()) {
            if let Err(e) = backend.set_volume(name, volume).await {
                warn!(
                    device_id = %device_id,
                    sink = %name,
                    error = %e,
                    event = "systemvolume_set_volume_failed",
                    "set_volume on backend failed"
                );
                return Ok(None);
            }
            // Pulse.cpp:46 also un-mutes: plugin.cpp:43-46 sets
            // volume AND calls setMuted(false).
            if let Err(e) = backend.set_muted(name, false).await {
                warn!(
                    device_id = %device_id,
                    sink = %name,
                    error = %e,
                    event = "systemvolume_unmute_failed",
                    "set_muted(false) on backend failed"
                );
            }
        }
        if let Some(muted) = body.get("muted").and_then(|v| v.as_bool()) {
            if let Err(e) = backend.set_muted(name, muted).await {
                warn!(
                    device_id = %device_id,
                    sink = %name,
                    error = %e,
                    event = "systemvolume_set_muted_failed",
                    "set_muted on backend failed"
                );
                return Ok(None);
            }
        }
        if let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) {
            if let Err(e) = backend.set_default(name, enabled).await {
                warn!(
                    device_id = %device_id,
                    sink = %name,
                    error = %e,
                    event = "systemvolume_set_default_failed",
                    "set_default on backend failed"
                );
                return Ok(None);
            }
        }

        Ok(None)
    }

    /// Build the upstream-shape sinkList packet.
    /// (`kdeconnect.systemvolume`, body = `{ "sinkList": [ ... ] }`)
    fn sink_list_packet(&self, sinks: &[LocalSinkState]) -> Packet {
        let list: Vec<SinkState> = sinks.iter().cloned().map(SinkState::from).collect();
        Packet::new(
            "kdeconnect.systemvolume".to_string(),
            serde_json::json!({ "sinkList": list }),
        )
    }
}

// ---------------------------------------------------------------------------
// Helpers for the subscribe loop
// ---------------------------------------------------------------------------

/// Handle a single subscribe event: re-list the sinks, return the new
/// snapshot so the caller can diff against the previous one and push
/// deltas. Returns `None` when the backend is unavailable or the
/// event should not trigger a refresh (Unclassified events).
async fn handle_subscribe_event(
    ev: &SubscribeEvent,
    backend: &Arc<dyn VolumeBackend>,
    _local_sinks: &Arc<RwLock<HashMap<String, SinkState>>>,
    _default_sink: &Arc<RwLock<Option<String>>>,
) -> Option<Vec<LocalSinkState>> {
    match ev {
        SubscribeEvent::SinkAdded { .. }
        | SubscribeEvent::SinkRemoved { .. }
        | SubscribeEvent::SinkChanged { .. }
        | SubscribeEvent::DefaultSinkChanged { .. } => match backend.list_sinks().await {
            Ok(list) => Some(list),
            Err(e) => {
                warn!(
                    error = %e,
                    event = "systemvolume_event_list_failed",
                    "list_sinks failed during subscribe event"
                );
                None
            }
        },
        SubscribeEvent::Unclassified { line } => {
            tracing::debug!(
                line = %line,
                event = "systemvolume_subscribe_unclassified",
                "Ignoring unclassified subscribe event"
            );
            None
        }
    }
}

/// Capability-gated peer handshake (the async side of `on_connected`).
/// Pushes the full sinkList to every connected consumer (peer incoming
/// has `kdeconnect.systemvolume` — the Android app renders whatever the
/// desktop pushes and never asks) and requests sinks only from peers
/// that advertise a provider (desktop-to-desktop). Runs on subscribe
/// events and a 5s tick; each peer is synced once per connection.
async fn sync_peers(
    registry: &Option<Arc<crate::device::DeviceRegistry>>,
    cm: &Arc<crate::protocol::ConnectionManager>,
    live_sinks: &Arc<RwLock<HashMap<String, SinkState>>>,
    synced: &mut std::collections::HashSet<String>,
) {
    let Some(reg) = registry else {
        return;
    };
    let connected = cm.connected_device_ids().await;
    synced.retain(|d| connected.contains(d));
    for dev in connected {
        if synced.contains(&dev) {
            continue;
        }
        let Ok(peer) = reg.get(&dev).await else {
            continue;
        };
        let consumes = peer
            .incoming_capabilities
            .iter()
            .any(|c| c == "kdeconnect.systemvolume");
        let provides = peer
            .outgoing_capabilities
            .iter()
            .any(|c| c == "kdeconnect.systemvolume");
        if consumes {
            let list = build_sink_list_packet(live_sinks);
            if let Err(e) = cm.send_packet(&dev, &list).await {
                tracing::warn!(
                    device_id = %dev,
                    error = %e,
                    event = "systemvolume_initial_list_failed",
                    "Failed to push initial sink list"
                );
            }
        }
        if provides {
            let req = Packet::new(
                "kdeconnect.systemvolume.request".to_string(),
                serde_json::json!({ "requestSinks": true }),
            );
            let _ = cm.send_packet(&dev, &req).await;
        }
        synced.insert(dev);
    }
}

/// Full `sinkList` packet in the upstream shape
/// (systemvolumeplugin-pulse.cpp:90-95).
fn build_sink_list_packet(live_sinks: &Arc<RwLock<HashMap<String, SinkState>>>) -> Packet {
    let state = live_sinks.read().unwrap_or_else(|e| e.into_inner());
    Packet::new(
        "kdeconnect.systemvolume".to_string(),
        serde_json::json!({ "sinkList": state.values().cloned().collect::<Vec<_>>() }),
    )
}

/// Push the new local-sinks list to every connected device and to
/// the plugin event stream. Replicates the sinkList / per-sink delta
/// shape upstream publishes (pulse.cpp:69-104).
#[allow(clippy::too_many_arguments)]
async fn push_local_state(
    new_list: &[LocalSinkState],
    local_sinks: &Arc<RwLock<HashMap<String, SinkState>>>,
    default_sink: &Arc<RwLock<Option<String>>>,
    live_sinks: &Arc<RwLock<HashMap<String, SinkState>>>,
    live_default: &Arc<RwLock<Option<String>>>,
    plugin_events: &Arc<PluginEventBroadcaster>,
    cm: &Arc<crate::protocol::ConnectionManager>,
) {
    // Compute the new sink maps so we can diff.
    let mut new_map: HashMap<String, SinkState> = HashMap::new();
    for s in new_list {
        new_map.insert(s.name.clone(), SinkState::from(s.clone()));
    }
    let new_default = new_map
        .values()
        .find(|s| s.enabled == Some(true))
        .map(|s| s.name.clone());

    // Diff against the supervisor's previous snapshot.
    let old_map = local_sinks
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let old_default = default_sink
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    let deltas: Vec<Packet> = build_deltas(&old_map, new_list);

    // Update the supervisor's snapshot (the diff base for wire pushes).
    {
        let mut state = local_sinks.write().unwrap_or_else(|e| e.into_inner());
        *state = new_map.clone();
    }
    {
        let mut d = default_sink.write().unwrap_or_else(|e| e.into_inner());
        *d = new_default.clone();
    }
    // Mirror into the plugin's live state so the REST surface and
    // requestSinks answers track reality — the snapshot above is only
    // the supervisor's diff base.
    {
        let mut state = live_sinks.write().unwrap_or_else(|e| e.into_inner());
        *state = new_map.clone();
    }
    {
        let mut d = live_default.write().unwrap_or_else(|e| e.into_inner());
        *d = new_default.clone();
    }

    // Emit a plugin event so the SSE channel sees the delta.
    plugin_events.broadcast(crate::plugins::events::PluginEvent::SystemVolumeUpdate {
        sinks: new_map.values().cloned().collect(),
    });

    // Push deltas to every connected device.
    for device_id in cm.connected_device_ids().await {
        for pkt in &deltas {
            if let Err(e) = cm.send_packet(&device_id, pkt).await {
                warn!(
                    device_id = %device_id,
                    error = %e,
                    event = "systemvolume_send_delta_failed",
                    "Failed to push systemvolume delta"
                );
            }
        }
        // Push a full sinkList when the sink SET changed or the default
        // moved — upstream rebuilds its copy on sinkAdded/Removed
        // (pulse.cpp:109-115) but not on defaultChanged; the safest for
        // the phone is the full list so its UI can re-render. Per-sink
        // deltas alone would leave new sinks invisible on the phone.
        let set_changed =
            old_map.len() != new_map.len() || new_map.keys().any(|k| !old_map.contains_key(k));
        if old_default != new_default || set_changed {
            let list_packet = Packet::new(
                "kdeconnect.systemvolume".to_string(),
                serde_json::json!({ "sinkList": new_map.values().cloned().collect::<Vec<_>>() }),
            );
            if let Err(e) = cm.send_packet(&device_id, &list_packet).await {
                warn!(
                    device_id = %device_id,
                    error = %e,
                    event = "systemvolume_send_list_failed",
                    "Failed to push systemvolume sink list"
                );
            }
        }
    }
}

/// Diff helper used by the subscribe-loop push step. Exposed for
/// testing so we don't need a full runtime to assert the delta shape.
fn build_deltas(old: &HashMap<String, SinkState>, new: &[LocalSinkState]) -> Vec<Packet> {
    let mut out = Vec::new();
    for s in new {
        let Some(prev) = old.get(&s.name) else {
            continue;
        };
        let sink: SinkState = s.clone().into();
        if prev.volume != sink.volume {
            out.push(Packet::new(
                "kdeconnect.systemvolume".to_string(),
                serde_json::json!({
                    "name": sink.name,
                    "volume": sink.volume,
                }),
            ));
        }
        if prev.muted != sink.muted {
            out.push(Packet::new(
                "kdeconnect.systemvolume".to_string(),
                serde_json::json!({
                    "name": sink.name,
                    "muted": sink.muted,
                }),
            ));
        }
        if prev.enabled != sink.enabled {
            out.push(Packet::new(
                "kdeconnect.systemvolume".to_string(),
                serde_json::json!({
                    "name": sink.name,
                    "enabled": sink.enabled,
                }),
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    // ----- helpers -----

    fn controller_state_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.systemvolume".to_string(), body)
    }

    fn request_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.systemvolume.request".to_string(), body)
    }

    /// EXACT sinkList kdeconnect-kde builds from PulseAudio
    /// (plugins/systemvolume/systemvolumeplugin-pulse.cpp:90-104).
    /// `volume` is an int on an absolute scale; `maxVolume` is
    /// PulseAudioQt::normalVolume() == 65536 (:94).
    fn kde_sink_list() -> serde_json::Value {
        serde_json::json!({
            "sinkList": [
                {
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo",
                    "muted": false,
                    "description": "Built-in Audio Analog Stereo",
                    "volume": 45874,
                    "maxVolume": 65536,
                    "enabled": true
                },
                {
                    "name": "alsa_output.usb-Generic_USB_Audio-00.analog-stereo",
                    "muted": true,
                    "description": "USB Audio Analog Stereo",
                    "volume": 65536,
                    "maxVolume": 65536,
                    "enabled": false
                }
            ]
        })
    }

    // ----- Plugin identity, no backend -----

    #[tokio::test]
    async fn test_systemvolume_name() {
        let plugin = SystemVolumePlugin::new();
        assert_eq!(plugin.name(), "systemvolume");
    }

    #[tokio::test]
    async fn test_caps_without_backend_are_controller_only() {
        let plugin = SystemVolumePlugin::new();
        let incoming = plugin.incoming_capabilities();
        let outgoing = plugin.outgoing_capabilities();
        assert_eq!(incoming, vec!["kdeconnect.systemvolume".to_string()]);
        assert_eq!(
            outgoing,
            vec!["kdeconnect.systemvolume.request".to_string()]
        );
        assert!(!plugin.is_backend_available());
    }

    /// With a live backend, BOTH roles advertise. This is the
    /// capabilities shape the tripwire fixture would fail to see
    /// without a mock-injection test.
    #[tokio::test]
    async fn test_caps_with_backend_include_provider_both_directions() {
        let plugin = SystemVolumePlugin::new();
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s".to_string(),
            max_volume: Some(65_536),
            ..Default::default()
        }]);
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();

        let incoming = plugin.incoming_capabilities();
        let outgoing = plugin.outgoing_capabilities();
        assert!(incoming.contains(&"kdeconnect.systemvolume".to_string()));
        assert!(incoming.contains(&"kdeconnect.systemvolume.request".to_string()));
        assert!(outgoing.contains(&"kdeconnect.systemvolume.request".to_string()));
        assert!(outgoing.contains(&"kdeconnect.systemvolume".to_string()));
        assert!(plugin.is_backend_available());
    }

    /// Tripwire: a backend that reports `is_available() == false`
    /// does not change the caps. Defensive against PA-daemon-stopped
    /// mid-flight.
    #[tokio::test]
    async fn test_caps_follow_is_available_not_presence() {
        let plugin = SystemVolumePlugin::new();
        let backend = MockBackend::new();
        backend.set_available(false);
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();

        let incoming = plugin.incoming_capabilities();
        assert_eq!(incoming, vec!["kdeconnect.systemvolume".to_string()]);
        assert!(!plugin.is_backend_available());
    }

    // ----- Controller-side regression (existing behavior) -----

    #[tokio::test]
    async fn test_sink_list_parsed_per_sink() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", controller_state_packet(kde_sink_list()))
            .await
            .unwrap();

        let sinks = plugin.get_sinks("device1");
        assert_eq!(sinks.len(), 2);

        let builtin = plugin
            .get_sink("device1", "alsa_output.pci-0000_00_1f.3.analog-stereo")
            .expect("Value expected to be present");
        assert_eq!(builtin.volume, Some(45874));
        assert_eq!(builtin.max_volume, Some(65536));
        assert_eq!(builtin.muted, Some(false));
        assert_eq!(builtin.enabled, Some(true));
        assert_eq!(
            builtin.description.as_deref(),
            Some("Built-in Audio Analog Stereo")
        );

        let usb = plugin
            .get_sink(
                "device1",
                "alsa_output.usb-Generic_USB_Audio-00.analog-stereo",
            )
            .expect("Value expected to be present");
        assert_eq!(usb.volume, Some(65536));
        assert_eq!(usb.muted, Some(true));
        assert_eq!(usb.enabled, Some(false));
    }

    #[tokio::test]
    async fn test_sink_list_replaces_previous_set() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", controller_state_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                controller_state_packet(serde_json::json!({
                    "sinkList": [
                        { "name": "only-one", "muted": false, "description": "d",
                          "volume": 100, "maxVolume": 65536, "enabled": true }
                    ]
                })),
            )
            .await
            .unwrap();
        let sinks = plugin.get_sinks("device1");
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "only-one");
    }

    #[tokio::test]
    async fn test_per_sink_volume_update_keyed_by_name() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", controller_state_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                controller_state_packet(serde_json::json!({
                    "volume": 32768,
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo"
                })),
            )
            .await
            .unwrap();

        assert_eq!(
            plugin
                .get_sink("device1", "alsa_output.pci-0000_00_1f.3.analog-stereo")
                .expect("Value expected to be present")
                .volume,
            Some(32768)
        );
        assert_eq!(
            plugin
                .get_sink(
                    "device1",
                    "alsa_output.usb-Generic_USB_Audio-00.analog-stereo"
                )
                .expect("Value expected to be present")
                .volume,
            Some(65536)
        );
    }

    #[tokio::test]
    async fn test_per_sink_muted_and_enabled_updates() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", controller_state_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                controller_state_packet(serde_json::json!({
                    "muted": true,
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo"
                })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                controller_state_packet(serde_json::json!({
                    "enabled": false,
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo"
                })),
            )
            .await
            .unwrap();

        let sink = plugin
            .get_sink("device1", "alsa_output.pci-0000_00_1f.3.analog-stereo")
            .expect("Value expected to be present");
        assert_eq!(sink.muted, Some(true));
        assert_eq!(sink.enabled, Some(false));
    }

    #[tokio::test]
    async fn test_update_for_unknown_sink_is_ignored() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", controller_state_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                controller_state_packet(json!({ "volume": 1, "name": "ghost-sink" })),
            )
            .await
            .unwrap();
        assert_eq!(plugin.get_sinks("device1").len(), 2);
        assert!(plugin.get_sink("device1", "ghost-sink").is_none());
    }

    #[tokio::test]
    async fn test_malformed_sink_entry_skipped() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet(
                "device1",
                controller_state_packet(serde_json::json!({
                    "sinkList": [
                        { "name": "good", "volume": 100, "maxVolume": 65536,
                          "muted": false, "description": "d", "enabled": true },
                        { "volume": 200 },
                        "not-an-object"
                    ]
                })),
            )
            .await
            .unwrap();
        let sinks = plugin.get_sinks("device1");
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "good");
    }

    // ----- Provider-side: request handling -----

    #[tokio::test]
    async fn test_request_sinks_returns_sinklist() {
        let sinks = vec![
            LocalSinkState {
                name: "s1".to_string(),
                description: Some("Sink 1".to_string()),
                volume: Some(50_000),
                max_volume: Some(65_536),
                muted: Some(false),
                enabled: Some(true),
            },
            LocalSinkState {
                name: "s2".to_string(),
                description: Some("Sink 2".to_string()),
                volume: Some(20_000),
                max_volume: Some(65_536),
                muted: Some(true),
                enabled: Some(false),
            },
        ];
        let backend = MockBackend::new()
            .with_sinks(sinks.clone())
            .with_default("s1");
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        let response = plugin
            .handle_packet("phone1", request_packet(json!({ "requestSinks": true })))
            .await
            .unwrap()
            .expect("response packet");
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].packet_type, "kdeconnect.systemvolume");
        let list = response[0]
            .body
            .get("sinkList")
            .expect("sinkList")
            .as_array()
            .unwrap();
        assert_eq!(list.len(), 2);
        let names: Vec<&str> = list
            .iter()
            .map(|s| s.get("name").and_then(|v| v.as_str()).unwrap())
            .collect();
        assert!(names.contains(&"s1"));
        assert!(names.contains(&"s2"));
    }

    #[tokio::test]
    async fn test_volume_request_applies_to_backend() {
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s1".to_string(),
            max_volume: Some(65_536),
            volume: Some(0),
            muted: Some(false),
            enabled: Some(true),
            description: Some("d".to_string()),
        }]);
        let backend_arc = backend.clone();
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        let r = plugin
            .handle_packet(
                "phone1",
                request_packet(json!({ "name": "s1", "volume": 32768 })),
            )
            .await
            .unwrap();
        assert!(r.is_none());
        let calls = backend_arc.calls();
        // volume path issues set_volume AND set_muted(false) per pulse.cpp:46
        assert!(calls.iter().any(|c| c.starts_with("set_volume:s1:32768")));
        assert!(calls.iter().any(|c| c == "set_muted:s1:false"));
    }

    #[tokio::test]
    async fn test_muted_request_applies_to_backend() {
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s1".to_string(),
            max_volume: Some(65_536),
            volume: Some(0),
            muted: Some(false),
            enabled: Some(true),
            description: Some("d".to_string()),
        }]);
        let backend_arc = backend.clone();
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        let _ = plugin
            .handle_packet(
                "phone1",
                request_packet(json!({ "name": "s1", "muted": true })),
            )
            .await
            .unwrap();
        let calls = backend_arc.calls();
        assert!(calls.iter().any(|c| c == "set_muted:s1:true"));
    }

    #[tokio::test]
    async fn test_default_request_applies_to_backend() {
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s1".to_string(),
            max_volume: Some(65_536),
            ..Default::default()
        }]);
        let backend_arc = backend.clone();
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        let _ = plugin
            .handle_packet(
                "phone1",
                request_packet(json!({ "name": "s1", "enabled": true })),
            )
            .await
            .unwrap();
        let calls = backend_arc.calls();
        assert!(calls.iter().any(|c| c == "set_default:s1:true"));
    }

    #[tokio::test]
    async fn test_request_without_backend_is_noop() {
        let plugin = SystemVolumePlugin::new();
        // No backend wired.
        let r = plugin
            .handle_packet("phone1", request_packet(json!({ "requestSinks": true })))
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn test_request_unknown_sink_name_does_not_crash() {
        // Upstream pulse.cpp:41 — `sinksMap.value(name)` returns null
        // and the branch is skipped. We log a warning but don't fail.
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s1".to_string(),
            max_volume: Some(65_536),
            ..Default::default()
        }]);
        let _ = backend.clone();
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        let r = plugin
            .handle_packet(
                "phone1",
                request_packet(json!({ "name": "ghost", "volume": 1 })),
            )
            .await
            .unwrap();
        assert!(r.is_none());
        // The mock backend errors before recording the call. Wire
        // level behaviour is identical to upstream: silent no-op.
    }

    /// Volume import: integer scale, not a 0.0-1.0 fraction.
    #[tokio::test]
    async fn test_volume_is_an_integer_on_the_max_volume_scale() {
        let sink: SinkState = serde_json::from_value(json!({
            "name": "s",
            "muted": false,
            "description": "d",
            "volume": 65536,
            "maxVolume": 65536,
            "enabled": true
        }))
        .unwrap();
        assert_eq!(sink.volume, Some(65536));
        assert_eq!(sink.max_volume, Some(65536));
    }

    #[tokio::test]
    async fn test_on_connected_is_quiet_peer_sync_owns_handshake() {
        // on_connected no longer emits requestSinks blindly (that spammed
        // phones on every identity re-exchange). The capability-gated
        // handshake lives in sync_peers.
        let plugin = SystemVolumePlugin::new();
        assert!(plugin.on_connected("device1").is_empty());
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_sinks() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", controller_state_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin.on_disconnected("device1");
        assert!(plugin.get_sinks("device1").is_empty());
    }

    // ----- Local sink state shape -----

    #[tokio::test]
    async fn test_local_sinks_round_trip() {
        let plugin = SystemVolumePlugin::new();
        plugin.with_local_sinks_for_test(vec![SinkState {
            name: "foo".to_string(),
            description: Some("Foo".to_string()),
            volume: Some(12_345),
            max_volume: Some(65_536),
            muted: Some(false),
            enabled: Some(true),
        }]);
        let got = plugin.get_local_sinks();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "foo");
        assert_eq!(got[0].volume, Some(12_345));
        assert_eq!(plugin.get_default_sink().as_deref(), Some("foo"));
    }

    #[tokio::test]
    async fn test_local_sinks_apply_preserves_previous_fields() {
        // A re-list may omit description or max_volume for an
        // unchanged sink; the cached fields should survive.
        let plugin = SystemVolumePlugin::new();
        plugin.with_local_sinks_for_test(vec![SinkState {
            name: "foo".to_string(),
            description: Some("Foo".to_string()),
            volume: Some(12_345),
            max_volume: Some(65_536),
            muted: Some(false),
            enabled: Some(true),
        }]);
        // Simulate a re-list that lacks description + max_volume.
        plugin.apply_local_sinks(vec![LocalSinkState {
            name: "foo".to_string(),
            description: None,
            max_volume: None,
            volume: Some(22_222),
            muted: Some(true),
            enabled: Some(true),
        }]);
        let got = plugin.get_local_sinks();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].description.as_deref(), Some("Foo"));
        assert_eq!(got[0].max_volume, Some(65_536));
        assert_eq!(got[0].volume, Some(22_222));
        assert_eq!(got[0].muted, Some(true));
    }

    #[tokio::test]
    async fn test_apply_local_sinks_drops_disappeared() {
        let plugin = SystemVolumePlugin::new();
        plugin.with_local_sinks_for_test(vec![
            SinkState {
                name: "a".to_string(),
                ..Default::default()
            },
            SinkState {
                name: "b".to_string(),
                ..Default::default()
            },
        ]);
        plugin.apply_local_sinks(vec![LocalSinkState {
            name: "a".to_string(),
            ..Default::default()
        }]);
        let sinks = plugin.get_local_sinks();
        let names: Vec<&str> = sinks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a"]);
    }

    /// The provider's per-sink deltas use the same shape as upstream
    /// publishes (pulse.cpp:69-88).
    #[tokio::test]
    async fn test_delta_packet_shape_matches_upstream() {
        let mut old_map = HashMap::new();
        old_map.insert(
            "s".to_string(),
            SinkState {
                name: "s".to_string(),
                volume: Some(10_000),
                muted: Some(false),
                enabled: Some(false),
                max_volume: Some(65_536),
                description: None,
            },
        );
        let new_sinks = vec![LocalSinkState {
            name: "s".to_string(),
            volume: Some(20_000),
            muted: Some(true),
            enabled: Some(true),
            max_volume: Some(65_536),
            description: Some("d".to_string()),
        }];
        let deltas = build_deltas(&old_map, &new_sinks);
        assert_eq!(deltas.len(), 3);
        assert!(deltas
            .iter()
            .any(
                |p| p.body.get("volume").and_then(|v| v.as_i64()) == Some(20_000)
                    && p.body.get("name").and_then(|v| v.as_str()) == Some("s")
            ));
        assert!(deltas
            .iter()
            .any(|p| p.body.get("muted").and_then(|v| v.as_bool()) == Some(true)));
        assert!(deltas
            .iter()
            .any(|p| p.body.get("enabled").and_then(|v| v.as_bool()) == Some(true)));
    }

    #[tokio::test]
    async fn test_subscribe_event_triggers_relist() {
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s1".to_string(),
            max_volume: Some(65_536),
            volume: Some(0),
            muted: Some(false),
            enabled: Some(true),
            description: Some("d".to_string()),
        }]);
        let inner: Arc<dyn VolumeBackend> = backend;
        let list = handle_subscribe_event(
            &SubscribeEvent::SinkChanged {
                name: Some("s1".to_string()),
            },
            &inner,
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(None)),
        )
        .await
        .expect("relist");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "s1");
    }

    #[tokio::test]
    async fn test_subscribe_unclassified_event_returns_none() {
        let backend = MockBackend::new();
        let inner: Arc<dyn VolumeBackend> = backend;
        let r = handle_subscribe_event(
            &SubscribeEvent::Unclassified {
                line: "Event 'change' on client #3693".to_string(),
            },
            &inner,
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(None)),
        )
        .await;
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn test_subscribe_returns_err_when_force_fail() {
        let backend = MockBackend::new();
        backend.force_subscribe_error.store(true, Ordering::SeqCst);
        let r = backend
            .start_subscribe(tokio::sync::mpsc::unbounded_channel().0)
            .await;
        assert!(r.is_err());
    }

    /// enable_session_backend is a no-op when pactl is missing.
    #[tokio::test]
    async fn test_enable_session_backend_keeps_controller_caps_when_pactl_missing() {
        let plugin = SystemVolumePlugin::new();
        // We can't easily force detect() to return None in a unit
        // test, but we can assert that a backend-less plugin keeps
        // its controller-only caps.
        plugin.enable_session_backend().await;
        let out = plugin.outgoing_capabilities();
        assert!(out.contains(&"kdeconnect.systemvolume.request".to_string()));
    }

    #[test]
    fn parse_kde_sink_list_fixture_end_to_end() {
        // The fixture is the JSON object kdeconnect-kde produces
        // (pulse.cpp:90-104).
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "sinkList": [
                    {
                        "name": "alsa_output.pci-0000_00_1f.3.analog-stereo",
                        "muted": false,
                        "description": "Built-in Audio Analog Stereo",
                        "volume": 45874,
                        "maxVolume": 65536,
                        "enabled": true
                    }
                ]
            }"#,
        )
        .unwrap();
        let sink: SinkState =
            serde_json::from_value(body.get("sinkList").unwrap().as_array().unwrap()[0].clone())
                .unwrap();
        assert_eq!(sink.max_volume, Some(65536));
        assert_eq!(sink.volume, Some(45874));
    }

    /// Defensive: rising-edge detection for an unrelated packet type
    /// does nothing.
    #[tokio::test]
    async fn test_unrelated_packet_type_returned_none() {
        let plugin = SystemVolumePlugin::new();
        let r = plugin
            .handle_packet(
                "device1",
                Packet::new("kdeconnect.ping".to_string(), json!({})),
            )
            .await
            .unwrap();
        assert!(r.is_none());
    }

    /// Defensive: an unknown packet type with a requestSinks body
    /// doesn't trigger anything (route gating).
    #[tokio::test]
    async fn test_request_sinks_only_handled_in_request_packet_type() {
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s".to_string(),
            max_volume: Some(65_536),
            ..Default::default()
        }]);
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        let r = plugin
            .handle_packet(
                "phone1",
                Packet::new(
                    "kdeconnect.systemvolume".to_string(),
                    json!({"requestSinks": true}),
                ),
            )
            .await
            .unwrap();
        assert!(r.is_none());
    }

    /// A packet that combines volume + muted (phone UI does both at
    /// once) hits both backend methods.
    #[tokio::test]
    async fn test_volume_and_muted_combined() {
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "s".to_string(),
            max_volume: Some(65_536),
            ..Default::default()
        }]);
        let backend_arc = backend.clone();
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        plugin
            .handle_packet(
                "phone1",
                request_packet(json!({"name": "s", "volume": 1000, "muted": true})),
            )
            .await
            .unwrap();
        let calls = backend_arc.calls();
        let volume_call = calls
            .iter()
            .find(|c| c.starts_with("set_volume:s:"))
            .expect("volume call");
        assert!(volume_call.ends_with(":1000"));
        assert!(calls.iter().any(|c| c == "set_muted:s:false"));
        assert!(calls.iter().any(|c| c == "set_muted:s:true"));
    }

    // ----- Upstream-pinned wire-shape fixtures -----

    /// Fixture: tests/fixtures/upstream-wire/systemvolume/sink_list.json
    ///   The first phone-driven requestSinks over a brand-new connect
    ///   pulls the upstream-published sinkList exactly (pulse.cpp:90-95).
    ///   Field set per entry: name, description, volume, maxVolume, muted,
    ///   enabled — all camelCase, types per the upstream source.
    #[tokio::test]
    async fn test_sink_list_packet_matches_upstream_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/systemvolume/sink_list.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read sink-list fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let upstream_entry = &upstream_body["sinkList"][0];

        let sinks = vec![LocalSinkState {
            name: upstream_entry["name"].as_str().unwrap().to_string(),
            description: upstream_entry["description"].as_str().map(String::from),
            volume: upstream_entry["volume"].as_i64(),
            max_volume: upstream_entry["maxVolume"].as_i64(),
            muted: upstream_entry["muted"].as_bool(),
            enabled: upstream_entry["enabled"].as_bool(),
        }];
        let backend = MockBackend::new()
            .with_sinks(sinks.clone())
            .with_default(&sinks[0].name);
        let plugin = SystemVolumePlugin::new();
        plugin.set_backend(backend).await;
        plugin.disable_watcher_for_test();
        let reply = plugin
            .handle_packet("phone1", request_packet(json!({"requestSinks": true})))
            .await
            .unwrap()
            .expect("reply");
        let list = reply[0]
            .body
            .get("sinkList")
            .expect("sinkList")
            .as_array()
            .unwrap();
        // Field-for-field equality against the upstream-derived entry.
        assert_eq!(list[0], *upstream_entry);
    }
}

//! MPRIS plugin - Media Player Remote Interfacing Specification
//!
//! Two protocol roles, both real:
//!
//! **Control role (this daemon = the player host).** We discover
//! `org.mpris.MediaPlayer2.*` players on the session D-Bus, publish the player
//! list + now-playing state as `kdeconnect.mpris`, and honor
//! `kdeconnect.mpris.request` commands against the live bus. The wire oracle
//! is kdeconnect-kde's mpriscontrol plugin (`/tmp/kdeconnect-kde
//! plugins/mpriscontrol/mpriscontrolplugin.cpp`, cited below by line), with
//! GSConnect's mpris plugin (`~/gsconnect-analysis src/service/plugins/mpris.js`)
//! and kdeconnect-android's MprisPlugin as cross-checks. There is NO upstream
//! "current player" concept on the desktop side: kdeconnect-kde tracks ALL
//! players and keys every packet by display name; the phone picks the target
//! (mpriscontrolplugin.cpp:68,116-119,186; kdeconnect-android MprisPlugin.kt
//! sendCommand :230-253 always carries "player"). playerctld is explicitly
//! ignored as a proxy, not treated as current (mpriscontrolplugin.cpp:59-61).
//!
//! Wire shapes (control role):
//! - Player list: `{"playerList": [...], "supportAlbumArtPayload": bool}`
//!   (mpriscontrolplugin.cpp:387-394). We send `false` for
//!   supportAlbumArtPayload because we don't implement payload transfer —
//!   upstream sends `true` and then honors `albumArtUrl` requests with a file
//!   payload (mpriscontrolplugin.cpp:217-253). Sending `true` without honoring
//!   requests would be capability-dishonest; Android gates art requests on
//!   this flag.
//! - Spontaneous updates on PropertiesChanged carry ONLY the changed fields,
//!   plus `player`, always `canSeek`, and `pos` when seekable
//!   (mpriscontrolplugin.cpp:137-195). No throttling upstream, and `pos` is
//!   never streamed on its own — only attached to other changes or to the
//!   Seeked signal (mpriscontrolplugin.cpp:101-120).
//! - `Seek` is MICROSECONDS on the wire and passed through to MPRIS Seek
//!   unchanged (mpriscontrolplugin.cpp:303-307; Android's default seek
//!   interval is 10000000µs = 10s, MprisNowPlayingFragment.kt:77-81 +
//!   res/values/strings.xml:277). `SetPosition` is absolute MILLISECONDS,
//!   converted ×1000 (mpriscontrolplugin.cpp:309-314; gsconnect
//!   mpris.js:246-259).
//!
//! **Remote role (phone = the player host).** The phone's players arrive as
//! `kdeconnect.mpris` updates; we store them per device and answer with
//! `kdeconnect.mpris.request` now-playing/volume pulls — the pre-existing
//! behavior, unchanged.
//!
//! Capability honesty: both packet types in both directions, matching
//! kdeconnect-kde which ships mpriscontrol (outgoing `kdeconnect.mpris`,
//! incoming `kdeconnect.mpris.request` — kdeconnect_mpriscontrol.json) and
//! mprisremote (the mirror — kdeconnect_mprisremote.json).
//!
//! Degradation (mousepad.rs/clipboard.rs pattern): if the session bus is
//! unreachable the plugin logs `mpris_backend_unavailable`, keeps
//! advertising, and the control role degrades to answering playerList with an
//! empty list; the remote role is unaffected. The backend is enabled ONLY at
//! the production entry point (bootstrap.rs `create_state`); AppState::new,
//! which the test suite exercises, never touches the session bus.

pub mod zbus_backend;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};

use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// Well-known bus-name prefix every MPRIS2 player registers under.
pub(crate) const MPRIS_SERVICE_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Transport actions the phone can legitimately send. GSConnect whitelists
/// exactly these (gsconnect src/service/plugins/mpris.js:217-231);
/// kdeconnect-kde passes any string through to the bus
/// (mpriscontrolplugin.cpp:282-287) with a TODO to validate — we validate.
const ALLOWED_ACTIONS: [&str; 6] = ["PlayPause", "Play", "Pause", "Next", "Previous", "Stop"];

// =====================================================================
// Remote role (phone is the player host) — pre-existing, unchanged.
// =====================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MprisInfo {
    #[serde(default)]
    pub player: Option<String>,
    #[serde(default)]
    pub player_list: Option<Vec<String>>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    #[serde(rename = "length")]
    pub length: Option<i64>,
    #[serde(default)]
    #[serde(rename = "pos")]
    pub position: Option<i64>,
    #[serde(default)]
    pub can_play: bool,
    #[serde(default)]
    pub can_go_next: bool,
    #[serde(default)]
    pub can_go_previous: bool,
    #[serde(default)]
    pub is_playing: bool,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub can_seek: Option<bool>,
    #[serde(default)]
    pub loop_status: Option<String>,
    #[serde(default)]
    pub shuffle: Option<bool>,
}

// =====================================================================
// Control role (this daemon is the player host)
// =====================================================================

/// Snapshot of one local session MPRIS player, keyed on the wire by `name`.
/// Field-for-field the state kdeconnect-kde reads out of its MPRIS proxies
/// (mpriscontrolplugin.cpp:317-358, 396-425).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct LocalPlayerState {
    /// Bus service name, e.g. `org.mpris.MediaPlayer2.brave.instance9654`.
    pub service: String,
    /// Display name the phone sees (MPRIS Identity, deduped) — see
    /// [`display_name_for`].
    pub name: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_art_url: String,
    pub url: String,
    /// Track length in ms; -1 when the player reports none
    /// (mpriscontrolplugin.cpp:418-423).
    pub length_ms: i64,
    /// Last known position in ms (MPRIS Position is µs; /1000 —
    /// mpriscontrolplugin.cpp:117,192,324).
    pub position_ms: i64,
    pub is_playing: bool,
    /// 0-100 integer (MPRIS Volume 0.0-1.0 ×100, truncated —
    /// mpriscontrolplugin.cpp:140,350).
    pub volume: i64,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
    /// Optional on the bus; only sent when the player actually exposes the
    /// property (mpriscontrolplugin.cpp:335-345).
    pub loop_status: Option<String>,
    pub shuffle: Option<bool>,
}

/// Which properties one PropertiesChanged signal carried. Mirrors the
/// `properties.contains(...)` checks in mpriscontrolplugin.cpp:139-183.
#[derive(Debug, Clone, Default)]
pub struct PlayerPropsChanged {
    /// Already converted to the 0-100 wire integer, deduped against the last
    /// value sent (mpriscontrolplugin.cpp:139-146).
    pub volume: Option<i64>,
    /// The Metadata key changed; fresh metadata lives in the accompanying
    /// [`LocalPlayerState`].
    pub metadata: bool,
    /// Already converted to `isPlaying` (== "Playing",
    /// mpriscontrolplugin.cpp:155-159).
    pub playback_status: Option<bool>,
    pub loop_status: Option<String>,
    pub shuffle: Option<bool>,
    pub can_pause: Option<bool>,
    pub can_play: Option<bool>,
    pub can_go_next: Option<bool>,
    pub can_go_previous: Option<bool>,
    /// CanSeek changes are forwarded even though kdeconnect-kde does not
    /// (:137-195 never checks CanSeek) — GSConnect does (full state on any
    /// notify, mpris.js:348-359), and a stale canSeek leaves the phone with
    /// wrong scrub/seek affordances.
    pub can_seek: Option<bool>,
}

/// Events the backend pushes to the plugin's fan-out task.
#[derive(Debug)]
pub enum MprisBackendEvent {
    /// A player appeared on the bus (fresh snapshot included).
    PlayerAdded(LocalPlayerState),
    /// A player's bus name vanished (mpriscontrolplugin.cpp:63-66).
    PlayerRemoved { service: String },
    /// org.freedesktop.DBus.Properties.PropertiesChanged on the Player
    /// interface; `state` is the fresh full snapshot, `changed` says which
    /// keys the signal carried (mpriscontrolplugin.cpp:122-196).
    PropertiesChanged {
        state: LocalPlayerState,
        changed: PlayerPropsChanged,
    },
    /// MPRIS Seeked signal; position in MICROSECONDS per the MPRIS spec.
    Seeked { service: String, position_us: i64 },
}

/// Session MPRIS abstraction so unit tests don't need a live bus. The zbus
/// implementation lives in [`zbus_backend`]; tests use a recording mock.
#[async_trait::async_trait]
pub trait MprisBackend: Send + Sync {
    /// Backend name for logs ("zbus").
    fn name(&self) -> &str;

    /// Fresh full snapshot of one player by display name, None when the
    /// player is gone.
    async fn player_state(&self, display_name: &str) -> Option<LocalPlayerState>;

    /// Relay a transport action. Only [`ALLOWED_ACTIONS`] reach this.
    async fn transport(&self, display_name: &str, action: &str) -> Result<()>;

    async fn set_loop_status(&self, display_name: &str, loop_status: &str) -> Result<()>;

    async fn set_shuffle(&self, display_name: &str, shuffle: bool) -> Result<()>;

    /// `volume` is the 0-100 wire integer; the backend scales to 0.0-1.0
    /// (mpriscontrolplugin.cpp:298-301).
    async fn set_volume(&self, display_name: &str, volume: i64) -> Result<()>;

    /// `offset_us` is MICROSECONDS, passed through to MPRIS Seek unchanged
    /// (mpriscontrolplugin.cpp:303-307).
    async fn seek(&self, display_name: &str, offset_us: i64) -> Result<()>;

    /// `position_ms` is the absolute SetPosition wire value (ms).
    async fn set_position(&self, display_name: &str, position_ms: i64) -> Result<()>;

    /// Start watching the session; events flow into `tx`. Returns Err when
    /// the watch cannot start (no runtime in scope, etc.).
    fn start_watching(&self, tx: UnboundedSender<MprisBackendEvent>) -> Result<()>;
}

/// Services upstream never exposes as players
/// (mpriscontrolplugin.cpp:55-61): `org.mpris.MediaPlayer2.kdeconnect.*` is
/// what the mprisREMOTE plugin exports for phone players (loop prevention),
/// and playerctld is only a proxy to other players.
pub(crate) fn is_ignored_service(service: &str) -> bool {
    service.starts_with("org.mpris.MediaPlayer2.kdeconnect.")
        || service == "org.mpris.MediaPlayer2.playerctld"
}

/// Display-name derivation (mpriscontrolplugin.cpp:74-88): the MPRIS
/// Identity property; when empty, the service name with the
/// `org.mpris.MediaPlayer2.` prefix stripped; duplicates suffixed ` [2]`,
/// ` [3]`, ...
pub(crate) fn display_name_for(identity: &str, service: &str, taken: &HashSet<String>) -> String {
    let base = if identity.is_empty() {
        // kde: service.mid(sizeof("org.mpris.MediaPlayer2")) — sizeof includes
        // the NUL, so the trailing dot is stripped too (:81-83).
        service
            .strip_prefix(MPRIS_SERVICE_PREFIX)
            .unwrap_or(service)
            .to_string()
    } else {
        identity.to_string()
    };
    let mut name = base.clone();
    let mut i = 2;
    while taken.contains(&name) {
        name = format!("{base} [{i}]");
        i += 1;
    }
    name
}

/// Player-list filtering (mpriscontrolplugin.cpp:361-385): when the Plasma
/// browser-integration extension publishes a player for a browser, the raw
/// browser player is hidden from the list.
pub(crate) fn filtered_player_names(players: &HashMap<String, LocalPlayerState>) -> Vec<String> {
    let has_plasma_firefox = players.values().any(|p| {
        p.service
            .starts_with("org.mpris.MediaPlayer2.plasma-browser-integration")
            && p.name.contains("Firefox")
    });
    let has_plasma_chromium = players.values().any(|p| {
        p.service
            .starts_with("org.mpris.MediaPlayer2.plasma-browser-integration")
            && (p.name.contains("Chrome") || p.name.contains("Chromium"))
    });
    let mut names: Vec<String> = players
        .values()
        .filter(|p| {
            !((has_plasma_firefox && p.service.starts_with("org.mpris.MediaPlayer2.firefox"))
                || (has_plasma_chromium
                    && p.service.starts_with("org.mpris.MediaPlayer2.chromium")))
        })
        .map(|p| p.name.clone())
        .collect();
    // kde's QHash order is arbitrary; sort for a stable wire shape.
    names.sort();
    names
}

/// Title/album fallback for local files (mpriscontrolplugin.cpp:404-411):
/// when both title and artist are empty and the URL is a local file, the
/// title becomes the filename and an empty album becomes the parent
/// directory name.
pub(crate) fn local_file_fallback(
    title: &str,
    artist: &str,
    album: &str,
    url: &str,
) -> (String, String) {
    let mut title = title.to_string();
    let mut album = album.to_string();
    if title.is_empty() && artist.is_empty() {
        // QUrl::isLocalFile — file:// URL or a bare absolute path.
        let path = url.strip_prefix("file://").or(if url.starts_with('/') {
            Some(url)
        } else {
            None
        });
        if let Some(path) = path {
            let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            if let Some(file) = parts.last() {
                title = (*file).to_string();
                if album.is_empty() && parts.len() > 1 {
                    album = parts[parts.len() - 2].to_string();
                }
            }
        }
    }
    (title, album)
}

/// Control-role decision core: local player cache + packet construction.
/// Pure/sync so unit tests exercise the exact wire shapes without a bus.
pub(crate) struct MprisCore {
    /// Local session players keyed by display name.
    local_players: StdRwLock<HashMap<String, LocalPlayerState>>,
}

impl MprisCore {
    fn read_players(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, LocalPlayerState>> {
        self.local_players.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_players(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, LocalPlayerState>> {
        self.local_players
            .write()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn cached_state(&self, display_name: &str) -> Option<LocalPlayerState> {
        self.read_players().get(display_name).cloned()
    }

    fn has_player(&self, display_name: &str) -> bool {
        self.read_players().contains_key(display_name)
    }

    /// `{"playerList": [...], "supportAlbumArtPayload": false}` —
    /// mpriscontrolplugin.cpp:387-394 for the shape; `false` because we don't
    /// transfer art payloads (module docs).
    pub(crate) fn player_list_packet(&self) -> Packet {
        let names = filtered_player_names(&self.read_players());
        Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "playerList": names,
                "supportAlbumArtPayload": false,
            }),
        )
    }

    /// Player appeared: cache it, resend the list
    /// (mpriscontrolplugin.cpp:92,98).
    pub(crate) fn apply_player_added(&self, state: LocalPlayerState) -> Packet {
        self.write_players().insert(state.name.clone(), state);
        self.player_list_packet()
    }

    /// Player vanished: drop it, resend the list
    /// (mpriscontrolplugin.cpp:198-215). None when the service wasn't ours.
    pub(crate) fn apply_player_removed(&self, service: &str) -> Option<Packet> {
        let mut players = self.write_players();
        let name = players
            .values()
            .find(|p| p.service == service)
            .map(|p| p.name.clone())?;
        players.remove(&name);
        drop(players);
        Some(self.player_list_packet())
    }

    /// PropertiesChanged → partial update packet carrying only the changed
    /// fields (mpriscontrolplugin.cpp:137-195). The cache always tracks the
    /// fresh snapshot; a packet is produced only when at least one
    /// wire-relevant property changed.
    pub(crate) fn apply_props_changed(
        &self,
        state: LocalPlayerState,
        changed: &PlayerPropsChanged,
    ) -> Option<Packet> {
        let mut body = serde_json::Map::new();
        let mut something_to_send = false;

        if let Some(volume) = changed.volume {
            body.insert("volume".to_string(), volume.into());
            something_to_send = true;
        }
        if changed.metadata {
            // mprisPlayerMetadataToNetworkPacket, mpriscontrolplugin.cpp:396-425
            body.insert("title".to_string(), state.title.clone().into());
            body.insert("artist".to_string(), state.artist.clone().into());
            body.insert("album".to_string(), state.album.clone().into());
            body.insert(
                "albumArtUrl".to_string(),
                state.album_art_url.clone().into(),
            );
            body.insert("url".to_string(), state.url.clone().into());
            body.insert("length".to_string(), state.length_ms.into());
            something_to_send = true;
        }
        if let Some(is_playing) = changed.playback_status {
            body.insert("isPlaying".to_string(), is_playing.into());
            something_to_send = true;
        }
        if let Some(loop_status) = &changed.loop_status {
            body.insert("loopStatus".to_string(), loop_status.clone().into());
            something_to_send = true;
        }
        if let Some(shuffle) = changed.shuffle {
            body.insert("shuffle".to_string(), shuffle.into());
            something_to_send = true;
        }
        if let Some(can_pause) = changed.can_pause {
            body.insert("canPause".to_string(), can_pause.into());
            something_to_send = true;
        }
        if let Some(can_play) = changed.can_play {
            body.insert("canPlay".to_string(), can_play.into());
            something_to_send = true;
        }
        if let Some(can_go_next) = changed.can_go_next {
            body.insert("canGoNext".to_string(), can_go_next.into());
            something_to_send = true;
        }
        if let Some(can_go_previous) = changed.can_go_previous {
            body.insert("canGoPrevious".to_string(), can_go_previous.into());
            something_to_send = true;
        }
        if changed.can_seek.is_some() {
            // The value itself is written below from the fresh snapshot (the
            // tail always attaches canSeek); the flag is what matters here.
            something_to_send = true;
        }

        let name = state.name.clone();
        let can_seek = state.can_seek;
        let position_ms = state.position_ms;
        self.write_players().insert(name.clone(), state);

        if !something_to_send {
            return None;
        }

        // kde:186-193 — every update also carries the player name and canSeek,
        // and the position when seekable.
        body.insert("player".to_string(), name.into());
        body.insert("canSeek".to_string(), can_seek.into());
        if can_seek {
            body.insert("pos".to_string(), position_ms.into());
        }
        Some(Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::Value::Object(body),
        ))
    }

    /// Seeked signal → `{"pos": <ms>, "player": <name>}`
    /// (mpriscontrolplugin.cpp:101-120; GSConnect sends the same two fields,
    /// mpris.js:361-372). None when the service isn't tracked.
    pub(crate) fn apply_seeked(&self, service: &str, position_us: i64) -> Option<Packet> {
        let mut players = self.write_players();
        let player = players.values_mut().find(|p| p.service == service)?;
        player.position_ms = position_us / 1000; // µs→ms, mpriscontrolplugin.cpp:117
        Some(Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "pos": player.position_ms,
                "player": player.name,
            }),
        ))
    }

    /// Full now-playing answer to requestNowPlaying/requestVolume
    /// (mpriscontrolplugin.cpp:317-358). `pos` is unconditional here (kde
    /// :323-324); loopStatus/shuffle only when the player exposes them
    /// (:335-345); volume only when requested (:349-353).
    pub(crate) fn now_playing_answer(
        &self,
        state: &LocalPlayerState,
        include_volume: bool,
    ) -> Packet {
        let mut body = serde_json::json!({
            "player": state.name,
            "title": state.title,
            "artist": state.artist,
            "album": state.album,
            "albumArtUrl": state.album_art_url,
            "url": state.url,
            "length": state.length_ms,
            "pos": state.position_ms,
            "isPlaying": state.is_playing,
            "canPause": state.can_pause,
            "canPlay": state.can_play,
            "canGoNext": state.can_go_next,
            "canGoPrevious": state.can_go_previous,
            "canSeek": state.can_seek,
        });
        if let Some(loop_status) = &state.loop_status {
            body["loopStatus"] = loop_status.clone().into();
        }
        if let Some(shuffle) = state.shuffle {
            body["shuffle"] = shuffle.into();
        }
        if include_volume {
            body["volume"] = state.volume.into();
        }
        Packet::new("kdeconnect.mpris".to_string(), body)
    }
}

// =====================================================================
// Plugin
// =====================================================================

pub struct MprisPlugin {
    /// Remote-role store: phone players per device (pre-existing).
    players: Arc<StdRwLock<HashMap<String, Vec<MprisInfo>>>>,
    plugin_events: Arc<PluginEventBroadcaster>,
    /// Control-role core (local session players).
    core: Arc<MprisCore>,
    backend: StdRwLock<Option<Arc<dyn MprisBackend>>>,
    connection_manager: Option<Arc<crate::protocol::ConnectionManager>>,
    watcher_started: AtomicBool,
}

impl MprisPlugin {
    pub fn new(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            players: Arc::new(StdRwLock::new(HashMap::new())),
            plugin_events,
            core: Arc::new(MprisCore {
                local_players: StdRwLock::new(HashMap::new()),
            }),
            backend: StdRwLock::new(None),
            connection_manager: None,
            watcher_started: AtomicBool::new(false),
        }
    }

    /// Wire the daemon's connection manager — the fan-out path for local
    /// player updates, mirroring clipboard.rs's with_connection_manager.
    pub fn with_connection_manager(
        mut self,
        connection_manager: Arc<crate::protocol::ConnectionManager>,
    ) -> Self {
        self.connection_manager = Some(connection_manager);
        self.try_start_watcher();
        self
    }

    /// Inject a session MPRIS backend (unit tests use a mock).
    pub fn set_backend(&self, backend: Arc<dyn MprisBackend>) {
        {
            let mut guard = self.backend.write().unwrap_or_else(|e| e.into_inner());
            *guard = Some(backend);
        }
        self.try_start_watcher();
    }

    /// Connect the real session-bus backend. Called ONLY from the production
    /// entry point (bootstrap.rs create_state) — never from AppState::new,
    /// which the test suite exercises against the developer's live session.
    /// Degrades with a log event, mousepad/clipboard-style, when no session
    /// bus is reachable.
    pub async fn enable_session_backend(&self) {
        match zbus_backend::ZbusMprisBackend::connect().await {
            Ok(backend) => {
                info!(
                    event = "mpris_backend_ready",
                    backend = backend.name(),
                    "Session MPRIS backend enabled"
                );
                self.set_backend(Arc::new(backend));
            }
            Err(e) => {
                warn!(
                    error = %e,
                    event = "mpris_backend_unavailable",
                    "No session D-Bus for MPRIS. Control role degraded to empty player list; \
                     remote (phone-as-player) role unaffected."
                );
            }
        }
    }

    fn backend(&self) -> Option<Arc<dyn MprisBackend>> {
        self.backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Start the backend event fan-out task once both the backend and the
    /// connection manager are present. Idempotent (clipboard.rs pattern).
    fn try_start_watcher(&self) {
        if self.watcher_started.load(Ordering::SeqCst) {
            return;
        }
        let (Some(cm), Some(backend)) = (self.connection_manager.clone(), self.backend()) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            warn!(
                event = "mpris_watcher_no_runtime",
                "No tokio runtime in scope; MPRIS watcher not started"
            );
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MprisBackendEvent>();
        if let Err(e) = backend.start_watching(tx) {
            // Do NOT mark started: a failed start must stay retryable (the
            // next set_backend/with_connection_manager tries again).
            warn!(error = %e, event = "mpris_watcher_unavailable", "Could not start MPRIS watcher");
            return;
        }
        self.watcher_started.store(true, Ordering::SeqCst);

        let core = self.core.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let packet = match event {
                    MprisBackendEvent::PlayerAdded(state) => {
                        info!(
                            player = %state.name,
                            service = %state.service,
                            event = "mpris_player_added",
                            "Local MPRIS player appeared"
                        );
                        Some(core.apply_player_added(state))
                    }
                    MprisBackendEvent::PlayerRemoved { service } => {
                        info!(
                            service = %service,
                            event = "mpris_player_removed",
                            "Local MPRIS player vanished"
                        );
                        core.apply_player_removed(&service)
                    }
                    MprisBackendEvent::PropertiesChanged { state, changed } => {
                        core.apply_props_changed(state, &changed)
                    }
                    MprisBackendEvent::Seeked {
                        service,
                        position_us,
                    } => core.apply_seeked(&service, position_us),
                };
                let Some(packet) = packet else { continue };
                // Fan out to every connected device — same recipient set the
                // clipboard watcher uses (clipboard.rs try_start_watcher).
                for device_id in cm.connected_device_ids().await {
                    if let Err(e) = cm.send_packet(&device_id, &packet).await {
                        warn!(
                            device_id = %device_id,
                            error = %e,
                            event = "mpris_send_failed",
                            "Failed to send MPRIS packet"
                        );
                    }
                }
            }
        });
    }

    #[allow(clippy::expect_used)]
    pub fn get_players(&self, device_id: &str) -> Vec<MprisInfo> {
        let players = self.players.read().unwrap_or_else(|e| e.into_inner());
        players.get(device_id).cloned().unwrap_or_default()
    }

    #[allow(clippy::expect_used)]
    pub fn all_players(&self) -> HashMap<String, Vec<MprisInfo>> {
        self.players
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Local (control-role) players by display name — for the REST API.
    pub fn local_players(&self) -> Vec<LocalPlayerState> {
        self.core.read_players().values().cloned().collect()
    }

    // -----------------------------------------------------------------
    // Remote role: phone is the player host (pre-existing logic).
    // -----------------------------------------------------------------

    #[allow(clippy::expect_used)]
    async fn handle_remote_update(
        &self,
        device_id: &str,
        packet: Packet,
    ) -> Result<Option<Vec<Packet>>> {
        let info: MprisInfo = packet.body_as("mpris")?;

        info!(
            device_id = %device_id,
            player = ?info.player,
            player_list = ?info.player_list,
            title = ?info.title,
            artist = ?info.artist,
            playing = info.is_playing,
            event = "mpris_update",
            "Received MPRIS update"
        );

        let mut responses = Vec::new();

        if let Ok(mut players) = self.players.write() {
            // Handle player list update
            if let Some(list) = &info.player_list {
                let device_players = players.entry(device_id.to_string()).or_default();

                // Remove players not in the new list
                device_players.retain(|p| {
                    if let Some(name) = &p.player {
                        list.contains(name)
                    } else {
                        false
                    }
                });

                // For any player name we don't have details for, request details
                for player_name in list {
                    if !device_players
                        .iter()
                        .any(|p| p.player.as_deref() == Some(player_name.as_str()))
                    {
                        responses.push(Packet::new(
                            "kdeconnect.mpris.request".to_string(),
                            serde_json::json!({
                                "player": player_name,
                                "requestNowPlaying": true,
                                "requestVolume": true,
                            }),
                        ));
                    }
                }
            }

            // Handle individual player update
            if let Some(player_name) = &info.player {
                let list = players.entry(device_id.to_string()).or_default();
                if let Some(pos) = list
                    .iter()
                    .position(|p| p.player.as_deref() == Some(player_name.as_str()))
                {
                    list[pos] = info.clone();
                } else {
                    list.push(info.clone());
                }
            }
        }

        self.plugin_events.broadcast(PluginEvent::MprisUpdate {
            device_id: device_id.to_string(),
            info,
        });

        if responses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(responses))
        }
    }

    // -----------------------------------------------------------------
    // Control role: honor kdeconnect.mpris.request against the live bus.
    // Branch-for-branch oracle: kdeconnect-kde mpriscontrolplugin.cpp:255-358.
    // -----------------------------------------------------------------

    async fn handle_control_request(
        &self,
        device_id: &str,
        packet: Packet,
    ) -> Result<Option<Vec<Packet>>> {
        let body = &packet.body;

        // A packet carrying playerList is an mpris CLIENT talking, not a
        // control peer (mpriscontrolplugin.cpp:257-259).
        if body.get("playerList").is_some() {
            return Ok(None);
        }

        // Album-art payload requests: we advertise supportAlbumArtPayload=false
        // in every player list, so a well-behaved phone never sends this
        // (module docs; mpriscontrolplugin.cpp:261-264 is the upstream branch).
        if body.get("albumArtUrl").is_some() {
            debug!(
                device_id = %device_id,
                event = "mpris_album_art_unsupported",
                "Album art payload requested but not supported — ignored"
            );
            return Ok(None);
        }

        let player = body.get("player").and_then(|v| v.as_str()).unwrap_or("");
        let known_player = !player.is_empty() && self.core.has_player(player);
        let wants_player_list = body
            .get("requestPlayerList")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut responses = Vec::new();

        // Send the player list when asked or when the named player is unknown
        // (mpriscontrolplugin.cpp:266-275).
        if !known_player || wants_player_list {
            responses.push(self.core.player_list_packet());
            if !known_player {
                return Ok(Some(responses));
            }
        }

        // Commands against the live bus.
        if let Some(backend) = self.backend() {
            if let Some(action) = body.get("action").and_then(|v| v.as_str()) {
                if ALLOWED_ACTIONS.contains(&action) {
                    if let Err(e) = backend.transport(player, action).await {
                        warn!(error = %e, player = %player, action = %action, event = "mpris_action_failed", "MPRIS action failed");
                    }
                } else {
                    // GSConnect logs and drops unknown actions (mpris.js:228-230).
                    debug!(action = %action, event = "mpris_unknown_action", "Ignoring unknown MPRIS action");
                }
            }
            if let Some(loop_status) = body.get("setLoopStatus").and_then(|v| v.as_str()) {
                if let Err(e) = backend.set_loop_status(player, loop_status).await {
                    warn!(error = %e, player = %player, event = "mpris_set_loop_failed", "setLoopStatus failed");
                }
            }
            if let Some(shuffle) = body.get("setShuffle").and_then(|v| v.as_bool()) {
                if let Err(e) = backend.set_shuffle(player, shuffle).await {
                    warn!(error = %e, player = %player, event = "mpris_set_shuffle_failed", "setShuffle failed");
                }
            }
            if let Some(volume) = body.get("setVolume").and_then(|v| v.as_i64()) {
                // 0-100 wire int; backend scales to 0.0-1.0
                // (mpriscontrolplugin.cpp:298-301).
                if let Err(e) = backend.set_volume(player, volume).await {
                    warn!(error = %e, player = %player, event = "mpris_set_volume_failed", "setVolume failed");
                }
            }
            if let Some(offset) = body.get("Seek").and_then(|v| v.as_i64()) {
                // MICROSECONDS, passed through unchanged
                // (mpriscontrolplugin.cpp:303-307).
                if let Err(e) = backend.seek(player, offset).await {
                    warn!(error = %e, player = %player, event = "mpris_seek_failed", "Seek failed");
                }
            }
            if let Some(position) = body.get("SetPosition").and_then(|v| v.as_i64()) {
                // Absolute position in ms (mpriscontrolplugin.cpp:309-314).
                if let Err(e) = backend.set_position(player, position).await {
                    warn!(error = %e, player = %player, event = "mpris_set_position_failed", "SetPosition failed");
                }
            }
        } else if body.get("action").is_some()
            || body.get("Seek").is_some()
            || body.get("SetPosition").is_some()
            || body.get("setVolume").is_some()
            || body.get("setLoopStatus").is_some()
            || body.get("setShuffle").is_some()
        {
            debug!(
                device_id = %device_id,
                event = "mpris_no_backend",
                "No session MPRIS backend; control command dropped"
            );
        }

        // Information requests (mpriscontrolplugin.cpp:316-358).
        let want_now_playing = body
            .get("requestNowPlaying")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let want_volume = body
            .get("requestVolume")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if want_now_playing || want_volume {
            // Fresh state from the bus, falling back to the event-fed cache.
            let state = match self.backend() {
                Some(backend) => backend
                    .player_state(player)
                    .await
                    .or_else(|| self.core.cached_state(player)),
                None => self.core.cached_state(player),
            };
            if let Some(state) = state {
                responses.push(self.core.now_playing_answer(&state, want_volume));
            }
        }

        if responses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(responses))
        }
    }
}

#[async_trait::async_trait]
impl Plugin for MprisPlugin {
    fn name(&self) -> &str {
        "mpris"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        // kdeconnect.mpris: phone-as-player updates (remote role).
        // kdeconnect.mpris.request: phone controlling OUR players (control
        // role — kdeconnect_mpriscontrol.json X-KdeConnect-SupportedPacketType).
        vec![
            "kdeconnect.mpris".to_string(),
            "kdeconnect.mpris.request".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // kdeconnect.mpris: our player list + now-playing updates (control
        // role — kdeconnect_mpriscontrol.json X-KdeConnect-OutgoingPacketType).
        // kdeconnect.mpris.request: now-playing/volume pulls of phone players
        // (remote role — kdeconnect_mprisremote.json).
        vec![
            "kdeconnect.mpris".to_string(),
            "kdeconnect.mpris.request".to_string(),
        ]
    }

    fn is_backend_available(&self) -> bool {
        self.backend.read().map(|b| b.is_some()).unwrap_or(false)
    }
    fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut players) = self.players.write() {
            players.remove(device_id);
        }
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        // GSConnect's connected() sends BOTH on one connect, request first
        // (gsconnect src/service/plugins/mpris.js:69-74): the remote-role
        // pull ({"requestPlayerList": true} — so the phone tells us about
        // ITS players; the pre-refactor behavior, old mpris.rs:102-107),
        // then our control-role player list.
        let mut packets = vec![Packet::new(
            "kdeconnect.mpris.request".to_string(),
            serde_json::json!({ "requestPlayerList": true }),
        )];
        // Control-role list only when the session backend is live — degraded,
        // we send no list rather than a fabricated empty one.
        if self.backend().is_some() {
            packets.push(self.core.player_list_packet());
        }
        packets
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        match packet.packet_type.as_str() {
            "kdeconnect.mpris" => self.handle_remote_update(device_id, packet).await,
            "kdeconnect.mpris.request" => self.handle_control_request(device_id, packet).await,
            _ => Ok(None),
        }
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::sync::Mutex;

    fn setup() -> (MprisPlugin, Arc<PluginEventBroadcaster>) {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        (MprisPlugin::new(broadcaster.clone()), broadcaster)
    }

    fn sample_state() -> LocalPlayerState {
        LocalPlayerState {
            service: "org.mpris.MediaPlayer2.vlc".to_string(),
            name: "VLC media player".to_string(),
            title: "Test Song".to_string(),
            artist: "Test Artist, Guest".to_string(),
            album: "Test Album".to_string(),
            album_art_url: "file:///tmp/art.png".to_string(),
            url: "file:///music/test.ogg".to_string(),
            length_ms: 180_000,
            position_ms: 60_000,
            is_playing: true,
            volume: 75,
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: false,
            can_seek: true,
            loop_status: Some("None".to_string()),
            shuffle: Some(false),
        }
    }

    // -----------------------------------------------------------------
    // Recording mock backend (no live bus needed)
    // -----------------------------------------------------------------

    struct MockMprisBackend {
        states: Mutex<HashMap<String, LocalPlayerState>>,
        calls: Mutex<Vec<String>>,
        event_tx: Mutex<Option<UnboundedSender<MprisBackendEvent>>>,
        fail_start: AtomicBool,
    }

    impl MockMprisBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                states: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
                event_tx: Mutex::new(None),
                fail_start: AtomicBool::new(false),
            })
        }

        fn with_player(self: &Arc<Self>, state: LocalPlayerState) -> Arc<Self> {
            self.states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(state.name.clone(), state);
            self.clone()
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait::async_trait]
    impl MprisBackend for MockMprisBackend {
        fn name(&self) -> &str {
            "mock"
        }

        async fn player_state(&self, display_name: &str) -> Option<LocalPlayerState> {
            self.states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(display_name)
                .cloned()
        }

        async fn transport(&self, display_name: &str, action: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("transport:{display_name}:{action}"));
            Ok(())
        }

        async fn set_loop_status(&self, display_name: &str, loop_status: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("set_loop_status:{display_name}:{loop_status}"));
            Ok(())
        }

        async fn set_shuffle(&self, display_name: &str, shuffle: bool) -> Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("set_shuffle:{display_name}:{shuffle}"));
            Ok(())
        }

        async fn set_volume(&self, display_name: &str, volume: i64) -> Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("set_volume:{display_name}:{volume}"));
            Ok(())
        }

        async fn seek(&self, display_name: &str, offset_us: i64) -> Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("seek:{display_name}:{offset_us}"));
            Ok(())
        }

        async fn set_position(&self, display_name: &str, position_ms: i64) -> Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("set_position:{display_name}:{position_ms}"));
            Ok(())
        }

        fn start_watching(&self, tx: UnboundedSender<MprisBackendEvent>) -> Result<()> {
            if self.fail_start.load(Ordering::SeqCst) {
                return Err(crate::utils::errors::Error::Internal(
                    "mock start_watching failure".to_string(),
                ));
            }
            *self.event_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
            Ok(())
        }
    }

    fn plugin_with_backend(backend: Arc<MockMprisBackend>) -> MprisPlugin {
        let (plugin, _) = setup();
        plugin.set_backend(backend);
        plugin
    }

    fn request_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.mpris.request".to_string(), body)
    }

    // -----------------------------------------------------------------
    // Plugin identity + capability honesty
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_mpris_plugin_name() {
        let (plugin, _) = setup();
        assert_eq!(plugin.name(), "mpris");
    }

    #[tokio::test]
    async fn test_capabilities_both_types_both_directions() {
        // kdeconnect-kde ships BOTH roles: mpriscontrol (outgoing
        // kdeconnect.mpris, incoming kdeconnect.mpris.request —
        // kdeconnect_mpriscontrol.json) and mprisremote (the mirror —
        // kdeconnect_mprisremote.json). We implement both roles in one
        // plugin, so both packet types appear in both directions.
        let (plugin, _) = setup();
        for caps in [
            plugin.incoming_capabilities(),
            plugin.outgoing_capabilities(),
        ] {
            assert!(caps.contains(&"kdeconnect.mpris".to_string()));
            assert!(caps.contains(&"kdeconnect.mpris.request".to_string()));
        }
    }

    // -----------------------------------------------------------------
    // Pure naming / filtering rules (kdeconnect-kde oracles)
    // -----------------------------------------------------------------

    #[test]
    fn test_display_name_uses_identity() {
        // mpriscontrolplugin.cpp:78-80
        let name = display_name_for(
            "Brave",
            "org.mpris.MediaPlayer2.brave.instance9654",
            &HashSet::new(),
        );
        assert_eq!(name, "Brave");
    }

    #[test]
    fn test_display_name_fallback_strips_prefix() {
        // Empty identity → service minus "org.mpris.MediaPlayer2."
        // (mpriscontrolplugin.cpp:81-83; sizeof includes the NUL, so the dot
        // goes too).
        let name = display_name_for("", "org.mpris.MediaPlayer2.vlc", &HashSet::new());
        assert_eq!(name, "vlc");
    }

    #[test]
    fn test_display_name_dedup() {
        // mpriscontrolplugin.cpp:85-88 — " [2]", " [3]".
        let taken: HashSet<String> = ["VLC".to_string(), "VLC [2]".to_string()]
            .into_iter()
            .collect();
        assert_eq!(
            display_name_for("VLC", "org.mpris.MediaPlayer2.vlc.inst2", &taken),
            "VLC [3]"
        );
    }

    #[test]
    fn test_ignored_services() {
        // mpriscontrolplugin.cpp:55-61
        assert!(is_ignored_service("org.mpris.MediaPlayer2.playerctld"));
        assert!(is_ignored_service(
            "org.mpris.MediaPlayer2.kdeconnect.phone"
        ));
        assert!(!is_ignored_service("org.mpris.MediaPlayer2.vlc"));
    }

    #[test]
    fn test_plasma_browser_integration_filtering() {
        // mpriscontrolplugin.cpp:361-385: with a plasma-browser-integration
        // player for Firefox present, the raw firefox player is hidden.
        let mut players = HashMap::new();
        let mut raw = sample_state();
        raw.service = "org.mpris.MediaPlayer2.firefox".to_string();
        raw.name = "Firefox".to_string();
        let mut plasma = sample_state();
        plasma.service = "org.mpris.MediaPlayer2.plasma-browser-integration".to_string();
        plasma.name = "Mozilla Firefox".to_string();
        players.insert(raw.name.clone(), raw);
        players.insert(plasma.name.clone(), plasma.clone());
        players.insert("VLC media player".to_string(), sample_state());

        let names = filtered_player_names(&players);
        assert_eq!(
            names,
            vec![
                "Mozilla Firefox".to_string(),
                "VLC media player".to_string()
            ]
        );

        // Without the plasma player, the raw one shows.
        players.remove("Mozilla Firefox");
        let names = filtered_player_names(&players);
        assert!(names.contains(&"Firefox".to_string()));
    }

    #[test]
    fn test_local_file_fallback() {
        // mpriscontrolplugin.cpp:404-411
        let (title, album) = local_file_fallback("", "", "", "file:///home/user/Music/song.ogg");
        assert_eq!(title, "song.ogg");
        assert_eq!(album, "Music");

        // Title present → no fallback.
        let (title, album) = local_file_fallback("Real Title", "", "", "file:///x/y.ogg");
        assert_eq!(title, "Real Title");
        assert_eq!(album, "");

        // Remote URL → no fallback.
        let (title, album) = local_file_fallback("", "", "", "https://example.com/stream");
        assert_eq!(title, "");
        assert_eq!(album, "");
    }

    // -----------------------------------------------------------------
    // Wire shapes we SEND (control role) — exact JSON, upstream-cited
    // -----------------------------------------------------------------

    #[test]
    fn test_player_list_wire_shape() {
        // mpriscontrolplugin.cpp:387-394: {"playerList": [...],
        // "supportAlbumArtPayload": ...}. We send false — no payload transfer
        // (module docs).
        let (plugin, _) = setup();
        plugin.core.apply_player_added(sample_state());
        let packet = plugin.core.player_list_packet();
        assert_eq!(packet.packet_type, "kdeconnect.mpris");
        assert_eq!(
            packet.body,
            serde_json::json!({
                "playerList": ["VLC media player"],
                "supportAlbumArtPayload": false,
            })
        );
    }

    #[test]
    fn test_props_changed_partial_update_wire_shape() {
        // PlaybackStatus change: ONLY isPlaying plus player/canSeek/pos
        // (mpriscontrolplugin.cpp:155-159 for isPlaying, :186-193 for the
        // always-attached fields).
        let (plugin, _) = setup();
        plugin.core.apply_player_added(sample_state());

        let state = sample_state();
        let changed = PlayerPropsChanged {
            playback_status: Some(false),
            ..Default::default()
        };
        let packet = plugin
            .core
            .apply_props_changed(state, &changed)
            .expect("changed playback status must produce a packet");
        assert_eq!(packet.packet_type, "kdeconnect.mpris");
        assert_eq!(
            packet.body,
            serde_json::json!({
                "player": "VLC media player",
                "isPlaying": false,
                "canSeek": true,
                "pos": 60000,
            })
        );
    }

    #[test]
    fn test_props_changed_metadata_wire_shape() {
        // Metadata change carries the full metadata mapping
        // (mpriscontrolplugin.cpp:147-154 + :396-425).
        let (plugin, _) = setup();
        let changed = PlayerPropsChanged {
            metadata: true,
            ..Default::default()
        };
        let packet = plugin
            .core
            .apply_props_changed(sample_state(), &changed)
            .expect("metadata change must produce a packet");
        assert_eq!(
            packet.body,
            serde_json::json!({
                "player": "VLC media player",
                "title": "Test Song",
                "artist": "Test Artist, Guest",
                "album": "Test Album",
                "albumArtUrl": "file:///tmp/art.png",
                "url": "file:///music/test.ogg",
                "length": 180000,
                "canSeek": true,
                "pos": 60000,
            })
        );
    }

    #[test]
    fn test_props_changed_volume_wire_shape() {
        // Volume is a 0-100 int (mpriscontrolplugin.cpp:139-146).
        let (plugin, _) = setup();
        let changed = PlayerPropsChanged {
            volume: Some(42),
            ..Default::default()
        };
        let packet = plugin
            .core
            .apply_props_changed(sample_state(), &changed)
            .unwrap();
        assert_eq!(packet.body.get("volume").unwrap(), 42);
    }

    #[test]
    fn test_props_changed_can_seek_only_change_produces_packet() {
        // A CanSeek-only change must reach the phone — otherwise it keeps
        // stale scrub/seek affordances. GSConnect propagates it (full state
        // on any property notify, mpris.js:348-359 + :272-294);
        // kdeconnect-kde does not (:137-195 never checks CanSeek) — we
        // follow GSConnect here.
        let (plugin, _) = setup();
        plugin.core.apply_player_added(sample_state());

        let mut state = sample_state();
        state.can_seek = false;
        let changed = PlayerPropsChanged {
            can_seek: Some(false),
            ..Default::default()
        };
        let packet = plugin
            .core
            .apply_props_changed(state, &changed)
            .expect("a CanSeek change must produce a packet");
        assert_eq!(
            packet.body,
            serde_json::json!({
                "player": "VLC media player",
                "canSeek": false,
            })
        );
    }

    #[test]
    fn test_props_changed_no_pos_when_not_seekable() {
        // pos only rides along when canSeek (mpriscontrolplugin.cpp:188-193).
        let (plugin, _) = setup();
        let mut state = sample_state();
        state.can_seek = false;
        let changed = PlayerPropsChanged {
            playback_status: Some(true),
            ..Default::default()
        };
        let packet = plugin.core.apply_props_changed(state, &changed).unwrap();
        assert_eq!(packet.body.get("canSeek").unwrap(), false);
        assert!(packet.body.get("pos").is_none());
    }

    #[test]
    fn test_props_changed_irrelevant_change_produces_no_packet() {
        // A PropertiesChanged with none of the wire-relevant keys updates the
        // cache but sends nothing (mpriscontrolplugin.cpp:137-138,185).
        let (plugin, _) = setup();
        let packet = plugin
            .core
            .apply_props_changed(sample_state(), &PlayerPropsChanged::default());
        assert!(packet.is_none());
        // ...but the cache has the fresh snapshot.
        assert_eq!(
            plugin.core.cached_state("VLC media player").unwrap().volume,
            75
        );
    }

    #[test]
    fn test_seeked_wire_shape() {
        // {"pos": <ms>, "player": <name>} — mpriscontrolplugin.cpp:116-119;
        // µs→ms (:117). GSConnect sends the same (mpris.js:361-372).
        let (plugin, _) = setup();
        plugin.core.apply_player_added(sample_state());
        let packet = plugin
            .core
            .apply_seeked("org.mpris.MediaPlayer2.vlc", 90_000_000)
            .expect("known service must produce a packet");
        assert_eq!(
            packet.body,
            serde_json::json!({ "pos": 90000, "player": "VLC media player" })
        );
        // Untracked service → nothing.
        assert!(plugin
            .core
            .apply_seeked("org.mpris.MediaPlayer2.gone", 1)
            .is_none());
    }

    #[test]
    fn test_now_playing_answer_wire_shape() {
        // requestNowPlaying + requestVolume answer
        // (mpriscontrolplugin.cpp:317-358).
        let (plugin, _) = setup();
        let packet = plugin.core.now_playing_answer(&sample_state(), true);
        assert_eq!(
            packet.body,
            serde_json::json!({
                "player": "VLC media player",
                "title": "Test Song",
                "artist": "Test Artist, Guest",
                "album": "Test Album",
                "albumArtUrl": "file:///tmp/art.png",
                "url": "file:///music/test.ogg",
                "length": 180000,
                "pos": 60000,
                "isPlaying": true,
                "canPause": true,
                "canPlay": true,
                "canGoNext": true,
                "canGoPrevious": false,
                "canSeek": true,
                "loopStatus": "None",
                "shuffle": false,
                "volume": 75,
            })
        );
    }

    #[test]
    fn test_now_playing_answer_omits_optional_fields() {
        // loopStatus/shuffle only when the player exposes them
        // (mpriscontrolplugin.cpp:335-345); volume only when requested
        // (:349-353).
        let (plugin, _) = setup();
        let mut state = sample_state();
        state.loop_status = None;
        state.shuffle = None;
        let packet = plugin.core.now_playing_answer(&state, false);
        assert!(packet.body.get("loopStatus").is_none());
        assert!(packet.body.get("shuffle").is_none());
        assert!(packet.body.get("volume").is_none());
        // pos is unconditional in the requestNowPlaying answer (:323-324).
        assert_eq!(packet.body.get("pos").unwrap(), 60000);
    }

    // -----------------------------------------------------------------
    // Wire shapes we RECEIVE (control role) — exact phone requests
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_request_player_list_without_player() {
        // Android onCreate sends exactly {"requestPlayerList": true}
        // (MprisPlugin.kt:459-463) — no "player" key. kde answers with the
        // list and returns (mpriscontrolplugin.cpp:266-275).
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend);
        plugin.core.apply_player_added(sample_state());

        let responses = plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({ "requestPlayerList": true })),
            )
            .await
            .unwrap()
            .expect("player list response expected");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].packet_type, "kdeconnect.mpris");
        assert_eq!(
            responses[0].body,
            serde_json::json!({
                "playerList": ["VLC media player"],
                "supportAlbumArtPayload": false,
            })
        );
    }

    #[tokio::test]
    async fn test_request_unknown_player_gets_list_only() {
        // mpriscontrolplugin.cpp:270-275.
        let backend = MockMprisBackend::new();
        let plugin = plugin_with_backend(backend.clone());
        let responses = plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "NoSuchPlayer",
                    "requestNowPlaying": true,
                    "action": "PlayPause"
                })),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert!(responses[0].body.get("playerList").is_some());
        // No command may reach the bus for an unknown player.
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn test_request_now_playing_answer_from_bus() {
        // Android's per-player pull (MprisPlugin.kt:466-472):
        // {"player": ..., "requestNowPlaying": true, "requestVolume": true}.
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend);
        plugin.core.apply_player_added(sample_state());

        let responses = plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "requestNowPlaying": true,
                    "requestVolume": true
                })),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].body,
            plugin.core.now_playing_answer(&sample_state(), true).body
        );
    }

    #[tokio::test]
    async fn test_request_actions_relayed_to_bus() {
        // The 6 actions Android sends (MprisPlugin.kt:125-153).
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend.clone());
        plugin.core.apply_player_added(sample_state());

        for action in ["PlayPause", "Play", "Pause", "Next", "Previous", "Stop"] {
            plugin
                .handle_packet(
                    "phone",
                    request_packet(serde_json::json!({
                        "player": "VLC media player",
                        "action": action
                    })),
                )
                .await
                .unwrap();
        }
        let calls = backend.calls();
        assert_eq!(calls.len(), 6);
        assert!(calls.contains(&"transport:VLC media player:PlayPause".to_string()));
        assert!(calls.contains(&"transport:VLC media player:Stop".to_string()));
    }

    #[tokio::test]
    async fn test_request_unknown_action_dropped() {
        // GSConnect whitelists and drops the rest (mpris.js:217-231); kde
        // passes anything through with a TODO to validate
        // (mpriscontrolplugin.cpp:282-287) — we validate.
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend.clone());
        plugin.core.apply_player_added(sample_state());

        plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "action": "Quit"
                })),
            )
            .await
            .unwrap();
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn test_request_seek_is_microseconds_passthrough() {
        // Android's default seek button sends ±10000000 (10s in µs —
        // MprisNowPlayingFragment.kt:77-81,100-104 + strings.xml:277); kde
        // passes it to MPRIS Seek unchanged (mpriscontrolplugin.cpp:303-307).
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend.clone());
        plugin.core.apply_player_added(sample_state());

        plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "Seek": -10000000
                })),
            )
            .await
            .unwrap();
        assert_eq!(
            backend.calls(),
            vec!["seek:VLC media player:-10000000".to_string()]
        );
    }

    #[tokio::test]
    async fn test_request_set_position_is_milliseconds() {
        // SetPosition is absolute ms on the wire
        // (mpriscontrolplugin.cpp:309-314; gsconnect mpris.js:246-259).
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend.clone());
        plugin.core.apply_player_added(sample_state());

        plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "SetPosition": 95000
                })),
            )
            .await
            .unwrap();
        assert_eq!(
            backend.calls(),
            vec!["set_position:VLC media player:95000".to_string()]
        );
    }

    #[tokio::test]
    async fn test_request_set_volume_is_0_to_100_int() {
        // mpriscontrolplugin.cpp:298-301 (int /100.f); Android sends the
        // SeekBar progress int (MprisPlugin.kt:166-168).
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend.clone());
        plugin.core.apply_player_added(sample_state());

        plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "setVolume": 30
                })),
            )
            .await
            .unwrap();
        assert_eq!(
            backend.calls(),
            vec!["set_volume:VLC media player:30".to_string()]
        );
    }

    #[tokio::test]
    async fn test_request_loop_and_shuffle_relayed() {
        // mpriscontrolplugin.cpp:288-297.
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend.clone());
        plugin.core.apply_player_added(sample_state());

        plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "setLoopStatus": "Track",
                    "setShuffle": true
                })),
            )
            .await
            .unwrap();
        let calls = backend.calls();
        assert!(calls.contains(&"set_loop_status:VLC media player:Track".to_string()));
        assert!(calls.contains(&"set_shuffle:VLC media player:true".to_string()));
    }

    #[tokio::test]
    async fn test_request_with_player_list_key_ignored() {
        // A "request" carrying playerList is an mpris client packet, not a
        // control request (mpriscontrolplugin.cpp:257-259).
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend.clone());
        plugin.core.apply_player_added(sample_state());

        let result = plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "playerList": ["VLC media player"],
                    "action": "Stop"
                })),
            )
            .await
            .unwrap();
        assert!(result.is_none());
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn test_request_album_art_declined() {
        // We advertise supportAlbumArtPayload=false, so this should never
        // arrive; if it does we drop it (upstream honors it with a payload —
        // mpriscontrolplugin.cpp:261-264,217-253).
        let backend = MockMprisBackend::new().with_player(sample_state());
        let plugin = plugin_with_backend(backend);
        plugin.core.apply_player_added(sample_state());

        let result = plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "albumArtUrl": "file:///tmp/art.png"
                })),
            )
            .await
            .unwrap();
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------
    // on_connected + degradation
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_on_connected_sends_request_then_player_list() {
        // GSConnect's connected() sends BOTH on one connect, request first
        // (gsconnect src/service/plugins/mpris.js:69-74): the remote-role
        // pull followed by our control-role list.
        let backend = MockMprisBackend::new();
        let plugin = plugin_with_backend(backend);
        plugin.core.apply_player_added(sample_state());

        let packets = plugin.on_connected("phone");
        assert_eq!(packets.len(), 2);
        // [0]: remote-role pull so the phone tells us about ITS players
        // (the pre-refactor on_connected behavior, old mpris.rs:102-107).
        assert_eq!(packets[0].packet_type, "kdeconnect.mpris.request");
        assert_eq!(
            packets[0].body,
            serde_json::json!({ "requestPlayerList": true })
        );
        // [1]: control-role player list (gsconnect mpris.js:73).
        assert_eq!(packets[1].packet_type, "kdeconnect.mpris");
        assert_eq!(
            packets[1].body.get("playerList").unwrap(),
            &serde_json::json!(["VLC media player"])
        );
    }

    #[tokio::test]
    async fn test_on_connected_sends_request_player_list_without_backend() {
        // The remote-role pull does not depend on the session backend: even
        // degraded, a connecting phone must be asked for its players.
        let (plugin, _) = setup();
        let packets = plugin.on_connected("phone");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.mpris.request");
        assert_eq!(
            packets[0].body,
            serde_json::json!({ "requestPlayerList": true })
        );
    }

    #[tokio::test]
    async fn test_degraded_no_backend_no_crash() {
        // mousepad/clipboard pattern: no backend → log + degrade, never
        // crash. on_connected still sends the remote-role requestPlayerList
        // (no backend needed); requests get an empty list and commands are
        // dropped.
        let (plugin, _) = setup();
        let packets = plugin.on_connected("phone");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.mpris.request");

        let responses = plugin
            .handle_packet(
                "phone",
                request_packet(serde_json::json!({
                    "player": "VLC media player",
                    "action": "PlayPause",
                    "requestNowPlaying": true
                })),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].body,
            serde_json::json!({
                "playerList": [],
                "supportAlbumArtPayload": false,
            })
        );
    }

    #[tokio::test]
    async fn test_watcher_task_feeds_core_from_events() {
        // The fan-out task (backend events → core cache) works end to end
        // with the mock backend; the playerList then reflects the added
        // player. Requires a connection manager for wiring; with zero
        // connected devices the fan-out is a no-op.
        let (plugin, _) = setup();
        let backend = MockMprisBackend::new();
        plugin.set_backend(backend.clone());

        // Simulate what the watcher task does with a PlayerAdded event.
        plugin.core.apply_player_added(sample_state());
        let changed = PlayerPropsChanged {
            playback_status: Some(true),
            ..Default::default()
        };
        let mut playing = sample_state();
        playing.is_playing = true;
        plugin.core.apply_props_changed(playing, &changed);

        let state = plugin.core.cached_state("VLC media player").unwrap();
        assert!(state.is_playing);
    }

    #[tokio::test]
    async fn test_watcher_retries_after_failed_start() {
        // watcher_started must be set only on a SUCCESSFUL start_watching:
        // a failed start (e.g. backend hiccup at wire time) must not wedge
        // the fan-out forever.
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "test"));
        let temp = tempfile::TempDir::new().unwrap();
        let certs = Arc::new(crate::protocol::CertificateManager::new(
            temp.path().to_path_buf(),
        ));
        certs.init().unwrap();
        let cm = Arc::new(crate::protocol::ConnectionManager::new(certs).unwrap());
        let plugin = MprisPlugin::new(broadcaster).with_connection_manager(cm);

        let failing = MockMprisBackend::new();
        failing.fail_start.store(true, Ordering::SeqCst);
        plugin.set_backend(failing.clone());
        assert!(
            failing
                .event_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none(),
            "failed start must not store an event channel"
        );

        // Retry with a working backend: the watcher must start.
        let working = MockMprisBackend::new();
        plugin.set_backend(working.clone());
        assert!(
            working
                .event_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some(),
            "watcher must be retryable after a failed start_watching"
        );
    }

    // -----------------------------------------------------------------
    // Remote role (phone-as-player) — pre-existing behavior, unchanged
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_mpris_packet() {
        let (plugin, _) = setup();
        let packet = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "player": "Spotify",
                "title": "Test Song",
                "artist": "Test Artist",
                "album": "Test Album",
                "isPlaying": true,
                "canPlay": true,
                "canGoNext": true,
                "canGoPrevious": true
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        let players = plugin.get_players("device1");
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].player.as_deref(), Some("Spotify"));
        assert_eq!(players[0].title.as_deref(), Some("Test Song"));
        assert!(players[0].is_playing);
    }

    #[tokio::test]
    async fn test_handle_mpris_updates_existing() {
        let (plugin, _) = setup();
        let packet1 = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "player": "Spotify",
                "title": "Song 1",
                "isPlaying": false
            }),
        );
        let packet2 = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "player": "Spotify",
                "title": "Song 2",
                "isPlaying": true
            }),
        );
        plugin
            .handle_packet("device1", packet1)
            .await
            .expect("Value expected to be present");
        plugin
            .handle_packet("device1", packet2)
            .await
            .expect("Value expected to be present");
        let players = plugin.get_players("device1");
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].title.as_deref(), Some("Song 2"));
        assert!(players[0].is_playing);
    }

    #[tokio::test]
    async fn test_handle_mpris_multiple_players() {
        let (plugin, _) = setup();
        let packet1 = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({ "player": "Spotify", "title": "Song A" }),
        );
        let packet2 = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({ "player": "YouTube Music", "title": "Song B" }),
        );
        plugin
            .handle_packet("device1", packet1)
            .await
            .expect("Value expected to be present");
        plugin
            .handle_packet("device1", packet2)
            .await
            .expect("Value expected to be present");
        let players = plugin.get_players("device1");
        assert_eq!(players.len(), 2);
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_players() {
        let (plugin, _) = setup();
        let packet = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({ "player": "Spotify" }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("Value expected to be present");
        assert_eq!(plugin.get_players("device1").len(), 1);
        plugin.on_disconnected("device1");
        assert!(plugin.get_players("device1").is_empty());
    }

    #[tokio::test]
    async fn test_mpris_info_defaults() {
        let info: MprisInfo = serde_json::from_value(serde_json::json!({
            "player": "Test"
        }))
        .expect("Value expected to be present");
        assert_eq!(info.player.as_deref(), Some("Test"));
        assert!(info.title.is_none());
        assert!(!info.can_play);
        assert!(!info.is_playing);
    }

    // -----------------------------------------------------------------
    // TESTS FROM PROTOCOL REFERENCE (gsconnect mpris.js, kdeconnect-kde)
    // These tests use the ACTUAL field names from the protocol
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_mpris_with_protocol_fields() {
        // GSConnect and kdeconnect-kde use "length" and "pos"
        // See gsconnect/src/service/plugins/mpris.js:327-385
        let (plugin, _) = setup();
        let packet = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "player": "vlc",
                "title": "Test Song",
                "artist": "Test Artist",
                "album": "Test Album",
                "isPlaying": true,
                "length": 180000,
                "pos": 60000,
                "canPlay": true,
                "canSeek": true,
                "canGoNext": true,
                "canGoPrevious": true
            }),
        );
        let result = plugin.handle_packet("device1", packet).await;
        assert!(
            result.is_ok(),
            "MPRIS should handle 'length' and 'pos' fields"
        );

        let players = plugin.get_players("device1");
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].player.as_deref(), Some("vlc"));
    }

    #[tokio::test]
    async fn test_handle_mpris_length_and_pos_values() {
        // Verify that length and pos are correctly captured
        let (plugin, _) = setup();
        let packet = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "player": "spotify",
                "title": "Another Song",
                "length": 240000,
                "pos": 120000,
                "isPlaying": false
            }),
        );
        let result = plugin.handle_packet("device1", packet).await;
        assert!(result.is_ok());

        let players = plugin.get_players("device1");
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].length, Some(240000));
        assert_eq!(players[0].position, Some(120000));
    }

    #[tokio::test]
    async fn test_handle_mpris_with_volume() {
        let (plugin, _) = setup();
        let packet = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "player": "vlc",
                "title": "Test Song",
                "volume": 0.75,
                "isPlaying": true
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());

        let players = plugin.get_players("device1");
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].volume, Some(0.75));
    }

    #[tokio::test]
    async fn test_handle_mpris_with_loop_and_shuffle() {
        let (plugin, _) = setup();
        let packet = Packet::new(
            "kdeconnect.mpris".to_string(),
            serde_json::json!({
                "player": "vlc",
                "title": "Test Song",
                "loopStatus": "track",
                "shuffle": true,
                "isPlaying": true
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());

        let players = plugin.get_players("device1");
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].loop_status, Some("track".to_string()));
        assert_eq!(players[0].shuffle, Some(true));
    }
}

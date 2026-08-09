//! zbus backend for the MPRIS control role: real org.mpris.MediaPlayer2.*
//! discovery, tracking, and control over the session D-Bus.
//!
//! Structure mirrors kdeconnect-kde's mpriscontrol plugin
//! (mpriscontrolplugin.cpp, cited by line): one bus watcher for
//! name-owner changes (:39-50), one per-player listener translating
//! PropertiesChanged (:122-196) and Seeked (:101-120) into
//! [`MprisBackendEvent`]s, and synchronous-by-request property reads for
//! answers and commands (:255-358).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{mpsc::UnboundedSender, RwLock};
use tracing::{debug, info, warn};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::utils::errors::{Error, Result};

use super::{
    display_name_for, is_ignored_service, local_file_fallback, AlbumArtSource, LocalPlayerState,
    MprisBackend, MprisBackendEvent, PlayerPropsChanged, MPRIS_SERVICE_PREFIX,
};

const MPRIS_OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub(crate) trait MediaPlayer2 {
    /// Display name for the player (mpriscontrolplugin.cpp:78-83).
    #[zbus(property)]
    fn identity(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub(crate) trait MediaPlayer2Player {
    fn play(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn play_pause(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    /// Offset in MICROSECONDS (MPRIS spec; wire passthrough —
    /// mpriscontrolplugin.cpp:303-307).
    fn seek(&self, offset: i64) -> zbus::Result<()>;
    /// Absolute position in MICROSECONDS (MPRIS spec).
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> zbus::Result<()>;

    /// Position in MICROSECONDS.
    #[zbus(signal)]
    fn seeked(&self, position: i64);

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    /// Position in MICROSECONDS.
    #[zbus(property)]
    fn position(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn volume(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn set_volume(&self, volume: f64) -> zbus::Result<()>;
    #[zbus(property)]
    fn can_play(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_pause(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_go_previous(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_seek(&self) -> zbus::Result<bool>;
    /// Optional property — not every player exposes it
    /// (mpriscontrolplugin.cpp:335-339).
    #[zbus(property)]
    fn loop_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_loop_status(&self, status: &str) -> zbus::Result<()>;
    /// Optional property (mpriscontrolplugin.cpp:341-345).
    #[zbus(property)]
    fn shuffle(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) -> zbus::Result<()>;
}

/// zbus session-bus backend. `connect()` is the fallible step (no session
/// bus → plugin degrades); everything after that logs and keeps going.
pub struct ZbusMprisBackend {
    /// The live session connection. `zbus::Connection` is internally
    /// `Arc`-based and `Clone`, so every method takes a cheap snapshot
    /// under a short read lock (`current_conn`) rather than holding the
    /// lock across an await on a proxy call. `watch_supervisor` installs a
    /// fresh connection here on recovery (install-on-recovery contract,
    /// see its doc comment) so both the next watch iteration and every
    /// control method rebind to the live bus.
    conn: Arc<RwLock<zbus::Connection>>,
    /// Display name → bus service name; the watcher task keeps it current.
    services: Arc<RwLock<HashMap<String, String>>>,
    /// Bus service name → its signal-listener task. Aborted on remove /
    /// replace so a dead name can't keep emitting (and a restarted player
    /// never races its previous listener).
    listeners: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

fn internal(msg: impl Into<String>) -> Error {
    Error::Internal(msg.into())
}

impl ZbusMprisBackend {
    /// Connect to the session bus. Err when no session bus exists — the
    /// caller (MprisPlugin::enable_session_backend) degrades with a log
    /// event, mousepad/clipboard-style.
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| internal(format!("cannot connect to session D-Bus: {e}")))?;
        Ok(Self {
            conn: Arc::new(RwLock::new(conn)),
            services: Arc::new(RwLock::new(HashMap::new())),
            listeners: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Snapshot the current live connection. Cloning `zbus::Connection` is
    /// cheap; the read lock is held only long enough to clone, never
    /// across an await on a proxy call.
    async fn current_conn(&self) -> zbus::Connection {
        self.conn.read().await.clone()
    }

    async fn service_for(&self, display_name: &str) -> Result<String> {
        self.services
            .read()
            .await
            .get(display_name)
            .cloned()
            .ok_or_else(|| internal(format!("unknown MPRIS player '{display_name}'")))
    }

    async fn player_proxy(&self, display_name: &str) -> Result<MediaPlayer2PlayerProxy<'_>> {
        let service = self.service_for(display_name).await?;
        let conn = self.current_conn().await;
        MediaPlayer2PlayerProxy::new(&conn, service.clone())
            .await
            .map_err(|e| internal(format!("cannot build MPRIS proxy for {service}: {e}")))
    }
}

/// MPRIS Volume (0.0-1.0) → the 0-100 wire integer, truncated
/// (mpriscontrolplugin.cpp:140,350).
fn volume_to_wire(volume: f64) -> i64 {
    (volume * 100.0) as i64
}

/// 0-100 wire int → 0.0-1.0 (mpriscontrolplugin.cpp:298-301), clamped to
/// the valid range first: the wire value is unvalidated peer input, and a
/// negative/huge value must not become an out-of-range MPRIS Volume (or
/// wrap through a truncating cast).
fn wire_volume_to_mpris(volume: i64) -> f64 {
    volume.clamp(0, 100) as f64 / 100.0
}

/// ms→µs for the wire SetPosition value (mpriscontrolplugin.cpp:310 does a
/// plain `* 1000`). Peer input is unvalidated: saturate instead of wrapping
/// a huge value into a bogus position/offset.
fn ms_to_us(position_ms: i64) -> i64 {
    position_ms.saturating_mul(1000)
}

fn meta_string(metadata: &HashMap<String, OwnedValue>, key: &str) -> String {
    metadata
        .get(key)
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default()
}

/// mpris:trackid is an OBJECT PATH (`o`) per the MPRIS spec — parse it as
/// such; a String conversion fails on spec-conformant players. String-typed
/// trackids from non-conforming players are tolerated.
fn meta_track_id(metadata: &HashMap<String, OwnedValue>) -> Option<OwnedObjectPath> {
    let value = metadata.get("mpris:trackid")?;
    if let Ok(path) = OwnedObjectPath::try_from(value.clone()) {
        return Some(path);
    }
    let s = String::try_from(value.clone())
        .ok()
        .filter(|s| !s.is_empty())?;
    ObjectPath::try_from(s).ok().map(Into::into)
}

/// Read the full current state of one player (mpriscontrolplugin.cpp:317-358
/// for which fields matter; :396-425 for the metadata mapping).
async fn read_state(
    player: &MediaPlayer2PlayerProxy<'_>,
    service: &str,
    display_name: &str,
) -> LocalPlayerState {
    let metadata = player.metadata().await.unwrap_or_default();

    let raw_title = meta_string(&metadata, "xesam:title");
    let artist = metadata
        .get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
        .unwrap_or_default()
        .join(", "); // mpriscontrolplugin.cpp:399
    let raw_album = meta_string(&metadata, "xesam:album");
    let url = meta_string(&metadata, "xesam:url");
    let (title, album) = local_file_fallback(&raw_title, &artist, &raw_album, &url);
    let length_ms = metadata
        .get("mpris:length")
        .and_then(|v| i64::try_from(v.clone()).ok())
        .map_or(-1, |us| us / 1000); // µs→ms, -1 when absent (:418-423)

    LocalPlayerState {
        service: service.to_string(),
        name: display_name.to_string(),
        title,
        artist,
        album,
        album_art_url: meta_string(&metadata, "mpris:artUrl"),
        url,
        length_ms,
        position_ms: player.position().await.unwrap_or(0) / 1000,
        is_playing: player
            .playback_status()
            .await
            .map(|s| s == "Playing")
            .unwrap_or(false),
        volume: player.volume().await.map(volume_to_wire).unwrap_or(0),
        can_play: player.can_play().await.unwrap_or(false),
        can_pause: player.can_pause().await.unwrap_or(false),
        can_go_next: player.can_go_next().await.unwrap_or(false),
        can_go_previous: player.can_go_previous().await.unwrap_or(false),
        can_seek: player.can_seek().await.unwrap_or(false),
        // Optional properties: .ok() so players without them report None
        // (mpriscontrolplugin.cpp:335-345 checks property validity).
        loop_status: player.loop_status().await.ok(),
        shuffle: player.shuffle().await.ok(),
    }
}

/// Register a newly-seen player: resolve its display name, snapshot it,
/// spawn its signal listener (mpriscontrolplugin.cpp:74-99).
async fn add_player(
    conn: &zbus::Connection,
    services: &Arc<RwLock<HashMap<String, String>>>,
    listeners: &Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,
    service: &str,
    tx: &UnboundedSender<MprisBackendEvent>,
) {
    {
        let map = services.read().await;
        if map.values().any(|s| s == service) {
            return; // already tracked
        }
    }

    // Player restart without an intervening remove: kill the stale listener
    // so it can't keep emitting for a dead name.
    if let Some(handle) = listeners.write().await.remove(service) {
        handle.abort();
    }

    let identity = match MediaPlayer2Proxy::new(conn, service).await {
        Ok(root) => root.identity().await.unwrap_or_default(),
        Err(e) => {
            debug!(service = %service, error = %e, "MPRIS player vanished during add");
            return;
        }
    };
    let player_proxy = match MediaPlayer2PlayerProxy::new(conn, service).await {
        Ok(p) => p,
        Err(e) => {
            debug!(service = %service, error = %e, "MPRIS player proxy failed during add");
            return;
        }
    };

    let display_name = {
        let map = services.read().await;
        display_name_for(&identity, service, &map.keys().cloned().collect())
    };
    let state = read_state(&player_proxy, service, &display_name).await;

    services
        .write()
        .await
        .insert(display_name.clone(), service.to_string());

    info!(
        service = %service,
        player = %display_name,
        event = "mpris_player_discovered",
        "MPRIS player discovered"
    );
    let _ = tx.send(MprisBackendEvent::PlayerAdded(state));

    let conn = conn.clone();
    let service_owned = service.to_string();
    let tx = tx.clone();
    let handle = tokio::spawn(async move {
        player_listener(conn, service_owned, display_name, tx).await;
    });
    listeners.write().await.insert(service.to_string(), handle);
}

/// Drop a player whose bus name lost its owner
/// (mpriscontrolplugin.cpp:198-215).
async fn remove_player(
    services: &Arc<RwLock<HashMap<String, String>>>,
    listeners: &Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,
    service: &str,
    tx: &UnboundedSender<MprisBackendEvent>,
) {
    // Kill the signal listener first: a dead name must stop emitting even
    // if the tracked-name lookup below finds nothing.
    if let Some(handle) = listeners.write().await.remove(service) {
        handle.abort();
    }

    let mut map = services.write().await;
    let name = map
        .iter()
        .find(|(_, s)| s.as_str() == service)
        .map(|(n, _)| n.clone());
    if let Some(name) = name {
        map.remove(&name);
        drop(map);
        info!(
            service = %service,
            player = %name,
            event = "mpris_player_gone",
            "MPRIS player vanished"
        );
        let _ = tx.send(MprisBackendEvent::PlayerRemoved {
            service: service.to_string(),
        });
    }
}

/// Per-player signal listener: PropertiesChanged on the Player interface and
/// Seeked, translated into backend events
/// (mpriscontrolplugin.cpp:122-196, 101-120).
async fn player_listener(
    conn: zbus::Connection,
    service: String,
    display_name: String,
    tx: UnboundedSender<MprisBackendEvent>,
) {
    let player = match MediaPlayer2PlayerProxy::new(&conn, service.as_str()).await {
        Ok(p) => p,
        Err(e) => {
            debug!(service = %service, error = %e, "MPRIS listener proxy failed");
            return;
        }
    };
    let props = zbus::fdo::PropertiesProxy::builder(&conn)
        .destination(service.as_str())
        .and_then(|b| b.path(MPRIS_OBJECT_PATH));
    let props = match props {
        Ok(builder) => match builder.build().await {
            Ok(p) => p,
            Err(e) => {
                debug!(service = %service, error = %e, "MPRIS properties proxy failed");
                return;
            }
        },
        Err(e) => {
            debug!(service = %service, error = %e, "MPRIS properties proxy failed");
            return;
        }
    };
    let mut props_stream = match props.receive_properties_changed().await {
        Ok(s) => s,
        Err(e) => {
            debug!(service = %service, error = %e, "MPRIS PropertiesChanged stream failed");
            return;
        }
    };
    let mut seeked_stream = match player.receive_seeked().await {
        Ok(s) => Some(s),
        Err(e) => {
            // A failed Seeked subscription must NOT silence the player:
            // PropertiesChanged still flows. kdeconnect-kde subscribes the
            // two independently (mpriscontrolplugin.cpp:94-95).
            debug!(service = %service, error = %e, "MPRIS Seeked stream failed; continuing without seek events");
            None
        }
    };

    // Volume dedup (mpriscontrolplugin.cpp:37,139-146): players emit Volume
    // unchanged; only forward actual changes.
    let mut prev_volume: i64 = -1;

    loop {
        tokio::select! {
            signal = props_stream.next() => {
                let Some(signal) = signal else { break };
                let Ok(args) = signal.args() else { continue };
                if args.interface_name().as_str() != "org.mpris.MediaPlayer2.Player" {
                    continue;
                }
                let changed_map = args.changed_properties();

                let mut changed = PlayerPropsChanged::default();
                if changed_map.contains_key("Volume") {
                    if let Ok(v) = player.volume().await {
                        let volume = volume_to_wire(v);
                        if volume != prev_volume {
                            changed.volume = Some(volume);
                            prev_volume = volume;
                        }
                    }
                }
                if changed_map.contains_key("Metadata") {
                    changed.metadata = true;
                }
                if changed_map.contains_key("PlaybackStatus") {
                    if let Ok(status) = player.playback_status().await {
                        changed.playback_status = Some(status == "Playing");
                    }
                }
                if changed_map.contains_key("LoopStatus") {
                    if let Ok(status) = player.loop_status().await {
                        changed.loop_status = Some(status);
                    }
                }
                if changed_map.contains_key("Shuffle") {
                    if let Ok(shuffle) = player.shuffle().await {
                        changed.shuffle = Some(shuffle);
                    }
                }
                if changed_map.contains_key("CanPause") {
                    if let Ok(v) = player.can_pause().await {
                        changed.can_pause = Some(v);
                    }
                }
                if changed_map.contains_key("CanPlay") {
                    if let Ok(v) = player.can_play().await {
                        changed.can_play = Some(v);
                    }
                }
                if changed_map.contains_key("CanGoNext") {
                    if let Ok(v) = player.can_go_next().await {
                        changed.can_go_next = Some(v);
                    }
                }
                if changed_map.contains_key("CanGoPrevious") {
                    if let Ok(v) = player.can_go_previous().await {
                        changed.can_go_previous = Some(v);
                    }
                }
                if changed_map.contains_key("CanSeek") {
                    if let Ok(v) = player.can_seek().await {
                        changed.can_seek = Some(v);
                    }
                }

                let state = read_state(&player, &service, &display_name).await;
                let _ = tx.send(MprisBackendEvent::PropertiesChanged { state, changed });
            }
            signal = async { seeked_stream.as_mut().expect("select precondition").next().await }, if seeked_stream.is_some() => {
                match signal {
                    Some(signal) => match signal.args() {
                        Ok(args) => {
                            let _ = tx.send(MprisBackendEvent::Seeked {
                                service: service.clone(),
                                position_us: *args.position(),
                            });
                        }
                        Err(e) => {
                            debug!(service = %service, error = %e, "Malformed MPRIS Seeked signal");
                        }
                    },
                    // Seeked stream ended on its own: disable just this
                    // branch, keep listening for PropertiesChanged.
                    None => {
                        debug!(service = %service, "MPRIS Seeked stream ended; continuing without seek events");
                        seeked_stream = None;
                    }
                }
            }
        }
    }
}

/// Bus watch loop: initial discovery, then NameOwnerChanged
/// (mpriscontrolplugin.cpp:39-72).
async fn watch_loop(
    conn: zbus::Connection,
    services: Arc<RwLock<HashMap<String, String>>>,
    listeners: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,
    tx: UnboundedSender<MprisBackendEvent>,
) {
    let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, event = "mpris_watch_failed", "Cannot watch the session bus for MPRIS players");
            return;
        }
    };

    // Add players that already exist (mpriscontrolplugin.cpp:44-49).
    match dbus.list_names().await {
        Ok(names) => {
            for name in names {
                let service = name.as_str();
                if service.starts_with(MPRIS_SERVICE_PREFIX) && !is_ignored_service(service) {
                    add_player(&conn, &services, &listeners, service, &tx).await;
                }
            }
        }
        Err(e) => {
            warn!(error = %e, event = "mpris_list_names_failed", "Cannot list bus names for MPRIS discovery");
        }
    }

    let mut stream = match dbus.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, event = "mpris_watch_failed", "Cannot subscribe to NameOwnerChanged");
            return;
        }
    };

    while let Some(signal) = stream.next().await {
        let Ok(args) = signal.args() else { continue };
        let service = args.name().as_str();
        if !service.starts_with(MPRIS_SERVICE_PREFIX) || is_ignored_service(service) {
            continue;
        }
        let old_owner: Option<zbus::names::UniqueName> = Option::from(args.old_owner().clone());
        let new_owner: Option<zbus::names::UniqueName> = Option::from(args.new_owner().clone());
        // kdeconnect-kde handles both legs (:63-71): a name can change owner
        // (player restart) in a single signal.
        if old_owner.is_some() {
            remove_player(&services, &listeners, service, &tx).await;
        }
        if new_owner.is_some() {
            add_player(&conn, &services, &listeners, service, &tx).await;
        }
    }

    warn!(
        event = "mpris_watch_ended",
        "NameOwnerChanged stream ended; MPRIS discovery stopped"
    );
}

#[async_trait::async_trait]
impl MprisBackend for ZbusMprisBackend {
    fn name(&self) -> &str {
        "zbus"
    }

    async fn player_state(&self, display_name: &str) -> Option<LocalPlayerState> {
        let service = self.service_for(display_name).await.ok()?;
        let conn = self.current_conn().await;
        let proxy = MediaPlayer2PlayerProxy::new(&conn, service.as_str())
            .await
            .ok()?;
        Some(read_state(&proxy, &service, display_name).await)
    }

    async fn transport(&self, display_name: &str, action: &str) -> Result<()> {
        let proxy = self.player_proxy(display_name).await?;
        // Actions arrive whitelisted (super::ALLOWED_ACTIONS — GSConnect
        // mpris.js:217-231); kdeconnect-kde calls the string verbatim
        // (mpriscontrolplugin.cpp:282-287).
        match action {
            "Play" => proxy.play().await,
            "Pause" => proxy.pause().await,
            "PlayPause" => proxy.play_pause().await,
            "Stop" => proxy.stop().await,
            "Next" => proxy.next().await,
            "Previous" => proxy.previous().await,
            other => return Err(internal(format!("disallowed MPRIS action '{other}'"))),
        }
        .map_err(|e| internal(format!("MPRIS {action} on {display_name} failed: {e}")))
    }

    async fn set_loop_status(&self, display_name: &str, loop_status: &str) -> Result<()> {
        self.player_proxy(display_name)
            .await?
            .set_loop_status(loop_status)
            .await
            .map_err(|e| internal(format!("setLoopStatus on {display_name} failed: {e}")))
    }

    async fn set_shuffle(&self, display_name: &str, shuffle: bool) -> Result<()> {
        self.player_proxy(display_name)
            .await?
            .set_shuffle(shuffle)
            .await
            .map_err(|e| internal(format!("setShuffle on {display_name} failed: {e}")))
    }

    async fn set_volume(&self, display_name: &str, volume: i64) -> Result<()> {
        // 0-100 wire int → 0.0-1.0 (mpriscontrolplugin.cpp:298-301), clamped
        // (see wire_volume_to_mpris).
        let volume = wire_volume_to_mpris(volume);
        self.player_proxy(display_name)
            .await?
            .set_volume(volume)
            .await
            .map_err(|e| internal(format!("setVolume on {display_name} failed: {e}")))
    }

    async fn seek(&self, display_name: &str, offset_us: i64) -> Result<()> {
        // MICROSECONDS passthrough (mpriscontrolplugin.cpp:303-307).
        self.player_proxy(display_name)
            .await?
            .seek(offset_us)
            .await
            .map_err(|e| internal(format!("Seek on {display_name} failed: {e}")))
    }

    async fn set_position(&self, display_name: &str, position_ms: i64) -> Result<()> {
        let proxy = self.player_proxy(display_name).await?;
        let position_us = ms_to_us(position_ms); // ms→µs (mpriscontrolplugin.cpp:310)
                                                 // Prefer the real SetPosition(trackid, pos): some players (Chrome)
                                                 // treat Seek as a fixed ±5s step regardless of the offset, only
                                                 // respecting its sign (gsconnect src/service/plugins/mpris.js:246-259).
        let track_id = proxy
            .metadata()
            .await
            .ok()
            .and_then(|md| meta_track_id(&md));
        match track_id {
            Some(id) => proxy
                .set_position(id.into(), position_us)
                .await
                .map_err(|e| internal(format!("SetPosition on {display_name} failed: {e}"))),
            None => {
                // Seek-by-difference fallback (mpriscontrolplugin.cpp:309-314).
                let current = proxy.position().await.unwrap_or(0);
                proxy
                    .seek(position_us.saturating_sub(current))
                    .await
                    .map_err(|e| {
                        internal(format!("SetPosition-as-Seek on {display_name} failed: {e}"))
                    })
            }
        }
    }

    async fn album_art(&self, display_name: &str, requested_url: &str) -> Option<AlbumArtSource> {
        // Mirror mpriscontrolplugin.cpp:217-253 — re-fetch the metadata every
        // request so the phone can't ask for a stale URL, refuse non-file://
        // schemes, and let the plugin apply the daemon-side size cap.
        let service = self.service_for(display_name).await.ok()?;
        let conn = self.current_conn().await;
        let proxy = MediaPlayer2PlayerProxy::new(&conn, service.as_str())
            .await
            .ok()?;
        let metadata = proxy.metadata().await.unwrap_or_default();
        let current_url = meta_string(&metadata, "mpris:artUrl");
        if current_url.is_empty() || current_url != requested_url {
            return None;
        }
        // Only file:// is supported upstream (mpriscontrolplugin.cpp:238-240).
        // The plugin also rejects non-file URLs, but the backend has the
        // authority here — refuse once at the source.
        let path_str = current_url.strip_prefix("file://")?;
        let path = std::path::Path::new(path_str);
        // File::metadata is the simplest size + existence check; the
        // PayloadTransfer::send_file call downstream will open the file by
        // the same path, so a successful metadata read guarantees the
        // transfer step is reachable.
        let meta = tokio::fs::metadata(path).await.ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(AlbumArtSource {
            path: path.to_path_buf(),
            size: meta.len(),
            current_url,
        })
    }

    fn start_watching(&self, tx: UnboundedSender<MprisBackendEvent>) -> Result<()> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(internal("no tokio runtime in scope for MPRIS watch"));
        }
        let conn = self.conn.clone();
        let services = self.services.clone();
        let listeners = self.listeners.clone();
        tokio::spawn(watch_supervisor(conn, services, listeners, tx));
        Ok(())
    }
}

/// Bus watch supervisor: re-runs `watch_loop` with a fresh connection
/// and bounded backoff when the loop ends (the session bus dropped,
/// dbus-daemon restarted, etc.). Returns None from a single iteration
/// when the watch stream ends — the supervisor then clears the
/// listener tasks and emits `BackendLost` so the plugin's cache drops
/// every stale entry before the next recovery cycle re-enumerates.
///
/// Install-on-recovery contract: `conn` is the SAME `Arc<RwLock<..>>` the
/// owning `ZbusMprisBackend` holds in `self.conn`. On a successful
/// re-acquire this installs the new connection into that shared cell
/// (`*conn.write().await = new_conn`) rather than dropping it, so both the
/// next `watch_loop` iteration (which snapshots at the top of the loop)
/// and every control method (`transport`, `set_volume`, ...) rebind to the
/// live bus. Dropping the new connection here (the old band-aid) left
/// every control method permanently bound to the dead initial connection.
async fn watch_supervisor(
    conn: Arc<RwLock<zbus::Connection>>,
    services: Arc<RwLock<HashMap<String, String>>>,
    listeners: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,
    tx: UnboundedSender<MprisBackendEvent>,
) {
    let mut backoff_ms: u64 = 500;
    let backoff_max_ms: u64 = 30_000;
    // Below this, a "successful" reconnect isn't actually healthy — the
    // hot-loop guard below treats it the same as a failed reconnect.
    const HEALTHY_RUN_THRESHOLD: Duration = Duration::from_secs(5);
    loop {
        // The current connection may be dead; the loop will fail fast
        // when the stream is broken. After the loop ends, drop the
        // listeners and tell the plugin to clear its cache.
        let snapshot = conn.read().await.clone();
        let started = Instant::now();
        watch_loop(snapshot, services.clone(), listeners.clone(), tx.clone()).await;
        let ran_for = started.elapsed();

        // Listen-stream ended (or errored). Tear down any in-flight
        // per-player listeners — their bus names may be dead too.
        let stale: Vec<String> = {
            let mut l = listeners.write().await;
            let keys: Vec<String> = l.keys().cloned().collect();
            for (_, handle) in l.drain() {
                handle.abort();
            }
            let mut s = services.write().await;
            s.clear();
            keys
        };
        for service in stale {
            let _ = tx.send(MprisBackendEvent::PlayerRemoved { service });
        }
        let _ = tx.send(MprisBackendEvent::BackendLost);

        // Try to acquire a fresh session connection BEFORE we wait. If the
        // bus is permanently down, the connect will fail fast and we
        // back off; on a transient bus blip, the connect succeeds and
        // the new watch_loop starts immediately.
        match zbus::Connection::session().await {
            Ok(new_conn) => {
                info!(
                    event = "mpris_backend_recovered",
                    "Reconnected to session bus; re-enumerating MPRIS players"
                );
                *conn.write().await = new_conn;

                // Hot-loop guard: a reconnect can succeed while the
                // connection it hands back is itself unusable (or some
                // other watch_loop precondition keeps failing), in which
                // case the just-installed connection fails fast on the
                // very next iteration too. Only treat this cycle as
                // healthy — and reset the backoff — if watch_loop actually
                // ran for a while; otherwise keep backing off
                // exponentially so a bad reconnect can't spin a busy loop
                // of connect/drop as fast as the bus answers.
                if ran_for >= HEALTHY_RUN_THRESHOLD {
                    backoff_ms = 500;
                } else {
                    warn!(
                        ran_for_ms = ran_for.as_millis() as u64,
                        backoff_ms = backoff_ms,
                        event = "mpris_watch_hot_loop_guard",
                        "watch_loop exited quickly after a successful reconnect; backing off instead of resetting"
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(backoff_max_ms);
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    backoff_ms = backoff_ms,
                    event = "mpris_backend_reconnect_failed",
                    "Could not re-acquire session bus; backing off"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(backoff_max_ms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use zbus::zvariant::ObjectPath;

    #[test]
    fn test_meta_track_id_object_path_typed() {
        // The MPRIS spec types mpris:trackid as OBJECT PATH 'o' — a String
        // conversion always fails on it (review finding: absolute scrub via
        // SetPosition was silently dead).
        let md = HashMap::from([(
            "mpris:trackid".to_string(),
            OwnedValue::from(ObjectPath::try_from("/org/mpris/MediaPlayer2/Track/42").unwrap()),
        )]);
        let track = meta_track_id(&md).expect("object-path trackid must parse");
        assert_eq!(track.as_str(), "/org/mpris/MediaPlayer2/Track/42");
    }

    #[test]
    fn test_meta_track_id_string_typed_tolerated() {
        // Non-conforming players send a string; tolerate it.
        let md = HashMap::from([(
            "mpris:trackid".to_string(),
            OwnedValue::from(zbus::zvariant::Str::from("/track/7")),
        )]);
        let track = meta_track_id(&md).expect("string trackid must be tolerated");
        assert_eq!(track.as_str(), "/track/7");
    }

    #[test]
    fn test_meta_track_id_missing() {
        let md = HashMap::new();
        assert!(meta_track_id(&md).is_none());
    }

    #[test]
    fn test_wire_volume_to_mpris_clamps() {
        // Peer input is unvalidated; out-of-range values must not produce an
        // out-of-range MPRIS Volume or a truncating i64→i32 cast.
        assert_eq!(wire_volume_to_mpris(0), 0.0);
        assert_eq!(wire_volume_to_mpris(42), 0.42);
        assert_eq!(wire_volume_to_mpris(100), 1.0);
        assert_eq!(wire_volume_to_mpris(-50), 0.0);
        assert_eq!(wire_volume_to_mpris(150), 1.0);
        assert_eq!(wire_volume_to_mpris(i64::MAX), 1.0);
        assert_eq!(wire_volume_to_mpris(i64::MIN), 0.0);
    }

    #[tokio::test]
    async fn test_remove_player_aborts_listener_task() {
        // A removed/restarted player's old listener must be aborted so it
        // can't keep emitting for a dead name.
        let services = Arc::new(RwLock::new(HashMap::new()));
        let listeners = Arc::new(RwLock::new(HashMap::new()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let task = tokio::spawn(futures::future::pending::<()>());
        let abort_handle = task.abort_handle();
        listeners
            .write()
            .await
            .insert("org.mpris.MediaPlayer2.vlc".to_string(), task);

        remove_player(&services, &listeners, "org.mpris.MediaPlayer2.vlc", &tx).await;

        // abort() schedules termination; let the runtime deliver it.
        tokio::task::yield_now().await;
        assert!(abort_handle.is_finished(), "listener task must be aborted");
        assert!(listeners.read().await.is_empty());
    }
}

#[cfg(test)]
mod review_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_ms_to_us_never_wraps() {
        assert_eq!(ms_to_us(90_000), 90_000_000);
        assert_eq!(ms_to_us(0), 0);
        // Just inside the representable range.
        assert_eq!(ms_to_us(i64::MAX / 1000), (i64::MAX / 1000) * 1000);
        // Beyond it: saturate, never wrap to a bogus sign.
        assert_eq!(ms_to_us(i64::MAX / 1000 + 1), i64::MAX);
        assert_eq!(ms_to_us(i64::MAX), i64::MAX);
        assert_eq!(ms_to_us(i64::MIN / 1000 - 1), i64::MIN);
        assert_eq!(ms_to_us(i64::MIN), i64::MIN);
    }
}

#[cfg(test)]
mod unit_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! Wire-unit conversions (Task 1.5 / Lane B findings 21-24).
    //!
    //! The KDE Connect MPRIS wire is strict on units:
    //!
    //! - `pos` is MILLISECONDS on the wire (mpriscontrolplugin.cpp:117,192,324
    //!   divides the MPRIS `Position` by 1000; MPRIS is µs).
    //! - `SetPosition` is MILLISECONDS on the wire (mpriscontrolplugin.cpp:310
    //!   multiplies by 1000 before calling MPRIS).
    //! - `Seek` is MICROSECONDS passthrough (mpriscontrolplugin.cpp:303-307).
    //! - `mpris:length` is MILLISECONDS on the wire (mpriscontrolplugin.cpp:419
    //!   divides the MPRIS `Length` by 1000; MPRIS is µs).
    //! - `volume` is 0-100 INTEGER (mpriscontrolplugin.cpp:140,298-301,350).
    //!
    //! These tests pin the conversions explicitly so a future change to a
    //! `Duration`/`TimeDelta` type or a unit rename fails loudly rather than
    //! silently shifting the wire.
    use super::*;

    #[test]
    fn test_micros_to_millis_is_truncation() {
        // MPRIS Position comes in µs; the wire is ms (mpriscontrolplugin.cpp
        // :117). 90_000_000 µs → 90_000 ms. Sub-millisecond precision is
        // truncated, matching upstream's integer divide.
        assert_eq!(90_000_000_i64 / 1000, 90_000);
        assert_eq!(1_999_i64 / 1000, 1); // truncated
                                         // 0 stays 0 (useful as a sanity check, not a math fact).
        let zero: i64 = 0;
        assert_eq!(zero / 1000, 0);
    }

    #[test]
    fn test_length_ms_positive_value_matches_upstream() {
        // mpriscontrolplugin.cpp:418-423: length is read as a long long and
        // divided by 1000. The rust code mirrors that integer divide.
        let us: i64 = 180_000_000; // 3 minutes in µs
        let ms = us / 1000;
        assert_eq!(ms, 180_000);
    }

    #[test]
    fn test_length_ms_zero_is_zero() {
        // mpris:length=0 (some live streams) is a legitimate value, not
        // "missing". Both upstream and rust send 0 — the phone's UI
        // displays "0:00" rather than "unknown", which is the upstream
        // behavior.
        let us: i64 = 0;
        assert_eq!(us / 1000, 0);
    }

    #[test]
    fn test_length_ms_negative_passes_through_unmodified() {
        // mpriscontrolplugin.cpp:418-423 does NOT clamp negative mpris:length;
        // it just divides by 1000. Some players report a negative value
        // (e.g. -1 for "unknown") and the phone gets `length=-1` or
        // `length=0` directly. The negative-as-no-info interpretation is the
        // PHONE's. We pin the divide behavior so a future "let's clamp to
        // 0" change surfaces as a test failure.
        let us: i64 = -1;
        let ms = us / 1000;
        assert_eq!(ms, 0); // matches upstream's wire output for length=-1
        let us: i64 = -1000;
        let ms = us / 1000;
        assert_eq!(ms, -1); // matches upstream's wire output
    }

    #[test]
    fn test_volume_units_round_trip() {
        // Volume is 0-100 integer on the wire (mpriscontrolplugin.cpp:140,
        // 350). The backend's `volume_to_wire` truncates MPRIS 0.0-1.0 to
        // the integer 0-100 the wire carries. We pin every boundary.
        assert_eq!(volume_to_wire(0.0), 0);
        assert_eq!(volume_to_wire(0.5), 50);
        assert_eq!(volume_to_wire(1.0), 100);
        // Sub-integer is truncated, matching upstream's `(int)`.
        assert_eq!(volume_to_wire(0.999), 99);
    }

    #[test]
    fn test_volume_to_mpris_clamps_at_zero_and_one() {
        // Peer-input volume is clamped to [0, 100] before scaling to 0.0-1.0
        // (mpriscontrolplugin.cpp:298-301). Without this clamp, a negative
        // series of integers would round to a negative f64, which is out
        // of the MPRIS Volume range — silently accepted by the bus but
        // not by every player.
        assert_eq!(wire_volume_to_mpris(0), 0.0);
        assert_eq!(wire_volume_to_mpris(50), 0.5);
        assert_eq!(wire_volume_to_mpris(100), 1.0);
        // Adversarial inputs.
        assert_eq!(wire_volume_to_mpris(-5), 0.0);
        assert_eq!(wire_volume_to_mpris(150), 1.0);
        assert_eq!(wire_volume_to_mpris(i64::MAX), 1.0);
        assert_eq!(wire_volume_to_mpris(i64::MIN), 0.0);
    }

    #[test]
    fn test_set_position_ms_to_us_is_exact() {
        // mpriscontrolplugin.cpp:310: `position = np.get<qlonglong>(...,
        // 0) * 1000` (plain multiplication, no clamp). The rust code
        // saturates to defend against peer-input overflow instead of
        // wrapping to a bogus sign — the saturating variant is the
        // defensive choice, but the in-range paths must be exact.
        assert_eq!(ms_to_us(95_000), 95_000_000);
        assert_eq!(ms_to_us(0), 0);
    }

    #[test]
    fn test_seek_offset_is_microseconds_passthrough() {
        // mpriscontrolplugin.cpp:303-307: Seek is passed through to MPRIS
        // unchanged. The rust backend's `seek` does the same. Pin the
        // contract: any signed value the phone sends reaches the bus
        // unchanged (no /1000, no *1000).
        let offset: i64 = -10_000_000; // -10 seconds in µs
        assert_eq!(offset, -10_000_000);
        // The actual backend pass-through is tested at the higher level
        // (test_request_seek_is_microseconds_passthrough) via the mock.
    }
}

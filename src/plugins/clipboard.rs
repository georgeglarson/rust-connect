//! Clipboard plugin
//!
//! Single Responsibility: Bidirectional clipboard sync between the desktop
//! session clipboard (via wl-copy/wl-paste on Wayland) and paired devices.
//!
//! Wire shapes (upstream-verified, Android is the oracle):
//! - `kdeconnect.clipboard` — `{"content": "<text>"}`, no timestamp. Sent
//!   whenever a device's local clipboard changes (kdeconnect-android
//!   ClipboardPlugin.kt:77-81 `propagateClipboard`, and :151-160 for the body
//!   contract; kdeconnect-kde clipboardplugin.cpp:131-138). Receivers apply it
//!   unconditionally (ClipboardPlugin.kt:43-46, clipboardplugin.cpp:186-187).
//! - `kdeconnect.clipboard.connect` — `{"content": "<text>", "timestamp": <ms
//!   since epoch>}`, sent once on connect with the last-known content and its
//!   update time (ClipboardPlugin.kt:83-98, 101-105, and :162-177 for the
//!   body contract; kdeconnect-kde connected()→sendConnectPacket,
//!   clipboardplugin.cpp:39-42, 141-152). Receivers IGNORE it when the
//!   timestamp is 0 (unknown) or older than their own last update
//!   (ClipboardPlugin.kt:48-52, clipboardplugin.cpp:188-194); a missing
//!   `content` key leaves the clipboard untouched (ClipboardPlugin.kt:53).
//!
//! Echo-loop suppression: writing phone content to the session clipboard
//! re-fires our own watcher. Both upstreams suppress by CONTENT EQUALITY, not
//! tokens: kdeconnect-kde refreshes its cached content BEFORE calling
//! setMimeData so the change signal compares equal and returns early
//! (clipboardlistener.cpp:112-118 feeding :103); GSConnect sets
//! `_localBuffer = _remoteBuffer` before writing so clipboardPush's
//! `_remoteBuffer !== _localBuffer` guard fails
//! (gsconnect src/service/plugins/clipboard.js:163-168 feeding :144). We do
//! the same: state is updated before the wl-copy write, and the watcher drops
//! any change identical to the stored content.
//!
//! Watcher mechanism: a supervised `wl-paste --watch <notifier>` subprocess
//! whose notifier prints one line per clipboard change; each line triggers a
//! fresh `wl-paste --no-newline` read (timeout-guarded). This is GSConnect's
//! mechanism verbatim
//! (`wl-paste -w dbus-send ...` + on-demand `wl-paste -n`, gsconnect
//! src/wl_clipboard.js:67-74 and :208) with the dbus notifier swapped for
//! stdout lines. The supervisor restarts the watcher with capped exponential
//! backoff after unexpected exits (compositor restart, wl-paste crash);
//! clean shutdown (plugin dropped) does not restart.
//!
//! Capability honesty: incoming and outgoing both advertise
//! `kdeconnect.clipboard` + `kdeconnect.clipboard.connect`, matching upstream
//! (kdeconnect-kde kdeconnect_clipboard.json X-KdeConnect-SupportedPacketType
//! / X-KdeConnect-OutgoingPacketType; ClipboardPlugin.kt:111-113; gsconnect
//! clipboard.js:15-22). `kdeconnect.clipboard.file` is deliberately NOT
//! advertised: we don't transfer file/image payloads.
//!
//! Degradation (mousepad.rs pattern): if no usable session clipboard exists
//! (no Wayland env, wl-clipboard not installed) the plugin logs
//! `clipboard_backend_unavailable`, keeps advertising, and degrades to
//! store-and-event only — it never crashes and never silently pretends to
//! have written the desktop clipboard. The backend is enabled ONLY at the
//! production entry point (bootstrap.rs `create_state`); AppState::new, which
//! the test suite exercises, never touches the real session clipboard.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::protocol::types::Packet;
use crate::utils::errors::{Error, Result};

use super::plugin::Plugin;

/// Session clipboard abstraction so unit tests don't need a live Wayland
/// session. A second backend (e.g. X11 via xsel) is a new impl of this trait
/// plus one branch in the production wiring.
pub trait ClipboardBackend: Send + Sync {
    /// Backend name for logs ("wayland").
    fn name(&self) -> &str;

    /// Write text to the session clipboard.
    fn set(&self, content: &str) -> Result<()>;

    /// Spawn a persistent watcher that pushes each newly observed clipboard
    /// text into `tx`. Returns Err if the watcher cannot be started;
    /// implementations should supervise post-start exits internally (the
    /// Wayland backend restarts with capped backoff).
    fn spawn_watcher(&self, tx: UnboundedSender<String>) -> Result<()>;
}

/// Wayland backend via wl-clipboard (`wl-copy` / `wl-paste`). Needs
/// WAYLAND_DISPLAY + XDG_RUNTIME_DIR in the process environment; the daemon's
/// systemd --user unit (packaging/rust-connect.service) is ordered
/// After=graphical-session.target so both are present in the manager
/// environment when a session exists — the same env-presence gate
/// notification.rs:224-227 uses for desktop popups.
pub struct WaylandClipboard;

impl WaylandClipboard {
    /// Detect a usable Wayland clipboard: session env present and both
    /// wl-clipboard binaries on PATH. Returns None (degrade) otherwise.
    pub fn detect() -> Option<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            || std::env::var_os("XDG_RUNTIME_DIR").is_none()
        {
            return None;
        }
        if !binary_in_path("wl-copy") || !binary_in_path("wl-paste") {
            return None;
        }
        Some(Self)
    }
}

fn binary_in_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
            && std::fs::metadata(&candidate)
                .map(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode() & 0o111 != 0
                })
                .unwrap_or(false)
    })
}

/// Synchronous wl-copy write; invoked via spawn_blocking from the async
/// write path (see write_to_session). wl-copy reads the selection from stdin
/// and forks to serve it, so wait() returns promptly.
fn wl_write(content: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::io(format!("failed to spawn wl-copy: {e}"), None))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| Error::io(format!("failed to write to wl-copy: {e}"), None))?;
    }
    let status = child
        .wait()
        .map_err(|e| Error::io(format!("failed to wait on wl-copy: {e}"), None))?;
    if !status.success() {
        return Err(Error::io(format!("wl-copy exited with {status}"), None));
    }
    Ok(())
}

impl ClipboardBackend for WaylandClipboard {
    fn name(&self) -> &str {
        "wayland"
    }

    fn set(&self, content: &str) -> Result<()> {
        wl_write(content)
    }

    fn spawn_watcher(&self, tx: UnboundedSender<String>) -> Result<()> {
        // The supervised task owns the wl-paste lifecycle: per-run failures
        // (spawn error, wl-paste exit, stdout error) restart with backoff
        // inside supervise_watcher, so desktop-to-phone sync self-heals after
        // a compositor restart or wl-paste crash. Returning Ok here is
        // therefore honest even if the first spawn fails — the supervisor
        // keeps retrying.
        tokio::spawn(supervise_watcher(tx, run_watcher));
        Ok(())
    }
}

/// Why one watcher run ended. `Shutdown` (the plugin's receiver is gone) is
/// the only clean exit and must NOT restart; everything else is supervised.
enum WatcherExit {
    Shutdown,
    Failed(String),
}

/// One content-free line per clipboard change; the content itself is re-read
/// per notification (gsconnect src/wl_clipboard.js:67-74, 208).
async fn run_watcher(tx: UnboundedSender<String>) -> WatcherExit {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut child = match Command::new("wl-paste")
        .args(["--watch", "sh", "-c", "echo"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return WatcherExit::Failed(format!("failed to spawn wl-paste --watch: {e}")),
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        return WatcherExit::Failed("wl-paste stdout not piped".to_string());
    };

    let mut lines = BufReader::new(stdout).lines();
    let exit = loop {
        match lines.next_line().await {
            Ok(Some(_)) => match read_clipboard_once().await {
                Ok(Some(content)) => {
                    if tx.send(content).is_err() {
                        break WatcherExit::Shutdown;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, event = "clipboard_read_failed", "wl-paste read failed");
                }
            },
            // stdout closed: wl-paste exited (compositor restart, crash).
            Ok(None) => break WatcherExit::Failed("wl-paste --watch exited".to_string()),
            Err(e) => break WatcherExit::Failed(format!("watcher stdout read error: {e}")),
        }
    };
    let _ = child.kill().await;
    exit
}

/// A single `wl-paste --no-newline` read with a hard timeout, so a stuck
/// wl-paste can't hang the watcher loop indefinitely. kill_on_drop makes the
/// timed-out child get reaped instead of leaking.
async fn read_clipboard_once() -> Result<Option<String>> {
    use tokio::process::Command;

    let read = Command::new("wl-paste")
        .arg("--no-newline")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(CLIPBOARD_READ_TIMEOUT, read).await {
        Err(_) => Err(Error::io(
            format!(
                "wl-paste read timed out after {}s",
                CLIPBOARD_READ_TIMEOUT.as_secs()
            ),
            None,
        )),
        Ok(Err(e)) => Err(Error::io(format!("failed to spawn wl-paste: {e}"), None)),
        // wl-paste exits non-zero when the selection is empty
        // ("Nothing is copied") — nothing to push.
        Ok(Ok(out)) if !out.status.success() => Ok(None),
        Ok(Ok(out)) => {
            let content = String::from_utf8_lossy(&out.stdout).into_owned();
            Ok(if content.is_empty() {
                None
            } else {
                Some(content)
            })
        }
    }
}

const CLIPBOARD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const WATCHER_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);
const WATCHER_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
/// A run that survived at least this long counts as healthy and resets the
/// backoff, so one bad stretch doesn't permanently slow recovery.
const WATCHER_HEALTHY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// Restart `run_once` after unexpected exits with exponential backoff
/// (500ms doubling to a 30s cap; reset after any healthy run). Clean
/// shutdown — the plugin's receiver dropped — breaks the loop instead.
async fn supervise_watcher<F, Fut>(tx: UnboundedSender<String>, mut run_once: F)
where
    F: FnMut(UnboundedSender<String>) -> Fut,
    Fut: std::future::Future<Output = WatcherExit>,
{
    let mut backoff = WATCHER_INITIAL_BACKOFF;
    loop {
        let started = std::time::Instant::now();
        match run_once(tx.clone()).await {
            WatcherExit::Shutdown => {
                debug!(
                    event = "clipboard_watcher_shutdown",
                    "Clipboard watcher receiver closed; not restarting"
                );
                break;
            }
            WatcherExit::Failed(reason) => {
                if started.elapsed() >= WATCHER_HEALTHY_AFTER {
                    backoff = WATCHER_INITIAL_BACKOFF;
                }
                warn!(
                    reason = %reason,
                    backoff_ms = backoff.as_millis() as u64,
                    event = "clipboard_watcher_restart",
                    "Clipboard watcher stopped unexpectedly; restarting after backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(WATCHER_MAX_BACKOFF);
            }
        }
    }
}

#[derive(Default)]
struct ClipboardState {
    content: Option<String>,
    /// ms since epoch of the last accepted change, local or remote.
    timestamp: i64,
}

/// Shared decision core: state + event broadcast + the suppress/send rules.
/// Kept separate from ClipboardPlugin so the watcher task can hold it without
/// holding the whole plugin.
struct ClipboardCore {
    state: RwLock<ClipboardState>,
    plugin_events: Arc<PluginEventBroadcaster>,
}

impl ClipboardCore {
    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, ClipboardState> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, ClipboardState> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// A local clipboard change observed by the watcher. Returns the
    /// `kdeconnect.clipboard` packet to fan out, or None when the change must
    /// be suppressed.
    fn apply_local_change(&self, content: String) -> Option<Packet> {
        // Empty selections are never propagated: GSConnect neither clears the
        // remote nor sends non-text content (gsconnect
        // src/service/plugins/clipboard.js:149-156); kdeconnect-kde ignores
        // empty changes (clipboardlistener.cpp:103).
        if content.is_empty() {
            return None;
        }

        {
            let mut state = self.write_state();
            // Own-write echo / duplicate suppression by content equality:
            // kdeconnect-kde refreshes its cache before writing so the change
            // signal compares equal (clipboardlistener.cpp:112-118 -> :103);
            // GSConnect's `_remoteBuffer !== _localBuffer` guard
            // (clipboard.js:144) does the same after clipboardPull primed the
            // buffer (:163-168).
            if state.content.as_deref() == Some(content.as_str()) {
                return None;
            }
            state.timestamp = chrono::Utc::now().timestamp_millis();
            state.content = Some(content.clone());
        }

        self.plugin_events.broadcast(PluginEvent::ClipboardUpdate {
            device_id: "local".to_string(),
            content: content.clone(),
        });

        // Wire shape: {"content": ...} only, no timestamp —
        // kdeconnect-android ClipboardPlugin.kt:77-81.
        Some(Packet::new(
            "kdeconnect.clipboard".to_string(),
            serde_json::json!({ "content": content }),
        ))
    }

    /// Remote `kdeconnect.clipboard` content. Applied unconditionally — the
    /// packet type carries no timestamp (kdeconnect-android
    /// ClipboardPlugin.kt:43-46; kdeconnect-kde clipboardplugin.cpp:186-187).
    /// State is updated BEFORE the caller writes the session clipboard so the
    /// watcher's echo compares equal and is suppressed
    /// (clipboardlistener.cpp:112-118).
    fn apply_remote(&self, device_id: &str, content: String) {
        {
            let mut state = self.write_state();
            state.timestamp = chrono::Utc::now().timestamp_millis();
            state.content = Some(content.clone());
        }
        self.plugin_events.broadcast(PluginEvent::ClipboardUpdate {
            device_id: device_id.to_string(),
            content,
        });
    }

    /// Remote `kdeconnect.clipboard.connect` content. Returns true when the
    /// content was accepted (caller should write it to the session
    /// clipboard). Timestamp gate: ignore when the packet timestamp is 0
    /// (unknown) or older than our last update — kdeconnect-android
    /// ClipboardPlugin.kt:48-52; kdeconnect-kde clipboardplugin.cpp:188-194.
    fn apply_remote_connect(&self, device_id: &str, content: String, timestamp: i64) -> bool {
        {
            let mut state = self.write_state();
            if timestamp == 0 || timestamp < state.timestamp {
                debug!(
                    device_id = %device_id,
                    packet_ts = timestamp,
                    our_ts = state.timestamp,
                    event = "clipboard_connect_stale",
                    "Ignoring clipboard.connect with stale/unknown timestamp"
                );
                return false;
            }
            state.timestamp = timestamp;
            state.content = Some(content.clone());
        }
        self.plugin_events.broadcast(PluginEvent::ClipboardUpdate {
            device_id: device_id.to_string(),
            content,
        });
        true
    }
}

pub struct ClipboardPlugin {
    core: Arc<ClipboardCore>,
    backend: RwLock<Option<Arc<dyn ClipboardBackend>>>,
    connection_manager: Option<Arc<crate::protocol::ConnectionManager>>,
    watcher_started: AtomicBool,
}

impl ClipboardPlugin {
    pub fn new(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            core: Arc::new(ClipboardCore {
                state: RwLock::new(ClipboardState::default()),
                plugin_events,
            }),
            backend: RwLock::new(None),
            connection_manager: None,
            watcher_started: AtomicBool::new(false),
        }
    }

    /// Wire the daemon's connection manager — the fan-out path for local
    /// clipboard changes, mirroring share.rs's with_connection_manager.
    pub fn with_connection_manager(
        mut self,
        connection_manager: Arc<crate::protocol::ConnectionManager>,
    ) -> Self {
        self.connection_manager = Some(connection_manager);
        self.try_start_watcher();
        self
    }

    /// Inject a session clipboard backend (unit tests use a mock).
    pub fn set_backend(&self, backend: Arc<dyn ClipboardBackend>) {
        {
            let mut guard = self.backend.write().unwrap_or_else(|e| e.into_inner());
            *guard = Some(backend);
        }
        self.try_start_watcher();
    }

    /// Detect and enable the real session clipboard backend. Called ONLY from
    /// the production entry point (bootstrap.rs create_state) — never from
    /// AppState::new, which the test suite exercises against the developer's
    /// live session. Degrades with a log event, mousepad-style, when no
    /// backend is available.
    pub fn enable_session_backend(&self) {
        match WaylandClipboard::detect() {
            Some(backend) => {
                info!(
                    event = "clipboard_backend_ready",
                    backend = backend.name(),
                    "Session clipboard backend enabled"
                );
                self.set_backend(Arc::new(backend));
            }
            None => {
                warn!(
                    event = "clipboard_backend_unavailable",
                    "No usable session clipboard (need WAYLAND_DISPLAY + XDG_RUNTIME_DIR and \
                     wl-copy/wl-paste on PATH). Clipboard sync degraded to store-and-event only."
                );
            }
        }
    }

    fn backend(&self) -> Option<Arc<dyn ClipboardBackend>> {
        self.backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Start the wl-paste watcher + fan-out task once both the backend and
    /// the connection manager are present. Idempotent.
    fn try_start_watcher(&self) {
        if self.watcher_started.load(Ordering::SeqCst) {
            return;
        }
        let (Some(cm), Some(backend)) = (self.connection_manager.clone(), self.backend()) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            warn!(
                event = "clipboard_watcher_no_runtime",
                "No tokio runtime in scope; clipboard watcher not started"
            );
            return;
        }
        self.watcher_started.store(true, Ordering::SeqCst);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Err(e) = backend.spawn_watcher(tx) {
            // Do NOT latch watcher_started on failure: a later
            // set_backend/with_connection_manager must be able to retry, or
            // desktop-to-phone sync stays dead for the process lifetime.
            self.watcher_started.store(false, Ordering::SeqCst);
            warn!(error = %e, event = "clipboard_watcher_unavailable", "Could not start clipboard watcher");
            return;
        }

        let core = self.core.clone();
        tokio::spawn(async move {
            while let Some(content) = rx.recv().await {
                let Some(packet) = core.apply_local_change(content) else {
                    continue;
                };
                info!(
                    event = "clipboard_local_change",
                    len = packet
                        .body
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map_or(0, str::len),
                    "Local clipboard changed, syncing to connected devices"
                );
                // Fan out to every connected device — same recipient set the
                // REST API's set_clipboard uses
                // (api/handlers/plugins/clipboard.rs:55-70).
                for device_id in cm.connected_device_ids().await {
                    if let Err(e) = cm.send_packet(&device_id, &packet).await {
                        warn!(
                            device_id = %device_id,
                            error = %e,
                            event = "clipboard_send_failed",
                            "Failed to send clipboard packet"
                        );
                    }
                }
            }
        });
    }

    /// Phone→desktop write path. Backends do blocking subprocess I/O
    /// (std::process wl-copy), so the write runs on the blocking thread pool:
    /// a large payload or a stuck wl-copy must not stall a tokio worker on
    /// the async handle_packet path. (The watcher's reads already use
    /// tokio::process; spawn_blocking keeps the backend trait synchronous so
    /// test mocks stay trivial.)
    async fn write_to_session(&self, content: &str) {
        let Some(backend) = self.backend() else {
            debug!(
                event = "clipboard_no_backend",
                "No session clipboard backend; content stored but not written to desktop"
            );
            return;
        };
        let backend_name = backend.name().to_string();
        let content = content.to_string();
        match tokio::task::spawn_blocking(move || backend.set(&content)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                warn!(
                    error = %e,
                    backend = backend_name,
                    event = "clipboard_write_failed",
                    "Failed to write received content to the session clipboard"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    backend = backend_name,
                    event = "clipboard_write_join_failed",
                    "Clipboard backend write task failed to complete"
                );
            }
        }
    }

    pub fn get_content(&self) -> Option<String> {
        self.core.read_state().content.clone()
    }

    /// Local-originated update used by the REST API after it has already
    /// pushed the packet to devices itself
    /// (api/handlers/plugins/clipboard.rs:72-75). Newer timestamps win.
    pub fn update_content(&self, device_id: Option<&str>, content: String, timestamp: Option<i64>) {
        let ts = timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        {
            let mut state = self.core.write_state();
            if ts > state.timestamp || state.content.is_none() {
                state.content = Some(content.clone());
                state.timestamp = ts;
            } else {
                return;
            }
        }

        self.core
            .plugin_events
            .broadcast(PluginEvent::ClipboardUpdate {
                device_id: device_id.unwrap_or("local").to_string(),
                content,
            });
    }
}

#[async_trait::async_trait]
impl Plugin for ClipboardPlugin {
    fn name(&self) -> &str {
        "clipboard"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.clipboard".to_string(),
            "kdeconnect.clipboard.connect".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // We genuinely send both: kdeconnect.clipboard on local change
        // (watcher fan-out) and kdeconnect.clipboard.connect from
        // on_connected. Matches upstream declarations (kdeconnect-kde
        // kdeconnect_clipboard.json X-KdeConnect-OutgoingPacketType;
        // kdeconnect-android ClipboardPlugin.kt:113; gsconnect
        // clipboard.js:19-22).
        vec![
            "kdeconnect.clipboard".to_string(),
            "kdeconnect.clipboard.connect".to_string(),
        ]
    }

    fn is_backend_available(&self) -> bool {
        self.backend.read().map(|b| b.is_some()).unwrap_or(false)
    }
    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        // clipboard.connect is sent on connect with the last-known content and
        // its update timestamp (kdeconnect-kde clipboardplugin.cpp:39-42,
        // 141-152; kdeconnect-android ClipboardPlugin.kt:101-105). Like
        // GSConnect (clipboard.js:78-79) we skip when nothing has been
        // observed yet rather than pushing a fabricated body.
        let state = self.core.read_state();
        match (&state.content, state.timestamp) {
            (Some(content), ts) if !content.is_empty() && ts > 0 => vec![Packet::new(
                "kdeconnect.clipboard.connect".to_string(),
                // Wire shape: {"content", "timestamp"} — kdeconnect-android
                // ClipboardPlugin.kt:93-97 and :162-177.
                serde_json::json!({ "content": content, "timestamp": ts }),
            )],
            _ => vec![],
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        match packet.packet_type.as_str() {
            "kdeconnect.clipboard" => {
                let Some(content) = packet.body.get("content").and_then(|v| v.as_str()) else {
                    debug!(
                        device_id = %device_id,
                        event = "clipboard_no_content",
                        "kdeconnect.clipboard without content key — ignored"
                    );
                    return Ok(None);
                };

                info!(
                    device_id = %device_id,
                    len = content.len(),
                    event = "clipboard_received",
                    "Received clipboard content"
                );

                self.core.apply_remote(device_id, content.to_string());
                self.write_to_session(content).await;
            }
            "kdeconnect.clipboard.connect" => {
                // The timestamp gates acceptance; content is optional —
                // Android leaves the clipboard untouched when the key is
                // absent (ClipboardPlugin.kt:53). Our own REST API uses a
                // content-less {timestamp: 1} as a pull trigger
                // (api/handlers/plugins/clipboard.rs:106-109).
                let timestamp = packet
                    .body
                    .get("timestamp")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);

                let Some(content) = packet.body.get("content").and_then(|v| v.as_str()) else {
                    debug!(
                        device_id = %device_id,
                        event = "clipboard_connect_no_content",
                        "clipboard.connect without content key — nothing to apply"
                    );
                    return Ok(None);
                };

                info!(
                    device_id = %device_id,
                    len = content.len(),
                    timestamp = timestamp,
                    event = "clipboard_connect",
                    "Received clipboard connect packet"
                );

                if self
                    .core
                    .apply_remote_connect(device_id, content.to_string(), timestamp)
                {
                    self.write_to_session(content).await;
                }
            }
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::sync::Mutex;

    // -----------------------------------------------------------------
    // Mock session-clipboard backend: records writes, hands the watcher's
    // channel to the test so it can inject "local" clipboard changes.
    // -----------------------------------------------------------------
    struct MockClipboard {
        content: Mutex<Option<String>>,
        writes: Mutex<Vec<String>>,
        watcher_tx: Mutex<Option<UnboundedSender<String>>>,
        fail_spawns: Mutex<usize>,
    }

    impl MockClipboard {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                content: Mutex::new(None),
                writes: Mutex::new(Vec::new()),
                watcher_tx: Mutex::new(None),
                fail_spawns: Mutex::new(0),
            })
        }

        fn writes(&self) -> Vec<String> {
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        fn inject_change(&self, content: &str) {
            self.watcher_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .expect("watcher not started")
                .send(content.to_string())
                .expect("watcher channel closed");
        }

        fn fail_next_spawns(&self, n: usize) {
            *self.fail_spawns.lock().unwrap_or_else(|e| e.into_inner()) = n;
        }

        fn watcher_running(&self) -> bool {
            self.watcher_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        }
    }

    impl ClipboardBackend for MockClipboard {
        fn name(&self) -> &str {
            "mock"
        }

        fn set(&self, content: &str) -> Result<()> {
            *self.content.lock().unwrap_or_else(|e| e.into_inner()) = Some(content.to_string());
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(content.to_string());
            Ok(())
        }

        fn spawn_watcher(&self, tx: UnboundedSender<String>) -> Result<()> {
            {
                let mut fails = self.fail_spawns.lock().unwrap_or_else(|e| e.into_inner());
                if *fails > 0 {
                    *fails -= 1;
                    return Err(Error::io("mock spawn failure".to_string(), None));
                }
            }
            *self.watcher_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
            Ok(())
        }
    }

    fn make_plugin() -> (ClipboardPlugin, Arc<PluginEventBroadcaster>) {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "test"));
        (ClipboardPlugin::new(broadcaster.clone()), broadcaster)
    }

    fn make_plugin_with_mock() -> (ClipboardPlugin, Arc<MockClipboard>) {
        let (plugin, _) = make_plugin();
        let mock = MockClipboard::new();
        plugin.set_backend(mock.clone());
        (plugin, mock)
    }

    fn clipboard_packet(content: &str) -> Packet {
        Packet::new(
            "kdeconnect.clipboard".to_string(),
            serde_json::json!({ "content": content }),
        )
    }

    #[tokio::test]
    async fn test_clipboard_plugin() {
        let (plugin, _) = make_plugin();
        assert_eq!(plugin.name(), "clipboard");
    }

    #[tokio::test]
    async fn test_initially_empty() {
        let (plugin, _) = make_plugin();
        assert!(plugin.get_content().is_none());
    }

    // -----------------------------------------------------------------
    // Wire shapes (upstream-verified)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_local_change_packet_wire_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/clipboard/local_change.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read clipboard fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let (plugin, _) = make_plugin();
        let packet = plugin
            .core
            .apply_local_change("copied text".to_string())
            .expect("new local content must produce a packet");
        // kdeconnect.clipboard body is exactly {"content": ...} — no
        // timestamp, no extra keys (kdeconnect-android ClipboardPlugin.kt:77-81,
        // :151-160; kdeconnect-kde clipboardplugin.cpp:131-138).
        assert_eq!(packet.packet_type, "kdeconnect.clipboard");
        assert_eq!(packet.body, upstream_body);
    }

    #[tokio::test]
    async fn test_on_connected_wire_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/clipboard/connect.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read clipboard connect fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let (plugin, _) = make_plugin();
        // Use the fixture's literal content so the produced body matches
        // the upstream-derived shape field-for-field.
        plugin.core.apply_local_change(upstream_body["content"].as_str().unwrap().to_string());

        let packets = plugin.on_connected("dev-1");
        assert_eq!(packets.len(), 1);
        // clipboard.connect body is {"content", "timestamp"} — kdeconnect-android
        // ClipboardPlugin.kt:93-97, :162-177; kdeconnect-kde
        // clipboardplugin.cpp:148-151.
        assert_eq!(packets[0].packet_type, "kdeconnect.clipboard.connect");
        // Compare key set + content; timestamp is a current ms (variable).
        assert_eq!(packets[0].body["content"], upstream_body["content"]);
        assert!(packets[0].body["timestamp"].as_i64().unwrap() > 0);
        assert_eq!(packets[0].body.as_object().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_on_connected_empty_state_sends_nothing() {
        // GSConnect skips the connect packet until something has been observed
        // (gsconnect src/service/plugins/clipboard.js:78-79).
        let (plugin, _) = make_plugin();
        assert!(plugin.on_connected("dev-1").is_empty());
    }

    #[tokio::test]
    async fn test_capabilities_honest_both_directions() {
        // Both packet types both ways, matching kdeconnect-kde
        // kdeconnect_clipboard.json and ClipboardPlugin.kt:111-113.
        // kdeconnect.clipboard.file must NOT appear (no payload transfers).
        let (plugin, _) = make_plugin();
        let incoming = plugin.incoming_capabilities();
        let outgoing = plugin.outgoing_capabilities();
        for caps in [&incoming, &outgoing] {
            assert!(caps.contains(&"kdeconnect.clipboard".to_string()));
            assert!(caps.contains(&"kdeconnect.clipboard.connect".to_string()));
            assert!(!caps.contains(&"kdeconnect.clipboard.file".to_string()));
        }
    }

    // -----------------------------------------------------------------
    // Phone -> desktop
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_clipboard_writes_session_clipboard() {
        let (plugin, mock) = make_plugin_with_mock();
        plugin
            .handle_packet("phone", clipboard_packet("hello world"))
            .await
            .expect("Value expected to be present");

        assert_eq!(plugin.get_content().as_deref(), Some("hello world"));
        assert_eq!(mock.writes(), vec!["hello world".to_string()]);
    }

    #[tokio::test]
    async fn test_handle_clipboard_connect_newer_timestamp_written() {
        let (plugin, mock) = make_plugin_with_mock();
        let packet = Packet::new(
            "kdeconnect.clipboard.connect".to_string(),
            serde_json::json!({ "content": "synced text", "timestamp": 1_700_000_000_000i64 }),
        );
        plugin
            .handle_packet("phone", packet)
            .await
            .expect("Value expected to be present");

        assert_eq!(plugin.get_content().as_deref(), Some("synced text"));
        assert_eq!(mock.writes(), vec!["synced text".to_string()]);
    }

    #[tokio::test]
    async fn test_handle_clipboard_connect_stale_timestamp_ignored() {
        // Timestamp gate: 0 (unknown) or older than ours is ignored —
        // kdeconnect-android ClipboardPlugin.kt:48-52; kdeconnect-kde
        // clipboardplugin.cpp:188-194.
        let (plugin, mock) = make_plugin_with_mock();

        let fresh = Packet::new(
            "kdeconnect.clipboard.connect".to_string(),
            serde_json::json!({ "content": "fresh", "timestamp": 2000i64 }),
        );
        plugin.handle_packet("phone", fresh).await.unwrap();
        assert_eq!(plugin.get_content().as_deref(), Some("fresh"));

        let stale = Packet::new(
            "kdeconnect.clipboard.connect".to_string(),
            serde_json::json!({ "content": "stale", "timestamp": 1000i64 }),
        );
        plugin.handle_packet("phone", stale).await.unwrap();
        assert_eq!(plugin.get_content().as_deref(), Some("fresh"));

        let unknown = Packet::new(
            "kdeconnect.clipboard.connect".to_string(),
            serde_json::json!({ "content": "unknown-ts", "timestamp": 0i64 }),
        );
        plugin.handle_packet("phone", unknown).await.unwrap();
        assert_eq!(plugin.get_content().as_deref(), Some("fresh"));

        // Only the accepted packet reached the session clipboard.
        assert_eq!(mock.writes(), vec!["fresh".to_string()]);
    }

    #[tokio::test]
    async fn test_handle_clipboard_connect_without_content_ignored() {
        // Android leaves the clipboard untouched when "content" is absent
        // (ClipboardPlugin.kt:53) — this is also the pull-trigger form our own
        // REST API sends ({timestamp: 1}).
        let (plugin, mock) = make_plugin_with_mock();
        let packet = Packet::new(
            "kdeconnect.clipboard.connect".to_string(),
            serde_json::json!({ "timestamp": 1i64 }),
        );
        let result = plugin.handle_packet("phone", packet).await;
        assert!(result.is_ok());
        assert!(plugin.get_content().is_none());
        assert!(mock.writes().is_empty());
    }

    #[tokio::test]
    async fn test_degraded_backend_stores_without_writing() {
        // No backend: content is still stored + evented, nothing crashes
        // (mousepad.rs degradation pattern).
        let (plugin, broadcaster) = make_plugin();
        let mut rx = broadcaster.subscribe();
        plugin
            .handle_packet("phone", clipboard_packet("stored only"))
            .await
            .expect("Value expected to be present");
        assert_eq!(plugin.get_content().as_deref(), Some("stored only"));

        let event = rx.try_recv().expect("event expected");
        match event {
            PluginEvent::ClipboardUpdate { device_id, content } => {
                assert_eq!(device_id, "phone");
                assert_eq!(content, "stored only");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Desktop -> phone decision logic (echo suppression)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_own_write_echo_suppressed() {
        // Phone packet -> we write the session clipboard -> the watcher fires
        // with identical content -> must NOT produce a packet back.
        // Suppression is by content equality, state primed before the write:
        // kdeconnect-kde clipboardlistener.cpp:112-118 -> :103; GSConnect
        // clipboard.js:163-168 -> :144.
        let (plugin, _mock) = make_plugin_with_mock();
        plugin
            .handle_packet("phone", clipboard_packet("phone text"))
            .await
            .unwrap();

        assert!(
            plugin
                .core
                .apply_local_change("phone text".to_string())
                .is_none(),
            "our own write echoed by the watcher must not be re-sent"
        );
    }

    #[tokio::test]
    async fn test_duplicate_local_change_suppressed() {
        let (plugin, _) = make_plugin();
        assert!(plugin.core.apply_local_change("a".to_string()).is_some());
        assert!(plugin.core.apply_local_change("a".to_string()).is_none());
        assert!(plugin.core.apply_local_change("b".to_string()).is_some());
    }

    #[tokio::test]
    async fn test_empty_local_change_suppressed() {
        // GSConnect never propagates empty/non-text selections
        // (clipboard.js:149-156); kdeconnect-kde ignores empty changes
        // (clipboardlistener.cpp:103).
        let (plugin, _) = make_plugin();
        assert!(plugin.core.apply_local_change(String::new()).is_none());
    }

    #[tokio::test]
    async fn test_new_local_content_after_remote_is_sent() {
        let (plugin, _mock) = make_plugin_with_mock();
        plugin
            .handle_packet("phone", clipboard_packet("from phone"))
            .await
            .unwrap();

        let packet = plugin
            .core
            .apply_local_change("user copied something else".to_string())
            .expect("genuinely new local content must be sent");
        assert_eq!(
            packet.body,
            serde_json::json!({ "content": "user copied something else" })
        );
    }

    #[tokio::test]
    async fn test_local_change_broadcasts_event() {
        let (plugin, broadcaster) = make_plugin();
        let mut rx = broadcaster.subscribe();
        plugin.core.apply_local_change("local copy".to_string());
        let event = rx.try_recv().expect("event expected");
        match event {
            PluginEvent::ClipboardUpdate { device_id, content } => {
                assert_eq!(device_id, "local");
                assert_eq!(content, "local copy");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Legacy API-facing semantics (REST handler pushes packets itself)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_update_content_ignores_older_timestamp() {
        let (plugin, _) = make_plugin();
        plugin.update_content(None, "new content".to_string(), Some(2000));
        assert_eq!(plugin.get_content().as_deref(), Some("new content"));

        plugin.update_content(None, "old content".to_string(), Some(1000));
        assert_eq!(plugin.get_content().as_deref(), Some("new content"));

        plugin.update_content(None, "newer content".to_string(), Some(3000));
        assert_eq!(plugin.get_content().as_deref(), Some("newer content"));
    }

    // -----------------------------------------------------------------
    // End-to-end over a live in-process link: watcher fan-out + echo
    // suppression through the real connection-manager send path.
    // -----------------------------------------------------------------

    const LINK_OUR_ID: &str = "clip-client-aaaaaaaaaaaaaaaaaa";
    const LINK_PEER_ID: &str = "clip-peer-aaaaaaaaaaaaaaaaaaaa";

    async fn cm_with_live_link() -> (
        Arc<crate::protocol::ConnectionManager>,
        Arc<crate::protocol::ConnectionManager>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        use crate::protocol::{CertificateManager, ConnectionManager};

        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let certs = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
        certs.init().expect("Value expected to be present");
        let server_cm =
            Arc::new(ConnectionManager::new(certs.clone()).expect("Value expected to be present"));
        server_cm.set_device_identity(LINK_OUR_ID, "Us");
        let client_cm =
            Arc::new(ConnectionManager::new(certs).expect("Value expected to be present"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        let server = server_cm.clone();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            server
                .accept_test(LINK_PEER_ID.to_string(), stream)
                .await
                .expect("Value expected to be present");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        client_cm
            .connect(&LINK_PEER_ID.to_string(), addr)
            .await
            .expect("Value expected to be present");
        (client_cm, server_cm, handle, temp)
    }

    #[tokio::test]
    async fn test_watcher_change_fans_out_over_live_link() {
        let (client_cm, server_cm, server, _t) = cm_with_live_link().await;
        let (plugin, _) = make_plugin();
        let plugin = plugin.with_connection_manager(client_cm);
        let mock = MockClipboard::new();
        plugin.set_backend(mock.clone());

        mock.inject_change("live clipboard text");

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_cm.recv_packet(&LINK_PEER_ID.to_string()),
        )
        .await
        .expect("packet expected within timeout")
        .expect("Value expected to be present");

        assert_eq!(received.packet_type, "kdeconnect.clipboard");
        assert_eq!(
            received.body,
            serde_json::json!({ "content": "live clipboard text" })
        );

        server.abort();
    }

    #[tokio::test]
    async fn test_own_write_not_echoed_over_live_link() {
        let (client_cm, server_cm, server, _t) = cm_with_live_link().await;
        let (plugin, _) = make_plugin();
        let plugin = plugin.with_connection_manager(client_cm);
        let mock = MockClipboard::new();
        plugin.set_backend(mock.clone());

        // Phone pushes content; the plugin "writes" it (mock) and the watcher
        // then reports the same text — the echo of our own write.
        plugin
            .handle_packet("phone", clipboard_packet("echo me not"))
            .await
            .unwrap();
        mock.inject_change("echo me not");

        // Give the fan-out task a beat to (not) fire, then assert the link
        // stays silent.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            server_cm.recv_packet(&LINK_PEER_ID.to_string()),
        )
        .await;
        assert!(
            result.is_err(),
            "own clipboard write must not be echoed back to the phone"
        );

        server.abort();
    }

    #[tokio::test]
    async fn test_watcher_retry_after_spawn_failure() {
        // A failed spawn_watcher must NOT latch watcher_started: a later
        // retry (e.g. the session backend becomes available after startup)
        // has to be able to start the fan-out. Regression test for the flag
        // being stored before spawn_watcher ran.
        let (client_cm, server_cm, server, _t) = cm_with_live_link().await;
        let (plugin, _) = make_plugin();
        let plugin = plugin.with_connection_manager(client_cm);
        let mock = MockClipboard::new();
        mock.fail_next_spawns(1);
        plugin.set_backend(mock.clone());
        assert!(
            !mock.watcher_running(),
            "failed spawn must not leave a watcher behind"
        );

        plugin.set_backend(mock.clone());
        assert!(
            mock.watcher_running(),
            "retry after a spawn failure must start the watcher"
        );

        mock.inject_change("after retry");
        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_cm.recv_packet(&LINK_PEER_ID.to_string()),
        )
        .await
        .expect("packet expected within timeout")
        .expect("Value expected to be present");
        assert_eq!(received.packet_type, "kdeconnect.clipboard");
        assert_eq!(
            received.body,
            serde_json::json!({ "content": "after retry" })
        );

        server.abort();
    }

    // -----------------------------------------------------------------
    // Watcher supervision (restart on unexpected exit, not on shutdown)
    // -----------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn test_supervise_restarts_failures_until_shutdown() {
        // Time is paused so the backoff sleeps complete instantly; the
        // assertion is on how many runs the supervisor drove.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let calls_in = calls.clone();
        supervise_watcher(tx, move |_tx| {
            let calls = calls_in.clone();
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                    WatcherExit::Failed("boom".to_string())
                } else {
                    WatcherExit::Shutdown
                }
            }
        })
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "supervisor must restart after each unexpected exit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_supervise_does_not_restart_clean_shutdown() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let calls_in = calls.clone();
        supervise_watcher(tx, move |_tx| {
            let calls = calls_in.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                WatcherExit::Shutdown
            }
        })
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "clean shutdown must not trigger a restart"
        );
    }
}

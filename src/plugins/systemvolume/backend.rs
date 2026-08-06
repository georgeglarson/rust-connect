//! SystemVolume backend seam
//!
//! Single Responsibility: own the PulseAudio/PipeWire surface used by the
//! systemvolume provider. The trait is the seam tests use to drive the
//! plugin without a live audio daemon; the real impl shells out to `pactl`,
//! matching the rest of the codebase (see `findthisdevice.rs`: "pactl is the
//! only PA surface this codebase uses"). PipeWire sessions are covered
//! because pipewire-pulse speaks the same PA protocol; no separate
//! library/crate needed.
//!
//! Wire / behavior contract comes from upstream kdeconnect-kde
//! `plugins/systemvolume/systemvolumeplugin-pulse.cpp`:
//! - `sinkList` shape (pulse.cpp:90-95): `name`, `description`, `volume` (int,
//!   absolute scale, ceiling = `maxVolume`), `muted`, `enabled` (= isDefault).
//! - `maxVolume` is `PulseAudioQt::normalVolume()` == 65536 (:94).
//! - Hot events: per-sink `volume`/`muted`/`enabled` deltas
//!   (pulse.cpp:69-88); full list rebuild on `sinkAdded`/`sinkRemoved`
//!   (:109-115).
//!
//! Discovery only: `pactl --format=json list sinks` (the jq-free PA surface
//! preferred by the codebase). Subscription: long-running `pactl subscribe`
//! child, supervised with exponential backoff (clipboard.rs's
//! `supervise_watcher` pattern).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::utils::errors::Result;

/// One PulseAudio sink as the desktop exposes it. Mirrors the wire shape
/// upstream `kdeconnect-kde` puts in `sinkList`
/// (systemvolumeplugin-pulse.cpp:90-95) and the fields the android phone
/// reads back (`Sink.kt:26-32`). Internal numerical scale: PA volume units,
/// ceiling is `max_volume` (PulseAudioQt::normalVolume() == 65536,
/// pulse.cpp:94).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSinkState {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub volume: Option<i64>,
    #[serde(default)]
    pub max_volume: Option<i64>,
    #[serde(default)]
    pub muted: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Delta from `pactl subscribe`. The subscription only emits a short event
/// line; the actual state is re-queried via `list_sinks` so the plugin does
/// not have to maintain a second source of truth. Sink names are
/// `Option<String>` because pipewire-pulse emits `Event 'change' on sink #N`
/// without a quoted name; the name is informational only — the handler
/// re-lists on every classified event regardless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscribeEvent {
    SinkAdded {
        name: Option<String>,
    },
    SinkRemoved {
        name: Option<String>,
    },
    SinkChanged {
        name: Option<String>,
    },
    /// Default output may have moved (PulseAudio emits `on server` events
    /// for this). The event line itself never carries a name.
    DefaultSinkChanged {
        name: Option<String>,
    },
    /// Any other event string we observed but did not classify. The
    /// plugin logs these once; the next list-sinks refresh still covers it.
    Unclassified {
        line: String,
    },
}

/// Backend seam. The real impl is `PactlBackend`; tests use a recording
/// mock. Every method is "best effort": a failure returns `Err` and the
/// plugin degrades to a log event, matching the codebase's other
/// backend-bearing plugins (clipboard, mpris, findthisdevice).
#[async_trait::async_trait]
pub trait VolumeBackend: Send + Sync {
    /// Stable name for logs ("pactl", "mock").
    fn name(&self) -> &str;

    /// Whether the backend is alive as of now. `pactl` calls (list,
    /// subscribe) are the cheapest probe without a long-lived state. For
    /// the mock this is always true while the test holds it.
    fn is_available(&self) -> bool;

    /// Snapshot of every sink the backend currently knows about. The
    /// plugin pushes this on `requestSinks` and on hot add/remove.
    async fn list_sinks(&self) -> Result<Vec<LocalSinkState>>;

    /// Set the absolute volume on the sink. The plugin never scales
    /// (pulse.cpp:44 - 45 reads the wire value as `int` and calls PA
    /// `setVolume(volume)`); the float-scale mapping is the only point
    /// at which a fraction would belong, and pactl accepts the integer
    /// scale directly.
    async fn set_volume(&self, name: &str, volume: i64) -> Result<()>;

    /// Toggle mute on the sink.
    async fn set_muted(&self, name: &str, muted: bool) -> Result<()>;

    /// Set/clear the default sink. `enabled=true` means "make this sink
    /// the default output" (pulse.cpp:84-87 reads the wire `enabled` as
    /// `isDefault`).
    async fn set_default(&self, name: &str, enabled: bool) -> Result<()>;

    /// Spawn the subscription. Failures return Err; the supervisor
    /// (plugin-side) restarts the run with backoff. The implementer
    /// owns the child process: `kill_on_drop` is fine.
    async fn start_subscribe(&self, tx: UnboundedSender<SubscribeEvent>) -> Result<()>;
}

// ---------------------------------------------------------------------------
// pactl backend
// ---------------------------------------------------------------------------

/// Default command line. The provider path never reads packets or replies
/// to them; it only talks to the local PA daemon. Tests inject a different
/// `pactl_path` (typically a shell script wrapper) so they can fake the
/// JSON/list output without a running daemon.
pub struct PactlBackend {
    /// Path to the `pactl` binary, or a wrapper. Default = `pactl` on PATH.
    pactl_path: PathBuf,
    /// Cached max_volume (PA `PA_VOLUME_NORM`). Avoids one `--format=json
    /// info` round-trip per `list_sinks` call. Defaults to 65536
    /// (PulseAudioQt::normalVolume(), pulse.cpp:94).
    max_volume: i64,
    /// Long-lived `pactl subscribe` child when present. The supervisor
    /// task (in `mod.rs`) owns spawning/killing; this field is `None`
    /// when subscribe is not running.
    subscribe_child: Mutex<Option<Arc<Mutex<tokio::process::Child>>>>,
    /// Stop signal for the supervisor task. Set true to break the loop
    /// cleanly when the plugin is dropped or the backend is replaced.
    shutdown: Arc<AtomicBool>,
}

impl Default for PactlBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PactlBackend {
    pub fn new() -> Self {
        Self::with_path(PathBuf::from("pactl"))
    }

    pub fn with_path(pactl_path: PathBuf) -> Self {
        Self {
            pactl_path,
            max_volume: 65_536,
            subscribe_child: Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn shutdown_handle(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    pub fn kill_subscribe(&self) {
        if let Some(child_arc) = self
            .subscribe_child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let mut child = child_arc.lock().unwrap_or_else(|e| e.into_inner());
            let _ = child.start_kill();
        }
    }

    /// Run `pactl` with given args, returning stdout (UTF-8 lossy).
    async fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.pactl_path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| {
                crate::utils::errors::Error::io(
                    format!("failed to spawn pactl: {e}"),
                    None::<String>,
                )
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            return Err(crate::utils::errors::Error::io(
                format!(
                    "pactl {} exited with status {:?}: {}",
                    args.join(" "),
                    out.status.code(),
                    stderr.trim()
                ),
                None::<String>,
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `pactl get-default-sink` prints a single sink name. `None` when the
    /// PA daemon is gone; callers treat that as "unknown default", not an
    /// error — the list itself still succeeded.
    async fn default_sink_name(&self) -> Option<String> {
        self.run(&["get-default-sink"])
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// Raw shape of a single sink entry from `pactl --format=json list sinks`.
/// Captured against pactl 17.0 with pipewire-pulse on 2026-08-05: pactl
/// 15+ emits a BARE JSON ARRAY of these objects (no wrapping key), `mute`
/// is a JSON boolean, and `volume` is a per-channel map of channel name to
/// `{ value, value_percent, db }` where `value` is already on PA's absolute
/// scale (PA_VOLUME_NORM = 65536 == 100%). Sink entries carry no default
/// flag; the default sink comes from `pactl get-default-sink`. Only the
/// fields used by the wire contract are decoded; unknown ones are ignored.
#[derive(Debug, Clone, Deserialize)]
struct PactlSink {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    volume: Option<std::collections::BTreeMap<String, PactlChannelVolume>>,
    #[serde(default)]
    mute: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct PactlChannelVolume {
    #[serde(default)]
    value: Option<i64>,
}

/// Default fallback normal volume for PA. PulseAudioQt::normalVolume()
/// returns 65536 (systemvolumeplugin-pulse.cpp:94) — the same scale pactl's
/// per-channel `value` integers already use, so no rescaling is needed.
#[allow(dead_code)]
const PA_VOLUME_NORM: i64 = 65_536;

impl PactlSink {
    fn to_state(&self, max_volume: i64) -> LocalSinkState {
        // The wire payload wants one integer per sink with ceiling
        // `maxVolume` (systemvolumeplugin-pulse.cpp:90-95). PA's sink volume
        // is the max across channels, and `pactl set-sink-volume` applies one
        // value to every channel, so max is the honest representative.
        let volume = self
            .volume
            .as_ref()
            .map(|chans| chans.values().filter_map(|c| c.value).max().unwrap_or(0))
            .unwrap_or(0);
        LocalSinkState {
            name: self.name.clone(),
            description: self.description.clone(),
            volume: Some(volume),
            max_volume: Some(max_volume),
            muted: self.mute,
            // Filled in by `mark_default` from `pactl get-default-sink`;
            // pactl sink entries themselves carry no default flag.
            enabled: None,
        }
    }
}

/// Set `enabled` on exactly the sink whose name matches the default. A
/// missing default (daemon gone mid-call) leaves every row at `None`.
fn mark_default(states: &mut [LocalSinkState], default: Option<&str>) {
    if let Some(def) = default {
        for st in states.iter_mut() {
            st.enabled = Some(st.name == def);
        }
    }
}

#[async_trait::async_trait]
impl VolumeBackend for PactlBackend {
    fn name(&self) -> &str {
        "pactl"
    }

    fn is_available(&self) -> bool {
        // Cheap probe: ask for the default sink. If the daemon is up the
        // call succeeds; if not it fails. We don't cache the result —
        // the subscription is the live signal, and list_sinks/set_* will
        // surface a real failure at use time.
        std::process::Command::new(&self.pactl_path)
            .args(["get-default-sink"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn list_sinks(&self) -> Result<Vec<LocalSinkState>> {
        let stdout = self.run(&["--format=json", "list", "sinks"]).await?;
        // pactl 15+ emits a bare JSON array of sink objects.
        let sinks: Vec<PactlSink> = serde_json::from_str(&stdout).map_err(|e| {
            crate::utils::errors::Error::io(
                format!("pactl list sinks JSON parse failed: {e}"),
                None::<String>,
            )
        })?;
        let default = self.default_sink_name().await;
        let mut states: Vec<LocalSinkState> = sinks
            .into_iter()
            .map(|s| s.to_state(self.max_volume))
            .collect();
        mark_default(&mut states, default.as_deref());
        Ok(states)
    }

    async fn set_volume(&self, name: &str, volume: i64) -> Result<()> {
        // The wire volume is already on PA's absolute scale (ceiling
        // 65536); `pactl set-sink-volume` accepts it directly, matching
        // upstream's write path (systemvolumeplugin-pulse.cpp:44-45).
        let volume_str = volume.to_string();
        self.run(&["set-sink-volume", name, &volume_str]).await?;
        Ok(())
    }

    async fn set_muted(&self, name: &str, muted: bool) -> Result<()> {
        let flag = if muted { "1" } else { "0" };
        self.run(&["set-sink-mute", name, flag]).await?;
        Ok(())
    }

    async fn set_default(&self, name: &str, enabled: bool) -> Result<()> {
        if !enabled {
            // PA has no "unset default"; "set-default-sink" picks one.
            // We accept the asymmetry: the wire only ever sets a different
            // sink as default (toggle off would be a no-op against the
            // current default, which stays default).
            return Ok(());
        }
        self.run(&["set-default-sink", name]).await?;
        Ok(())
    }

    async fn start_subscribe(&self, tx: UnboundedSender<SubscribeEvent>) -> Result<()> {
        // kill any previous run
        self.kill_subscribe();

        let mut child = Command::new(&self.pactl_path)
            .args(["subscribe"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                crate::utils::errors::Error::io(
                    format!("failed to spawn pactl subscribe: {e}"),
                    None::<String>,
                )
            })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            crate::utils::errors::Error::io(
                "pactl subscribe stdout not piped".to_string(),
                None::<String>,
            )
        })?;
        let child_arc = Arc::new(Mutex::new(child));
        *self
            .subscribe_child
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(child_arc.clone());

        let shutdown = self.shutdown.clone();
        let child_for_task = child_arc.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    _ = shutdown.notified() => {
                        debug!(event = "pactl_subscribe_shutdown", "subscribe task observed shutdown");
                        break;
                    }
                    line = lines.next_line() => match line {
                        Ok(Some(text)) => {
                            if let Some(ev) = parse_subscribe_line(&text) {
                                if tx.send(ev).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(None) => {
                            // pactl subscribe exited (daemon gone,
                            // process killed). End the task; the
                            // supervisor will restart.
                            let mut c = child_for_task.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = c.start_kill();
                            break;
                        }
                        Err(e) => {
                            warn!(error = %e, event = "pactl_subscribe_read_failed", "pactl subscribe stdout read failed");
                            break;
                        }
                    }
                }
            }
        });
        Ok(())
    }
}

/// One kafka-light handler: parse a single `pactl subscribe` line.
/// Examples (per `pactl(1)` and live captures on PulseAudio + pipewire):
///   `Event 'change' on sink #0 'alsa_output.pci-0000_00_1f.3.analog-stereo'`
///   `Event 'change' on sink #2516` (pipewire-pulse omits the name)
///   `Event 'new' on sink #1 '...'` / `Event 'remove' on sink #2 '...'`
///   `Event 'change' on server` (default output may have moved)
///   `Event 'change' on card #55` (pipewire-pulse suspended-sink changes)
pub(crate) fn parse_subscribe_line(line: &str) -> Option<SubscribeEvent> {
    let line = line.trim();
    if !line.starts_with("Event ") {
        return None;
    }
    // `Event 'EVENT' on sink #N 'NAME'`
    let after_event = line.strip_prefix("Event ")?;
    let (event_type, rest) = quoted_field(after_event)?;
    let rest = rest.trim_start();
    // Target dialects (captured live 2026-08-06):
    // - PulseAudio: `Event 'change' on sink #0 'NAME'`, `... on server`.
    // - pipewire-pulse: `Event 'change' on sink #2516` WITHOUT a quoted
    //   name, and volume/mute changes on SUSPENDED sinks arrive only as
    //   `Event 'change' on card #N`. Card hotplug implies sink
    //   add/remove, so every card event triggers a re-list too. The
    //   handler re-queries the full list on any classified event, so
    //   over-triggering is cheap and idempotent.
    let after_on = rest.strip_prefix("on ")?.trim_start();
    if let Some(after_sink) = after_on.strip_prefix("sink ") {
        // after_sink = "#N" or "#N 'NAME'" (name is optional).
        let after_sink = after_sink.trim_start();
        if !after_sink.starts_with('#') {
            return Some(SubscribeEvent::Unclassified {
                line: line.to_string(),
            });
        }
        let name = after_sink
            .split_once(' ')
            .map(|(_, tail)| tail.trim_start())
            .and_then(|tail| quoted_field(tail).map(|(n, _)| n.to_string()));
        return match event_type {
            "new" => Some(SubscribeEvent::SinkAdded { name }),
            "remove" => Some(SubscribeEvent::SinkRemoved { name }),
            "change" => Some(SubscribeEvent::SinkChanged { name }),
            _ => Some(SubscribeEvent::Unclassified {
                line: line.to_string(),
            }),
        };
    }
    if after_on.starts_with("server") {
        // Default output may have moved; the re-list picks up the new
        // default via `pactl get-default-sink`.
        return match event_type {
            "change" | "new" => Some(SubscribeEvent::DefaultSinkChanged { name: None }),
            _ => Some(SubscribeEvent::Unclassified {
                line: line.to_string(),
            }),
        };
    }
    if after_on
        .strip_prefix("card ")
        .map(str::trim_start)
        .is_some_and(|s| s.starts_with('#'))
    {
        return match event_type {
            "new" | "remove" | "change" => Some(SubscribeEvent::SinkChanged { name: None }),
            _ => Some(SubscribeEvent::Unclassified {
                line: line.to_string(),
            }),
        };
    }
    // client, source, sink-input, sample-cache, …
    Some(SubscribeEvent::Unclassified {
        line: line.to_string(),
    })
}

/// Consume `Event 'X' on sink #N 'Y'` walking from the start. Returns the
/// quoted field and the tail.
fn quoted_field(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix('\'')?;
    let end = s.find('\'')?;
    let field = &s[..end];
    let rest = &s[end + 1..];
    Some((field, rest))
}

/// Try to detect a working pactl backend. Returns `Some` when pactl is on
/// PATH and answers `get-default-sink`; `None` otherwise. Called from the
/// production entry point (bootstrap.rs create_state).
pub fn detect() -> Option<PactlBackend> {
    if !which_exists("pactl") {
        return None;
    }
    let backend = PactlBackend::new();
    if backend.is_available() {
        Some(backend)
    } else {
        None
    }
}

fn which_exists(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Mock backend for tests
// ---------------------------------------------------------------------------

/// Recording mock backend. Tests inject this into the plugin instead of
/// `PactlBackend` and drive the API surface without a real PA daemon.
pub struct MockBackend {
    pub sinks: Mutex<Vec<LocalSinkState>>,
    pub default_name: Mutex<Option<String>>,
    pub calls: Mutex<Vec<String>>,
    pub available: AtomicBool,
    pub force_subscribe_error: AtomicBool,
    pub event_tx: Mutex<Option<UnboundedSender<SubscribeEvent>>>,
    pub name: &'static str,
}

impl MockBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sinks: Mutex::new(Vec::new()),
            default_name: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            available: AtomicBool::new(true),
            force_subscribe_error: AtomicBool::new(false),
            event_tx: Mutex::new(None),
            name: "mock",
        })
    }

    pub fn with_sinks(self: &Arc<Self>, sinks: Vec<LocalSinkState>) -> Arc<Self> {
        *self.sinks.lock().unwrap_or_else(|e| e.into_inner()) = sinks;
        self.clone()
    }

    pub fn with_default(self: &Arc<Self>, name: &str) -> Arc<Self> {
        *self.default_name.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
        self.clone()
    }

    pub fn set_available(&self, v: bool) {
        self.available.store(v, Ordering::SeqCst);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Emit one synthetic subscribe event as if pactl had written it.
    pub fn push_event(&self, ev: SubscribeEvent) {
        let tx_opt = self
            .event_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx_opt {
            let _ = tx.send(ev);
        }
    }
}

#[async_trait::async_trait]
impl VolumeBackend for MockBackend {
    fn name(&self) -> &str {
        self.name
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    async fn list_sinks(&self) -> Result<Vec<LocalSinkState>> {
        if !self.is_available() {
            return Err(crate::utils::errors::Error::io(
                "mock backend unavailable".to_string(),
                None::<String>,
            ));
        }
        let default = self
            .default_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ok(self
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .map(|mut s| {
                s.enabled = Some(default.as_deref() == Some(s.name.as_str()));
                s
            })
            .collect())
    }

    async fn set_volume(&self, name: &str, volume: i64) -> Result<()> {
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        let mut found = false;
        for s in sinks.iter_mut() {
            if s.name == name {
                s.volume = Some(volume);
                found = true;
            }
        }
        if !found {
            return Err(crate::utils::errors::Error::not_found(
                "sink",
                Some(name.to_string()),
            ));
        }
        drop(sinks);
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("set_volume:{name}:{volume}"));
        Ok(())
    }

    async fn set_muted(&self, name: &str, muted: bool) -> Result<()> {
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        let mut found = false;
        for s in sinks.iter_mut() {
            if s.name == name {
                s.muted = Some(muted);
                found = true;
            }
        }
        if !found {
            return Err(crate::utils::errors::Error::not_found(
                "sink",
                Some(name.to_string()),
            ));
        }
        drop(sinks);
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("set_muted:{name}:{muted}"));
        Ok(())
    }

    async fn set_default(&self, name: &str, enabled: bool) -> Result<()> {
        if enabled {
            *self.default_name.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
        } else if self
            .default_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(name)
        {
            *self.default_name.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("set_default:{name}:{enabled}"));
        Ok(())
    }

    async fn start_subscribe(&self, tx: UnboundedSender<SubscribeEvent>) -> Result<()> {
        if self.force_subscribe_error.load(Ordering::SeqCst) {
            return Err(crate::utils::errors::Error::io(
                "mock subscribe start failed".to_string(),
                None::<String>,
            ));
        }
        *self.event_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        Ok(())
    }
}

/// A non-blocking `notified` helper for the shutdown AtomicBool. tokio's
/// `Notify` doesn't compose with `AtomicBool`, so we wrap one here.
trait AtomicBoolNotifyExt {
    async fn notified(&self);
}

impl AtomicBoolNotifyExt for Arc<AtomicBool> {
    async fn notified(&self) {
        // Resolve ONLY when the poison flag is actually raised. This arm of
        // the reader's select! must stay pending for the lifetime of the
        // subscription otherwise; returning early here kills the subscribe
        // loop and puts the supervisor in a restart churn (caught live:
        // a one-shot sleep made the reader exit ~10x/second).
        while !self.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    // ---------- pactl line parser ----------

    #[test]
    fn parse_subscribe_change_event() {
        let line = "Event 'change' on sink #0 'alsa_output.pci-0000_00_1f.3.analog-stereo'";
        match parse_subscribe_line(line).expect("parsed") {
            SubscribeEvent::SinkChanged { name } => {
                assert_eq!(
                    name.as_deref(),
                    Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
                );
            }
            other => panic!("expected SinkChanged, got {other:?}"),
        }
    }

    /// pipewire-pulse dialect captured live 2026-08-06: no quoted name.
    #[test]
    fn parse_subscribe_change_event_pipewire_no_name() {
        let line = "Event 'change' on sink #2516";
        match parse_subscribe_line(line).expect("parsed") {
            SubscribeEvent::SinkChanged { name } => assert_eq!(name, None),
            other => panic!("expected SinkChanged, got {other:?}"),
        }
    }

    /// pipewire-pulse reports volume/mute changes on suspended sinks only
    /// as card events (captured live 2026-08-06); they must trigger a
    /// re-list like sink events do.
    #[test]
    fn parse_subscribe_card_event_maps_to_sink_change() {
        let line = "Event 'change' on card #55";
        match parse_subscribe_line(line).expect("parsed") {
            SubscribeEvent::SinkChanged { name } => assert_eq!(name, None),
            other => panic!("expected SinkChanged, got {other:?}"),
        }
    }

    /// PulseAudio emits `on server` when the default output moves.
    #[test]
    fn parse_subscribe_server_event_maps_to_default_change() {
        let line = "Event 'change' on server";
        match parse_subscribe_line(line).expect("parsed") {
            SubscribeEvent::DefaultSinkChanged { name } => assert_eq!(name, None),
            other => panic!("expected DefaultSinkChanged, got {other:?}"),
        }
    }

    #[test]
    fn parse_subscribe_new_event() {
        let line = "Event 'new' on sink #42 'foo'";
        assert!(matches!(
            parse_subscribe_line(line).expect("parsed"),
            SubscribeEvent::SinkAdded { name } if name.as_deref() == Some("foo")
        ));
    }

    #[test]
    fn parse_subscribe_remove_event() {
        let line = "Event 'remove' on sink #42 'foo'";
        assert!(matches!(
            parse_subscribe_line(line).expect("parsed"),
            SubscribeEvent::SinkRemoved { name } if name.as_deref() == Some("foo")
        ));
    }

    #[test]
    fn parse_subscribe_unclassified_event_passes_through() {
        let line = "Event 'change' on client #3693";
        // client events are not sink state; surface them as Unclassified.
        match parse_subscribe_line(line).expect("parsed") {
            SubscribeEvent::Unclassified { .. } => {}
            other => panic!("expected Unclassified, got {other:?}"),
        }
    }

    #[test]
    fn parse_subscribe_garbage_returns_none() {
        assert!(parse_subscribe_line("random noise").is_none());
        assert!(parse_subscribe_line("").is_none());
    }

    // ---------- pactl JSON shape ----------

    /// Real `pactl --format=json list sinks` output captured 2026-08-05 on
    /// this project's development machine (pactl 17.0, pipewire-pulse,
    /// Fedora 43): a bare JSON array, boolean `mute`, per-channel volume
    /// map with integer `value` already on PA's 65536 scale. Do not edit
    /// this fixture by hand — re-capture from a live pactl and trim.
    const REAL_PACTL_LIST_SINKS: &str = r#"[
        {
            "index": 68,
            "name": "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Headphones__sink",
            "description": "700 Series Chipset Family HD Audio Headphones",
            "mute": false,
            "volume": {
                "front-left": {
                    "value": 32760,
                    "value_percent": "50%",
                    "db": "-18.07 dB"
                },
                "front-right": {
                    "value": 32760,
                    "value_percent": "50%",
                    "db": "-18.07 dB"
                }
            },
            "state": "SUSPENDED"
        },
        {
            "index": 2516,
            "name": "alsa_output.pci-0000_01_00.1.hdmi-stereo",
            "description": "AD104 High Definition Audio Controller Digital Stereo (HDMI)",
            "mute": false,
            "volume": {
                "front-left": {
                    "value": 29437,
                    "value_percent": "45%",
                    "db": "-20.86 dB"
                },
                "front-right": {
                    "value": 29437,
                    "value_percent": "45%",
                    "db": "-20.86 dB"
                }
            },
            "state": "SUSPENDED"
        }
    ]"#;

    #[test]
    fn parse_pactl_sink_list_json() {
        let parsed: Vec<PactlSink> = serde_json::from_str(REAL_PACTL_LIST_SINKS).expect("parse");
        assert_eq!(parsed.len(), 2);
        let converted: Vec<LocalSinkState> =
            parsed.into_iter().map(|s| s.to_state(65_536)).collect();
        assert_eq!(
            converted[0].name,
            "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Headphones__sink"
        );
        // pactl reports per-channel PA integers; we take the max (32760).
        assert_eq!(converted[0].volume, Some(32_760));
        assert_eq!(converted[0].muted, Some(false));
        // Parsing alone cannot know the default; mark_default does.
        assert_eq!(converted[0].enabled, None);
        assert_eq!(converted[1].volume, Some(29_437));
        assert_eq!(converted[1].muted, Some(false));
    }

    #[test]
    fn parse_pactl_sink_list_unknown_fields_ignored() {
        let raw = r#"[{
            "index": 1,
            "name": "s",
            "description": "d",
            "mute": false,
            "volume": { "front-left": { "value": 65536, "value_percent": "100%", "db": "0.00 dB" } },
            "state": "RUNNING",
            "future_field": "ignored"
        }]"#;
        let parsed: Vec<PactlSink> = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].clone().to_state(65_536).volume, Some(65_536));
    }

    #[test]
    fn parse_pactl_sink_list_missing_volume() {
        let raw = r#"[{ "name": "s", "description": "d", "mute": true }]"#;
        let parsed: Vec<PactlSink> = serde_json::from_str(raw).expect("parse");
        let state = parsed[0].clone().to_state(65_536);
        assert_eq!(state.volume, Some(0));
        assert_eq!(state.muted, Some(true));
    }

    #[test]
    fn mark_default_sets_enabled_only_on_match() {
        let mut states = vec![
            LocalSinkState {
                name: "a".to_string(),
                description: None,
                volume: Some(0),
                max_volume: Some(65_536),
                muted: None,
                enabled: None,
            },
            LocalSinkState {
                name: "b".to_string(),
                description: None,
                volume: Some(0),
                max_volume: Some(65_536),
                muted: None,
                enabled: None,
            },
        ];
        mark_default(&mut states, Some("b"));
        assert_eq!(states[0].enabled, Some(false));
        assert_eq!(states[1].enabled, Some(true));
        // None is a no-op on an already-marked list; production only ever
        // marks a freshly parsed list, where None leaves everything at
        // `enabled: None` (unknown default).
        mark_default(&mut states, None);
        assert_eq!(states[0].enabled, Some(false));
        assert_eq!(states[1].enabled, Some(true));
    }

    // ---------- shutdown flag ----------

    /// Regression: `notified()` must stay pending while the flag is false.
    /// The live defect was a one-shot sleep that resolved unconditionally,
    /// killing the pactl subscribe reader ~10x/second.
    #[tokio::test(start_paused = true)]
    async fn shutdown_notified_stays_pending_until_flag_raised() {
        let flag = Arc::new(AtomicBool::new(false));
        let waiter = flag.clone();
        let handle = tokio::spawn(async move {
            waiter.notified().await;
        });
        // Let the poll loop run several iterations; an early-returning
        // notified() would finish the task immediately.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !handle.is_finished(),
            "notified() must stay pending while the flag is false"
        );
        flag.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            handle.is_finished(),
            "notified() must resolve once the flag is raised"
        );
    }

    // ---------- MockBackend ----------

    #[tokio::test]
    async fn mock_backend_list_sinks_returns_clone() {
        let sinks = vec![LocalSinkState {
            name: "foo".to_string(),
            description: Some("Foo".to_string()),
            volume: Some(32_768),
            max_volume: Some(65_536),
            muted: Some(false),
            enabled: Some(true),
        }];
        let backend = MockBackend::new().with_sinks(sinks).with_default("foo");
        let got = backend.list_sinks().await.expect("list");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "foo");
        assert_eq!(got[0].enabled, Some(true));
    }

    #[tokio::test]
    async fn mock_backend_set_volume_records_call() {
        let backend = MockBackend::new().with_sinks(vec![LocalSinkState {
            name: "foo".to_string(),
            ..Default::default()
        }]);
        backend.set_volume("foo", 12345).await.expect("set");
        assert_eq!(backend.calls(), vec!["set_volume:foo:12345".to_string()]);
    }

    #[tokio::test]
    async fn mock_backend_set_volume_unknown_sink_errors() {
        let backend = MockBackend::new();
        let err = backend.set_volume("ghost", 1).await.unwrap_err();
        // expects NOT_FOUND
        let _ = err;
    }

    #[tokio::test]
    async fn mock_backend_subscribe_starts_and_accepts_events() {
        let backend = MockBackend::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubscribeEvent>();
        backend.start_subscribe(tx).await.expect("start");
        backend.push_event(SubscribeEvent::SinkAdded {
            name: Some("x".to_string()),
        });
        let ev = rx.recv().await.expect("event");
        assert!(matches!(ev, SubscribeEvent::SinkAdded { name } if name.as_deref() == Some("x")));
    }

    #[tokio::test]
    async fn mock_backend_unavailable_blocks_list() {
        let backend = MockBackend::new();
        backend.set_available(false);
        let err = backend.list_sinks().await.unwrap_err();
        let _ = err;
    }
}

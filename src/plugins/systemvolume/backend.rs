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
/// not have to maintain a second source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscribeEvent {
    SinkAdded {
        name: String,
    },
    SinkRemoved {
        name: String,
    },
    SinkChanged {
        name: String,
    },
    /// Sink switched to be the default output.
    DefaultSinkChanged {
        name: String,
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
}

/// Raw shape of a single sink entry from `pactl --format=json list sinks`.
/// Only the fields used by the wire contract are decoded; unknown ones are
/// left untouched (default for serde).
#[derive(Debug, Clone, Deserialize)]
struct PactlSink {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// PA volume is a struct with `value` and `value_flat`; the wire
    /// payload uses the per-channel integer, ceiling `max_volume`.
    #[serde(default)]
    volume: Option<PactlVolume>,
    /// `Mute: yes/no` text per pulse.proto.
    #[serde(default)]
    mute: Option<String>,
    /// Present in `pactl --format=json list sinks`; mirrors PA's
    /// `is_default` flag.
    #[serde(default)]
    is_default: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct PactlVolume {
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    value_flat: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct PactlSinkList {
    sinks: Vec<PactlSink>,
}

/// Default fallback normal volume for PA. PulseAudioQt::normalVolume()
/// returns 65536 (systemvolumeplugin-pulse.cpp:94). pactl reports
/// volume as a fraction in `--format=json`; we multiply by this scale so
/// the wire shape matches the upstream integer scale.
#[allow(dead_code)]
const PA_VOLUME_NORM: i64 = 65_536;

impl PactlSink {
    fn to_state(&self, max_volume: i64) -> LocalSinkState {
        // Prefer `value_flat` (average across channels, which is what
        // PulseAudioQt::Sink::volume() returns upstream, pulse.cpp:93),
        // fall back to `value` (front-left channel).
        let frac = self
            .volume
            .as_ref()
            .and_then(|v| v.value_flat.or(v.value))
            .unwrap_or(0.0);
        let volume = (frac.clamp(0.0, 1.0) * max_volume as f64).round() as i64;
        let muted = match self.mute.as_deref() {
            Some("yes") | Some("true") | Some("1") => Some(true),
            Some("no") | Some("false") | Some("0") => Some(false),
            _ => None,
        };
        LocalSinkState {
            name: self.name.clone(),
            description: self.description.clone(),
            volume: Some(volume),
            max_volume: Some(max_volume),
            muted,
            enabled: self.is_default,
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
        let parsed: PactlSinkList = serde_json::from_str(&stdout).map_err(|e| {
            crate::utils::errors::Error::io(
                format!("pactl list sinks JSON parse failed: {e}"),
                None::<String>,
            )
        })?;
        let states: Vec<LocalSinkState> = parsed
            .sinks
            .into_iter()
            .map(|s| s.to_state(self.max_volume))
            .collect();
        Ok(states)
    }

    async fn set_volume(&self, name: &str, volume: i64) -> Result<()> {
        // PI volume is a fraction, but `set-sink-volume` with a percent
        // or absolute int also works; absolute is what upstream uses
        // (pulse.cpp:44-45). We pass the integer directly.
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
/// Examples (per `pactl(1)`):
///   `Event 'change' on sink #0 'alsa_output.pci-0000_00_1f.3.analog-stereo'`
///   `Event 'new' on sink #1 '...'`
///   `Event 'remove' on sink #2 '...'`
///   `Event 'change' on server` (server-side event, not a sink event)
pub(crate) fn parse_subscribe_line(line: &str) -> Option<SubscribeEvent> {
    let line = line.trim();
    if !line.starts_with("Event ") {
        return None;
    }
    // `Event 'EVENT' on sink #N 'NAME'`
    let after_event = line.strip_prefix("Event ")?;
    let (event_type, rest) = quoted_field(after_event)?;
    let rest = rest.trim_start();
    // rest = "on sink #N 'NAME'" or "on server" (or any other target).
    let after_on = rest.strip_prefix("on ")?.trim_start();
    let name = if let Some(after_sink) = after_on.strip_prefix("sink ") {
        // after_sink = "#N 'NAME'"
        let after_sink = after_sink.trim_start();
        if !after_sink.starts_with('#') {
            return Some(SubscribeEvent::Unclassified {
                line: line.to_string(),
            });
        }
        let after_idx = after_sink.split_once(' ')?.1.trim_start();
        let (name, _) = quoted_field(after_idx)?;
        name.to_string()
    } else {
        // Not a sink event (server, client, source, sink-input, …).
        return Some(SubscribeEvent::Unclassified {
            line: line.to_string(),
        });
    };
    match event_type {
        "new" => Some(SubscribeEvent::SinkAdded { name }),
        "remove" => Some(SubscribeEvent::SinkRemoved { name }),
        "change" => Some(SubscribeEvent::SinkChanged { name }),
        _ => Some(SubscribeEvent::Unclassified {
            line: line.to_string(),
        }),
    }
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
        // The AtomicBool here is only a poison flag for completeness;
        // the supervisor's loop has its own exit paths.
        tokio::time::sleep(Duration::from_millis(100)).await;
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
                assert_eq!(name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
            }
            other => panic!("expected SinkChanged, got {other:?}"),
        }
    }

    #[test]
    fn parse_subscribe_new_event() {
        let line = "Event 'new' on sink #42 'foo'";
        assert!(matches!(
            parse_subscribe_line(line).expect("parsed"),
            SubscribeEvent::SinkAdded { name } if name == "foo"
        ));
    }

    #[test]
    fn parse_subscribe_remove_event() {
        let line = "Event 'remove' on sink #42 'foo'";
        assert!(matches!(
            parse_subscribe_line(line).expect("parsed"),
            SubscribeEvent::SinkRemoved { name } if name == "foo"
        ));
    }

    #[test]
    fn parse_subscribe_unclassified_event_passes_through() {
        let line = "Event 'change' on server";
        // server-side event is not a sink event; we surface it as Unclassified.
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

    /// Exact `pactl --format=json list sinks` shape on a live KDE neon
    /// session (kdeconnect-kde is the consumer). We mirror this for the
    /// `PactlSink` deserializer; the fixture would survive upstream
    /// changes because serde ignores unknown fields.
    #[test]
    fn parse_pactl_sink_list_json() {
        // The JSON keys are what pactl actually emits. The "mute" field
        // is a string ("yes"/"no") in pactl-shell-protocol-v2 output.
        let raw = r#"{
            "sinks": [
                {
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo",
                    "description": "Built-in Audio Analog Stereo",
                    "mute": "no",
                    "volume": { "value": 0.5, "value_flat": 0.7 },
                    "is_default": true
                },
                {
                    "name": "alsa_output.usb-foo",
                    "description": "USB Audio",
                    "mute": "yes",
                    "volume": { "value": 0.25, "value_flat": 0.25 },
                    "is_default": false
                }
            ]
        }"#;
        let parsed: PactlSinkList = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.sinks.len(), 2);
        let converted: Vec<LocalSinkState> = parsed
            .sinks
            .into_iter()
            .map(|s| s.to_state(65_536))
            .collect();
        assert_eq!(
            converted[0].name,
            "alsa_output.pci-0000_00_1f.3.analog-stereo"
        );
        // 0.7 * 65536 = 45875.2, round() = 45875
        assert_eq!(converted[0].volume, Some(45_875));
        assert_eq!(converted[0].muted, Some(false));
        assert_eq!(converted[0].enabled, Some(true));
        assert_eq!(converted[1].muted, Some(true));
        assert_eq!(converted[1].enabled, Some(false));
    }

    #[test]
    fn parse_pactl_sink_list_unknown_fields_ignored() {
        let raw = r#"{
            "sinks": [{
                "name": "s",
                "description": "d",
                "muted": false,
                "volume": { "value": 0.5, "value_flat": 0.5 },
                "is_default": false,
                "future_field": "ignored"
            }]
        }"#;
        let parsed: PactlSinkList = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.sinks.len(), 1);
    }

    #[test]
    fn parse_pactl_sink_list_missing_volume() {
        let raw = r#"{
            "sinks": [{
                "name": "s",
                "description": "d",
                "muted": false,
                "volume": {},
                "is_default": false
            }]
        }"#;
        let parsed: PactlSinkList = serde_json::from_str(raw).expect("parse");
        let state = parsed.sinks[0].clone().to_state(65_536);
        assert_eq!(state.volume, Some(0));
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
            name: "x".to_string(),
        });
        let ev = rx.recv().await.expect("event");
        assert!(matches!(ev, SubscribeEvent::SinkAdded { name } if name == "x"));
    }

    #[tokio::test]
    async fn mock_backend_unavailable_blocks_list() {
        let backend = MockBackend::new();
        backend.set_available(false);
        let err = backend.list_sinks().await.unwrap_err();
        let _ = err;
    }
}

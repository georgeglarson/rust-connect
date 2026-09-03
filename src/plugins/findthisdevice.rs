//! Find This Device plugin
//!
//! Single Responsibility: Ring THIS machine when the paired phone asks —
//! the desktop-side counterpart of the findmyphone plugin (phone rings the
//! desktop instead of the desktop ringing the phone). The two plugins share
//! one packet type in opposite roles, exactly like upstream
//! (kdeconnect-kde plugins/findthisdevice/kdeconnect_findthisdevice.json:
//! SupportedPacketType = ["kdeconnect.findmyphone.request"],
//! OutgoingPacketType = []).
//!
//! Wire shape: the request has an EMPTY body — the desktop
//! findmyphoneplugin.cpp:17-21 sends `{}` and findthisdeviceplugin.cpp:25
//! ignores the contents entirely, so we do too.
//!
//! Action (findthisdeviceplugin.cpp:25-52): play a ringtone at full player
//! volume, temporarily UNMUTING muted PulseAudio sinks and re-muting them
//! when playback ends. Differences from upstream, documented:
//! - Fixed bundled alarm (assets/findthisdevice-alarm.wav, generated
//!   in-repo, embedded via include_bytes) instead of the configurable
//!   `ringtone` setting — no config surface, same default behavior.
//! - Mute handling covers the DEFAULT sink only (`pactl get/set-sink-mute
//!   @DEFAULT_SINK@`), not every sink — restoring per-sink state needs
//!   PulseAudioQt; pactl is the only PA surface this codebase uses.
//! - Single-flight: upstream spawns one QMediaPlayer per packet, so a
//!   packet flood stacks overlapping alarms. A paired device is trusted but
//!   not infallible; a second request while an alarm is playing is logged
//!   and dropped instead of amplifying.
//!
//! The player is the first available of pw-play / paplay / ffplay / aplay;
//! with none of them (or no pactl/PA at all) the plugin degrades to a log
//! event, mousepad/clipboard-style.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};

use tracing::{debug, info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// The generated two-tone alarm, embedded so the daemon never depends on an
/// asset install path. Written to a temp file at play time because every
/// player we shell out to takes a path.
const ALARM_WAV: &[u8] = include_bytes!("../../assets/findthisdevice-alarm.wav");

/// The ring-action seam. The real impl shells out to pactl + a player;
/// tests drive a mock. Mirrors the clipboard/mpris/pausemusic backend
/// split: detection is lazy and everything degrades to a log event.
#[async_trait::async_trait]
pub(crate) trait RingBackend: Send + Sync {
    /// Play the alarm once, handling unmute/restore. Returns false when no
    /// player is available (degraded).
    async fn ring(&self) -> bool;
}

pub struct FindThisDevicePlugin {
    backend: StdRwLock<Option<Arc<dyn RingBackend>>>,
    /// Single-flight guard: one alarm at a time (see module docs).
    ringing: Arc<AtomicBool>,
}

impl Default for FindThisDevicePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl FindThisDevicePlugin {
    pub fn new() -> Self {
        Self {
            backend: StdRwLock::new(Some(Arc::new(ProcessRingBackend::new()))),
            ringing: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_backend(self, backend: Arc<dyn RingBackend>) -> Self {
        if let Ok(mut guard) = self.backend.write() {
            *guard = Some(backend);
        }
        self
    }

    fn backend(&self) -> Option<Arc<dyn RingBackend>> {
        self.backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Test-visible single-flight state.
    #[cfg(test)]
    pub(crate) fn is_ringing(&self) -> bool {
        self.ringing.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Plugin for FindThisDevicePlugin {
    fn name(&self) -> &str {
        "findthisdevice"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        // kdeconnect-kde kdeconnect_findthisdevice.json
        // X-KdeConnect-SupportedPacketType.
        vec!["kdeconnect.findmyphone.request".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // kdeconnect_findthisdevice.json X-KdeConnect-OutgoingPacketType: [].
        vec![]
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        // Body is ignored upstream (findthisdeviceplugin.cpp:25 takes the
        // packet as unused); we only log that a request arrived.
        let _ = packet;

        let Some(backend) = self.backend() else {
            warn!(
                device_id = %device_id,
                event = "findthisdevice_no_backend",
                "Find-this-device requested but no ring backend available"
            );
            return Ok(None);
        };

        // Single-flight: compare-and-swap so concurrent packets can't both
        // pass the gate.
        if self.ringing.swap(true, Ordering::SeqCst) {
            debug!(
                device_id = %device_id,
                event = "findthisdevice_already_ringing",
                "Alarm already playing, dropping duplicate request"
            );
            return Ok(None);
        }

        info!(
            device_id = %device_id,
            event = "findthisdevice_ring",
            "Find-this-device requested, playing alarm"
        );

        let ringing = self.ringing.clone();
        tokio::spawn(async move {
            struct RingGuard(Arc<AtomicBool>);
            impl Drop for RingGuard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _guard = RingGuard(ringing);

            let played = backend.ring().await;
            if !played {
                warn!(
                    event = "findthisdevice_ring_failed",
                    "No usable audio player found (tried pw-play, paplay, ffplay, aplay)"
                );
            }
        });

        Ok(None)
    }
}

/// Real backend: best-effort default-sink unmute, play the bundled alarm
/// with the first available player, restore the mute afterwards.
struct ProcessRingBackend {
    // `Arc`-wrapped (not a bare `std::sync::Mutex` field) so `ring()` can
    // clone a 'static handle and hand it to `spawn_blocking` — the
    // temp-file creation + write is blocking I/O and must not run
    // directly on the async task (see `alarm_path_blocking`'s doc).
    alarm_file: Arc<std::sync::Mutex<Option<Arc<tempfile::NamedTempFile>>>>,
}

impl ProcessRingBackend {
    fn new() -> Self {
        Self {
            alarm_file: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Free function (not `&self`) so it can be moved into
    /// `tokio::task::spawn_blocking` with only an owned `Arc` clone of
    /// the cache, not a borrow of `self` — `spawn_blocking`'s closure
    /// must be `'static`. Creates and writes the alarm temp file on
    /// first use (blocking `tempfile`/`std::fs::write`), then caches the
    /// handle for every ring after.
    fn alarm_path_blocking(
        alarm_file: &std::sync::Mutex<Option<Arc<tempfile::NamedTempFile>>>,
    ) -> Option<PathBuf> {
        let mut guard = alarm_file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref tf) = *guard {
            return Some(tf.path().to_path_buf());
        }
        match tempfile::Builder::new()
            .prefix("rust-connect-alarm-")
            .suffix(".wav")
            .tempfile()
        {
            Ok(tf) => {
                let path = tf.path().to_path_buf();
                if let Err(e) = std::fs::write(&path, ALARM_WAV) {
                    warn!(error = %e, event = "findthisdevice_temp_write_failed", "Cannot write alarm temp file");
                    return None;
                }
                *guard = Some(Arc::new(tf));
                Some(path)
            }
            Err(e) => {
                warn!(error = %e, event = "findthisdevice_temp_create_failed", "Cannot create alarm temp file");
                None
            }
        }
    }

    /// The player priority table (Task 1.6 Backend D "verify + pin", vk
    /// #1010): pw-play and paplay take the file bare; ffplay/aplay need
    /// flags to stay non-interactive and quiet. Order is pw-play > paplay >
    /// ffplay > aplay — PipeWire/PulseAudio native tools first, then the
    /// generic media player, then the bare-ALSA fallback.
    const PLAYER_CANDIDATES: &'static [(&'static str, &'static [&'static str])] = &[
        ("pw-play", &[]),
        ("paplay", &[]),
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("aplay", &["-q"]),
    ];

    /// First available candidate, in `PLAYER_CANDIDATES` priority order.
    /// Pure — takes each candidate's PATH-availability as input — so the
    /// priority order is unit-testable without depending on which players
    /// happen to be installed on the test host, and without touching the
    /// real PATH at all.
    fn choose_player(available: [bool; 4]) -> Option<(&'static str, &'static [&'static str])> {
        Self::PLAYER_CANDIDATES
            .iter()
            .zip(available)
            .find(|(_, avail)| *avail)
            .map(|((bin, args), _)| (*bin, *args))
    }

    /// (binary, args prefix) for the first player found on PATH.
    fn player() -> Option<(&'static str, Vec<&'static str>)> {
        let available = [
            which_exists("pw-play"),
            which_exists("paplay"),
            which_exists("ffplay"),
            which_exists("aplay"),
        ];
        Self::choose_player(available).map(|(bin, args)| (bin, args.to_vec()))
    }

    /// Some(was_muted) when pactl is usable — the state to restore after.
    async fn unmute_default_sink() -> Option<bool> {
        let out = tokio::process::Command::new("pactl")
            .args(["get-sink-mute", "@DEFAULT_SINK@"])
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let was_muted = String::from_utf8_lossy(&out.stdout).contains("yes");
        if was_muted {
            let _ = tokio::process::Command::new("pactl")
                .args(["set-sink-mute", "@DEFAULT_SINK@", "0"])
                .output()
                .await;
        }
        Some(was_muted)
    }

    async fn restore_mute(was_muted: bool) {
        if was_muted {
            // findthisdeviceplugin.cpp:45-50 re-mutes what it unmuted.
            let _ = tokio::process::Command::new("pactl")
                .args(["set-sink-mute", "@DEFAULT_SINK@", "1"])
                .output()
                .await;
        }
    }
}

fn which_exists(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl RingBackend for ProcessRingBackend {
    async fn ring(&self) -> bool {
        // Both lookups are blocking (`which_exists` shells out to `which`
        // per candidate; the alarm path may create + write a temp file):
        // run them on the blocking thread pool instead of inline on this
        // async task. `ring()` already runs inside a `tokio::spawn`ed
        // task (see `handle_packet`), so this doesn't block a packet
        // handler directly, but it still parks a runtime worker thread
        // for the duration of every `which` invocation and the first
        // temp-file write.
        let Some((player, args)) = tokio::task::spawn_blocking(Self::player)
            .await
            .unwrap_or(None)
        else {
            return false;
        };
        let alarm_file = self.alarm_file.clone();
        let Some(alarm) =
            tokio::task::spawn_blocking(move || Self::alarm_path_blocking(&alarm_file))
                .await
                .unwrap_or(None)
        else {
            return false;
        };

        // findthisdeviceplugin.cpp:35-44 unmutes muted sinks for the alarm.
        let was_muted = Self::unmute_default_sink().await;

        let status = tokio::process::Command::new(player)
            .args(&args)
            .arg(&alarm)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        if let Some(was_muted) = was_muted {
            Self::restore_mute(was_muted).await;
        }

        match status {
            Ok(s) if s.success() => true,
            Ok(s) => {
                warn!(player = player, code = ?s.code(), event = "findthisdevice_player_failed", "Audio player exited non-zero");
                false
            }
            Err(e) => {
                warn!(player = player, error = %e, event = "findthisdevice_player_spawn_failed", "Cannot spawn audio player");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Returns immediately; `completed` counts finished rings.
    struct FastMock {
        completed: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RingBackend for FastMock {
        async fn ring(&self) -> bool {
            self.completed.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    /// Blocks until released; `started` counts entered rings.
    struct GatedMock {
        started: AtomicUsize,
        gate: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl RingBackend for GatedMock {
        async fn ring(&self) -> bool {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.gate.notified().await;
            true
        }
    }

    fn request_packet() -> Packet {
        // Upstream wire literal — empty body (the desktop-side mirror of
        // findmyphoneplugin.cpp:17-21; findthisdeviceplugin.cpp:25 takes the
        // packet as unused). The fixture that pins this is
        // tests/fixtures/upstream-wire/findthisdevice/ring_request.json.
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/findthisdevice/ring_request.json"),
            )
            .expect("findthisdevice/ring_request.json"),
        )
        .expect("findthisdevice/ring_request.json parses");
        Packet::new("kdeconnect.findmyphone.request".to_string(), body)
    }

    async fn wait_until<F: FnMut() -> bool>(mut cond: F) -> bool {
        for _ in 0..80 {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    }

    #[tokio::test]
    async fn test_findthisdevice_name_and_capabilities() {
        let plugin = FindThisDevicePlugin::new();
        assert_eq!(plugin.name(), "findthisdevice");
        assert_eq!(
            plugin.incoming_capabilities(),
            vec!["kdeconnect.findmyphone.request".to_string()]
        );
        assert!(plugin.outgoing_capabilities().is_empty());
    }

    #[tokio::test]
    async fn test_request_rings() {
        let backend = Arc::new(FastMock {
            completed: AtomicUsize::new(0),
        });
        let plugin = FindThisDevicePlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet("device1", request_packet())
            .await
            .unwrap();
        assert!(wait_until(|| backend.completed.load(Ordering::SeqCst) == 1).await);
    }

    #[tokio::test]
    async fn test_single_flight_drops_duplicate_while_ringing() {
        let backend = Arc::new(GatedMock {
            started: AtomicUsize::new(0),
            gate: tokio::sync::Notify::new(),
        });
        let plugin = FindThisDevicePlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet("device1", request_packet())
            .await
            .unwrap();
        assert!(wait_until(|| backend.started.load(Ordering::SeqCst) == 1).await);
        assert!(plugin.is_ringing());

        // Second request while the first alarm is still playing: dropped.
        plugin
            .handle_packet("device1", request_packet())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(backend.started.load(Ordering::SeqCst), 1);

        // Release the first alarm; the NEXT request rings again.
        backend.gate.notify_one();
        assert!(wait_until(|| !plugin.is_ringing()).await);
        plugin
            .handle_packet("device1", request_packet())
            .await
            .unwrap();
        assert!(wait_until(|| backend.started.load(Ordering::SeqCst) == 2).await);
    }

    #[tokio::test]
    async fn test_rings_again_after_completion() {
        let backend = Arc::new(FastMock {
            completed: AtomicUsize::new(0),
        });
        let plugin = FindThisDevicePlugin::new().with_backend(backend.clone());
        plugin
            .handle_packet("device1", request_packet())
            .await
            .unwrap();
        assert!(wait_until(|| backend.completed.load(Ordering::SeqCst) == 1).await);
        plugin
            .handle_packet("device1", request_packet())
            .await
            .unwrap();
        assert!(wait_until(|| backend.completed.load(Ordering::SeqCst) == 2).await);
    }

    #[tokio::test]
    async fn test_body_contents_ignored() {
        // Upstream ignores the body entirely; a junk body must still ring.
        let backend = Arc::new(FastMock {
            completed: AtomicUsize::new(0),
        });
        let plugin = FindThisDevicePlugin::new().with_backend(backend.clone());
        let packet = Packet::new(
            "kdeconnect.findmyphone.request".to_string(),
            serde_json::json!({ "unexpected": true, "volume": 0 }),
        );
        plugin.handle_packet("device1", packet).await.unwrap();
        assert!(wait_until(|| backend.completed.load(Ordering::SeqCst) == 1).await);
    }

    // -----------------------------------------------------------------
    // Player selection order (Task 1.6 Backend D "verify + pin", vk
    // #1010): pure, no PATH dependency.
    // -----------------------------------------------------------------

    #[test]
    fn test_choose_player_first_available_wins() {
        // Exact priority order pinned against the module doc: pw-play >
        // paplay > ffplay > aplay.
        assert_eq!(
            ProcessRingBackend::choose_player([true, true, true, true]),
            Some(("pw-play", &[][..]))
        );
        assert_eq!(
            ProcessRingBackend::choose_player([false, true, true, true]),
            Some(("paplay", &[][..]))
        );
        assert_eq!(
            ProcessRingBackend::choose_player([false, false, true, true]),
            Some((
                "ffplay",
                &["-nodisp", "-autoexit", "-loglevel", "quiet"][..]
            ))
        );
        assert_eq!(
            ProcessRingBackend::choose_player([false, false, false, true]),
            Some(("aplay", &["-q"][..]))
        );
    }

    #[test]
    fn test_choose_player_none_available_is_none() {
        // The no-player-available degraded path: `player()` returns None,
        // `ring()` returns false without spawning anything, and
        // handle_packet's caller treats that as a logged, non-fatal
        // failure (see test_no_player_and_crashed_player_release_the_latch
        // below — from handle_packet's perspective the two cases are the
        // same event).
        assert_eq!(
            ProcessRingBackend::choose_player([false, false, false, false]),
            None
        );
    }

    #[test]
    fn test_choose_player_only_middle_candidate_available() {
        // Guards against an off-by-one in the zip/find: aplay-only and
        // ffplay-only are covered above; this pins paplay being picked
        // when it is the ONLY one present, not merely the first.
        assert_eq!(
            ProcessRingBackend::choose_player([false, true, false, false]),
            Some(("paplay", &[][..]))
        );
    }

    // -----------------------------------------------------------------
    // Latch release on ring failure (Task 1.6 Backend D): a player that
    // crashes, exits non-zero, or was never found all collapse to the
    // SAME event above ProcessRingBackend — `ring()` returning `false` —
    // so a mock returning `false` exercises the identical code path
    // (RingGuard's unconditional Drop) a real crashed/killed player would.
    // -----------------------------------------------------------------

    /// Returns immediately with `false` — simulates any of "no player on
    /// PATH", "player exited non-zero", "player crashed/was killed
    /// mid-playback", or "spawn failed": ProcessRingBackend::ring()
    /// normalizes all four into `false` without ever panicking (see
    /// ring()'s match arms above), so this mock is a faithful stand-in
    /// for all of them at the layer handle_packet actually depends on.
    struct FailingMock {
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RingBackend for FailingMock {
        async fn ring(&self) -> bool {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            false
        }
    }

    #[tokio::test]
    async fn test_no_player_and_crashed_player_release_the_latch() {
        // The single-flight latch releases via RingGuard's Drop
        // (handle_packet, spawned task) regardless of ring()'s return
        // value. If this DID stick, a real player crash would leave the
        // daemon permanently unable to ring again — read the code: Drop
        // runs unconditionally when the spawned task's async block ends,
        // success or failure, so it does not stick. Pinned here so a
        // future refactor that moves the guard inside a conditional
        // breaks this test loudly.
        let backend = Arc::new(FailingMock {
            attempts: AtomicUsize::new(0),
        });
        let plugin = FindThisDevicePlugin::new().with_backend(backend.clone());

        // First request: handle_packet must still return Ok (never
        // fatal), and the failure must be non-panicking.
        let result = plugin.handle_packet("device1", request_packet()).await;
        assert!(
            result.is_ok(),
            "a failed ring must not error the packet handler"
        );
        assert!(wait_until(|| backend.attempts.load(Ordering::SeqCst) == 1).await);
        assert!(
            wait_until(|| !plugin.is_ringing()).await,
            "latch must release after a failed ring, not just a successful one"
        );

        // Second request after the failure: must actually attempt to
        // ring again, proving the latch never stuck.
        plugin
            .handle_packet("device1", request_packet())
            .await
            .unwrap();
        assert!(
            wait_until(|| backend.attempts.load(Ordering::SeqCst) == 2).await,
            "a request after a failed ring must ring again, not stay latched"
        );
    }

    #[tokio::test]
    async fn test_alarm_wav_embedded_and_valid() {
        // The embedded asset must stay a parseable RIFF/WAVE (a botched
        // include path fails the build, but a corrupt file would only fail
        // at ring time).
        assert!(ALARM_WAV.len() > 44);
        assert_eq!(&ALARM_WAV[0..4], b"RIFF");
        assert_eq!(&ALARM_WAV[8..12], b"WAVE");
    }
}

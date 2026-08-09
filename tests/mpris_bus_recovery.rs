//! Bus-recovery integration test for the MPRIS zbus backend (vk #1024):
//! proves that after a session-bus restart, the SAME `ZbusMprisBackend`
//! instance both re-discovers players AND keeps working control methods
//! (`set_volume`), with no reconstruction.
//!
//! This spawns a private `dbus-daemon` and sets `DBUS_SESSION_BUS_ADDRESS`
//! for the WHOLE PROCESS via `std::env::set_var` before any zbus connection
//! exists (edition 2021, so `set_var` is safe). That is why this file is
//! its own test process and MUST stay the only `#[tokio::test]` here: a
//! second test in this file would either fight over the process-wide env
//! var or run concurrently against a bus this test has already killed.
//!
//! Skips cleanly (passes) when `dbus-daemon` is not on PATH.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rust_connect::plugins::mpris::zbus_backend::ZbusMprisBackend;
use rust_connect::plugins::mpris::{MprisBackend, MprisBackendEvent};
use zbus::zvariant::OwnedValue;

const FAKE_NAME: &str = "org.mpris.MediaPlayer2.rustconnectrecoveryprobe";
const FAKE_PATH: &str = "/org/mpris/MediaPlayer2";
const FAKE_IDENTITY: &str = "RustConnectRecoveryProbe";

struct FakeRoot;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl FakeRoot {
    #[zbus(property)]
    async fn identity(&self) -> &str {
        FAKE_IDENTITY
    }
}

#[derive(Default)]
struct FakePlayer {
    volume: std::sync::Mutex<f64>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    #[zbus(property)]
    async fn playback_status(&self) -> &str {
        "Playing"
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    /// MICROSECONDS per the MPRIS spec.
    #[zbus(property)]
    async fn position(&self) -> i64 {
        0
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// zbus's object server emits PropertiesChanged automatically after a
    /// setter, exactly like the real player_listener depends on.
    #[zbus(property)]
    async fn set_volume(&self, volume: f64) {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner()) = volume;
    }

    #[zbus(property)]
    async fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    async fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    async fn can_go_next(&self) -> bool {
        true
    }
    #[zbus(property)]
    async fn can_go_previous(&self) -> bool {
        true
    }
    #[zbus(property)]
    async fn can_seek(&self) -> bool {
        true
    }
}

/// Kills the dbus-daemon child on drop, even if the test panics partway
/// through — a leaked private dbus-daemon is exactly the kind of test
/// pollution the brief calls out to guard against.
struct DaemonGuard(Option<Child>);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_daemon(socket_path: &std::path::Path) -> Child {
    Command::new("dbus-daemon")
        .arg("--session")
        .arg(format!("--address=unix:path={}", socket_path.display()))
        .arg("--print-address")
        .arg("--nofork")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dbus-daemon failed")
}

/// Bounded poll for the daemon's socket file to appear.
async fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("dbus-daemon socket never appeared at {}", path.display());
}

async fn serve_fake_player() -> zbus::Connection {
    let server = zbus::Connection::session()
        .await
        .expect("fake-player server connect failed");
    server
        .object_server()
        .at(FAKE_PATH, FakeRoot)
        .await
        .expect("serve root failed");
    server
        .object_server()
        .at(FAKE_PATH, FakePlayer::default())
        .await
        .expect("serve player failed");
    server
        .request_name(FAKE_NAME)
        .await
        .expect("request_name failed");
    server
}

/// Read the fake player's Volume property through an independent client
/// connection — proof the SET actually reached the fake, not just that the
/// backend call returned `Ok`.
async fn read_fake_volume() -> f64 {
    let client = zbus::Connection::session()
        .await
        .expect("client connect failed");
    let props = zbus::fdo::PropertiesProxy::builder(&client)
        .destination(FAKE_NAME)
        .expect("destination")
        .path(FAKE_PATH)
        .expect("path")
        .build()
        .await
        .expect("properties proxy");
    let value = props
        .get(
            "org.mpris.MediaPlayer2.Player".try_into().expect("iface"),
            "Volume",
        )
        .await
        .expect("get Volume failed");
    f64::try_from(value).expect("Volume was not f64")
}

async fn recv_until<F>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<MprisBackendEvent>,
    timeout: Duration,
    mut pred: F,
) -> Option<MprisBackendEvent>
where
    F: FnMut(&MprisBackendEvent) -> bool,
{
    let deadline = Instant::now() + timeout;
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline.into(), rx.recv()).await {
        if pred(&event) {
            return Some(event);
        }
    }
    None
}

#[tokio::test]
async fn backend_survives_session_bus_restart() {
    if Command::new("dbus-daemon")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("dbus-daemon not on PATH — skipping");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("bus");

    // SAFETY: process-wide env mutation, done once, before any zbus
    // connection exists anywhere in this process. This file is
    // deliberately the only #[tokio::test] here (see module doc) so
    // nothing else in the process races this var.
    std::env::set_var(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", socket_path.display()),
    );

    let mut daemon = DaemonGuard(Some(spawn_daemon(&socket_path)));
    wait_for_socket(&socket_path).await;

    let fake_server = serve_fake_player().await;

    let backend = ZbusMprisBackend::connect()
        .await
        .expect("backend connect failed");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MprisBackendEvent>();
    backend.start_watching(tx).expect("start_watching failed");

    // 1. Initial discovery.
    recv_until(
        &mut rx,
        Duration::from_secs(5),
        |e| matches!(e, MprisBackendEvent::PlayerAdded(state) if state.service == FAKE_NAME),
    )
    .await
    .expect("fake player was not discovered before the bus drop");

    // 2. A control method works pre-drop, observed on the fake itself.
    backend
        .set_volume(FAKE_IDENTITY, 42)
        .await
        .expect("set_volume failed before bus drop");
    let observed = read_fake_volume().await;
    assert!(
        (observed - 0.42).abs() < 1e-9,
        "fake did not observe pre-drop set_volume: {observed}"
    );

    // 3. Kill the bus out from under the backend.
    if let Some(mut child) = daemon.0.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(fake_server);
    let _ = std::fs::remove_file(&socket_path);

    recv_until(&mut rx, Duration::from_secs(10), |e| {
        matches!(e, MprisBackendEvent::BackendLost)
    })
    .await
    .expect("BackendLost was not observed after the bus died");

    // 4. Restart dbus-daemon at the SAME socket path and re-register the
    // fake player — its old connection died with the bus.
    daemon.0 = Some(spawn_daemon(&socket_path));
    wait_for_socket(&socket_path).await;
    let fake_server_2 = serve_fake_player().await;

    // 5. Re-discovery AND a working control method, same backend instance,
    // no reconstruction. Pre-fix: control methods stay bound to the dead
    // initial connection forever, so this set_volume call fails/hangs
    // waiting on a dead conn even after re-discovery succeeds.
    recv_until(
        &mut rx,
        Duration::from_secs(15),
        |e| matches!(e, MprisBackendEvent::PlayerAdded(state) if state.service == FAKE_NAME),
    )
    .await
    .expect("fake player was not re-discovered after bus restart");

    backend.set_volume(FAKE_IDENTITY, 77).await.expect(
        "set_volume failed after bus restart — control methods did not roll to the new connection",
    );
    let observed = read_fake_volume().await;
    assert!(
        (observed - 0.77).abs() < 1e-9,
        "fake did not observe post-recovery set_volume: {observed}"
    );

    drop(fake_server_2);
}

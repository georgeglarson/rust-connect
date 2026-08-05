//! Live session-bus integration test for the MPRIS zbus backend: serves a
//! fake org.mpris.MediaPlayer2 player on the REAL session bus via zbus's
//! object server, then verifies the backend discovers it, reads its state,
//! and translates a real PropertiesChanged signal into a backend event.
//!
//! Ignored by default: it needs a session D-Bus. Run on a desktop host with:
//!
//! ```sh
//! cargo test --test mpris_session_bus -- --ignored
//! ```
//!
//! It skips cleanly (passes) when no session bus address is present, and the
//! fake player's well-known name vanishes with the test process.

use std::collections::HashMap;
use std::time::Duration;

use rust_connect::plugins::mpris::zbus_backend::ZbusMprisBackend;
use rust_connect::plugins::mpris::{MprisBackend, MprisBackendEvent};
use zbus::zvariant::OwnedValue;

const FAKE_NAME: &str = "org.mpris.MediaPlayer2.rustconnectprobe";
const FAKE_PATH: &str = "/org/mpris/MediaPlayer2";

struct FakeRoot;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl FakeRoot {
    #[zbus(property)]
    async fn identity(&self) -> &str {
        "RustConnectProbe"
    }
}

#[derive(Default)]
struct FakePlayer {
    volume: std::sync::Mutex<f64>,
    can_seek: std::sync::Mutex<bool>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    async fn play_pause(&self) {}

    #[zbus(property)]
    async fn playback_status(&self) -> &str {
        "Playing"
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        HashMap::from([
            (
                "xesam:title".to_string(),
                OwnedValue::from(zbus::zvariant::Str::from("Probe Song")),
            ),
            ("mpris:length".to_string(), OwnedValue::from(123_000_000i64)),
        ])
    }

    /// MICROSECONDS per the MPRIS spec.
    #[zbus(property)]
    async fn position(&self) -> i64 {
        5_000_000
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// zbus's object server emits PropertiesChanged automatically after a
    /// setter — that signal is what the backend under test must catch.
    #[zbus(property)]
    async fn set_volume(&self, volume: f64) {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner()) = volume;
    }

    #[zbus(property)]
    async fn can_seek(&self) -> bool {
        *self.can_seek.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Settable so the test can drive a real CanSeek PropertiesChanged —
    /// the backend must forward capability changes, not just value changes.
    #[zbus(property)]
    async fn set_can_seek(&self, can_seek: bool) {
        *self.can_seek.lock().unwrap_or_else(|e| e.into_inner()) = can_seek;
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
        false
    }
}

async fn recv_until<F>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<MprisBackendEvent>,
    timeout: Duration,
    mut pred: F,
) -> Option<MprisBackendEvent>
where
    F: FnMut(&MprisBackendEvent) -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline.into(), rx.recv()).await {
        if pred(&event) {
            return Some(event);
        }
    }
    None
}

#[tokio::test]
#[ignore = "needs a live session D-Bus"]
async fn fake_player_on_real_bus_is_discovered_and_tracked() {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        eprintln!("no DBUS_SESSION_BUS_ADDRESS — skipping");
        return;
    }

    // Serve the fake player on the real session bus.
    let server = zbus::Connection::session()
        .await
        .expect("session bus connect failed");
    server
        .object_server()
        .at(FAKE_PATH, FakeRoot)
        .await
        .expect("serve root failed");
    server
        .object_server()
        .at(
            FAKE_PATH,
            FakePlayer {
                volume: std::sync::Mutex::new(0.0),
                can_seek: std::sync::Mutex::new(true),
            },
        )
        .await
        .expect("serve player failed");
    server
        .request_name(FAKE_NAME)
        .await
        .expect("request_name failed");

    // Backend under test.
    let backend = ZbusMprisBackend::connect()
        .await
        .expect("backend connect failed");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MprisBackendEvent>();
    backend.start_watching(tx).expect("start_watching failed");

    // Discovery: PlayerAdded with the Identity-derived display name.
    let added = recv_until(
        &mut rx,
        Duration::from_secs(5),
        |e| matches!(e, MprisBackendEvent::PlayerAdded(state) if state.service == FAKE_NAME),
    )
    .await
    .expect("fake player was not discovered");
    let MprisBackendEvent::PlayerAdded(state) = added else {
        unreachable!()
    };
    assert_eq!(state.name, "RustConnectProbe");
    assert_eq!(state.title, "Probe Song");
    assert_eq!(state.length_ms, 123_000); // µs→ms
    assert_eq!(state.position_ms, 5_000);
    assert!(state.is_playing);
    assert!(state.can_seek);
    assert!(!state.can_go_previous);

    // Fresh state read (the requestNowPlaying path).
    let state = backend
        .player_state("RustConnectProbe")
        .await
        .expect("player_state failed");
    assert_eq!(state.title, "Probe Song");

    // PropertiesChanged: set Volume to 0.42 through a client proxy; the
    // backend must emit changed.volume == Some(42) (0.0-1.0 → 0-100,
    // truncated — kdeconnect-kde mpriscontrolplugin.cpp:140).
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
    props
        .set(
            "org.mpris.MediaPlayer2.Player".try_into().expect("iface"),
            "Volume",
            zbus::zvariant::Value::from(0.42f64),
        )
        .await
        .expect("set volume failed");

    let event = recv_until(&mut rx, Duration::from_secs(5), |e| {
        matches!(e, MprisBackendEvent::PropertiesChanged { changed, .. } if changed.volume.is_some())
    })
    .await
    .expect("no PropertiesChanged with Volume received");
    let MprisBackendEvent::PropertiesChanged { state, changed } = event else {
        unreachable!()
    };
    assert_eq!(changed.volume, Some(42));
    assert_eq!(state.name, "RustConnectProbe");
    assert_eq!(state.volume, 42);

    // Capability change: flip CanSeek; the backend must forward it (a stale
    // canSeek leaves the phone with wrong scrub/seek affordances).
    props
        .set(
            "org.mpris.MediaPlayer2.Player".try_into().expect("iface"),
            "CanSeek",
            zbus::zvariant::Value::from(false),
        )
        .await
        .expect("set can_seek failed");

    let event = recv_until(&mut rx, Duration::from_secs(5), |e| {
        matches!(e, MprisBackendEvent::PropertiesChanged { changed, .. } if changed.can_seek.is_some())
    })
    .await
    .expect("no PropertiesChanged with CanSeek received");
    let MprisBackendEvent::PropertiesChanged { state, changed } = event else {
        unreachable!()
    };
    assert_eq!(changed.can_seek, Some(false));
    assert!(!state.can_seek);
}

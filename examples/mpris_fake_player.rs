//! Tiny MPRIS fake-player host, modeled on tests/mpris_bus_recovery.rs:23-80.
//!
//! Plants a single `org.mpris.MediaPlayer2.<name>` name on the session bus
//! addressed by $DBUS_SESSION_BUS_ADDRESS (so the rust mpris zbus backend
//! can find it the same way it finds a real player). Stays alive until
//! killed — the harness starts it, observes the rust side's REST output,
//! then kills it on cleanup.
//!
//! Args:
//!   <player_name>   short name; service = org.mpris.MediaPlayer2.<player_name>
//!
//! Run: cargo run --example mpris_fake_player -- <player_name>

use std::collections::HashMap;

use zbus::connection::Builder as ConnectionBuilder;
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue, Str};

const IDENTITY_DEFAULT: &str = "RustConnectFakePlayer";

struct FakeRoot;

#[interface(name = "org.mpris.MediaPlayer2")]
impl FakeRoot {
    #[zbus(property)]
    async fn identity(&self) -> String {
        std::env::var("MPRIS_FAKE_IDENTITY").unwrap_or_else(|_| IDENTITY_DEFAULT.to_string())
    }
    #[zbus(property)]
    async fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property)]
    async fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    async fn supported_uri_schemes(&self) -> Vec<String> {
        vec![]
    }
    #[zbus(property)]
    async fn supported_mime_types(&self) -> Vec<String> {
        vec![]
    }
}

#[derive(Default)]
struct FakePlayer {
    volume: std::sync::Mutex<f64>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    #[zbus(property)]
    async fn playback_status(&self) -> String {
        std::env::var("MPRIS_FAKE_PLAYBACK_STATUS").unwrap_or_else(|_| "Playing".to_string())
    }
    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        // Static metadata so the rust zbus backend has something to read.
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        let title = std::env::var("MPRIS_FAKE_TITLE")
            .unwrap_or_else(|_| "RustConnectFakeTitle".to_string());
        let artist = std::env::var("MPRIS_FAKE_ARTIST")
            .unwrap_or_else(|_| "RustConnectFakeArtist".to_string());
        let album = std::env::var("MPRIS_FAKE_ALBUM")
            .unwrap_or_else(|_| "RustConnectFakeAlbum".to_string());
        m.insert(
            "xesam:title".to_string(),
            OwnedValue::from(Str::from(title)),
        );
        m.insert(
            "xesam:artist".to_string(),
            OwnedValue::from(Str::from(artist)),
        );
        m.insert(
            "xesam:album".to_string(),
            OwnedValue::from(Str::from(album)),
        );
        m.insert(
            "mpris:length".to_string(),
            OwnedValue::from(180_000_000_i64),
        ); // 3 min in us
        m.insert(
            "mpris:trackid".to_string(),
            OwnedValue::from(ObjectPath::try_from("/org/mpris/MediaPlayer2/FakeTrack").unwrap()),
        );
        m
    }
    #[zbus(property)]
    async fn position(&self) -> i64 {
        0
    }
    #[zbus(property)]
    async fn volume(&self) -> f64 {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner())
    }
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
    #[zbus(property)]
    async fn can_control(&self) -> bool {
        true
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let player_short = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "rustconnectfake".to_string());
    let service_name = format!("org.mpris.MediaPlayer2.{player_short}");
    eprintln!(
        "[mpris_fake_player] claiming {service_name} on {}",
        std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_else(|_| "<unset>".to_string())
    );

    let conn = ConnectionBuilder::session()
        .expect("session bus connect builder")
        .name(service_name.as_str())
        .expect("name() takes a well-formed bus name")
        .serve_at("/org/mpris/MediaPlayer2", FakeRoot)
        .expect("serve_at FakeRoot")
        .serve_at("/org/mpris/MediaPlayer2", FakePlayer::default())
        .expect("serve_at FakePlayer")
        .build()
        .await
        .expect("connection build");

    eprintln!("[mpris_fake_player] up; press Ctrl-C or get killed");
    // Hold forever. Harness sends SIGTERM/SIGKILL on cleanup.
    std::future::pending::<()>().await;
    drop(conn);
}

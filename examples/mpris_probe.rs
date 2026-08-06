//! Live MPRIS probe: connects the real zbus backend to the session bus,
//! prints discovered players + now-playing state, then watches backend
//! events for a few seconds. READ-ONLY — never sends transport commands.
//!
//! Run: cargo run --example mpris_probe

use rust_connect::plugins::mpris::zbus_backend::ZbusMprisBackend;
use rust_connect::plugins::mpris::{MprisBackend, MprisBackendEvent};

#[tokio::main]
async fn main() {
    let backend = ZbusMprisBackend::connect()
        .await
        .expect("session bus connect failed");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MprisBackendEvent>();
    backend.start_watching(tx).expect("start_watching failed");

    // Give the watch loop a moment to discover existing players.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut players: Vec<String> = Vec::new();
    while std::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline.into(), rx.recv()).await else {
            break;
        };
        match event {
            MprisBackendEvent::PlayerAdded(state) => {
                println!("PLAYER ADDED: {:?} (service {})", state.name, state.service);
                println!(
                    "  now playing: {:?} / {:?} / {:?} | playing={} pos={}ms len={}ms vol={} canSeek={}",
                    state.title,
                    state.artist,
                    state.album,
                    state.is_playing,
                    state.position_ms,
                    state.length_ms,
                    state.volume,
                    state.can_seek
                );
                players.push(state.name.clone());
            }
            MprisBackendEvent::PlayerRemoved { service } => {
                println!("PLAYER REMOVED: {service}");
            }
            MprisBackendEvent::PropertiesChanged { state, changed } => {
                println!(
                    "PROPS CHANGED: {:?} | volume={:?} metadata={} playing={:?} loop={:?} shuffle={:?}",
                    state.name,
                    changed.volume,
                    changed.metadata,
                    changed.playback_status,
                    changed.loop_status,
                    changed.shuffle
                );
            }
            MprisBackendEvent::Seeked {
                service,
                position_us,
            } => {
                println!("SEEKED: {service} -> {}us", position_us);
            }
            MprisBackendEvent::BackendLost => {
                println!("BACKEND LOST: session bus dropped; recovery in progress");
            }
        }
    }

    // Fresh per-player state read (the requestNowPlaying path).
    for name in &players {
        match backend.player_state(name).await {
            Some(state) => println!(
                "STATE {:?}: playing={} title={:?} vol={}",
                state.name, state.is_playing, state.title, state.volume
            ),
            None => println!("STATE {name:?}: gone"),
        }
    }
    println!("probe done: {} player(s) discovered", players.len());
}

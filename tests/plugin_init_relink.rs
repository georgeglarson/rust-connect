//! Plugin init advertisements must survive a link replacement at pairing time.
//!
//! A phone that redials in the same moment pairing completes used to lose
//! every advertisement: they were sent once, into a connection handle that
//! had just been evicted, and each failure was only a per-packet warning.
//! The phone then sat paired with no features until the next redial.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use rust_connect::device::{Device, DeviceType};

async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let settings = AppSettings::default()
        .with_data_dir(temp_dir.path().to_path_buf())
        .with_api_keys(vec!["test-api-key".to_string()]);
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    state.init_plugins().await;
    (state, temp_dir)
}

/// Mark `device_id` as paired without going through the SAS dance —
/// these tests only exercise the wait/bound semantics around a missing
/// link, not the pairing handshake itself.
async fn mark_paired(state: &AppState, device_id: &str) {
    state
        .pairing_handler
        .paired_handle()
        .write()
        .await
        .insert(device_id.to_string(), Utc::now());
}

/// With no live link, the send must WAIT for one (bounded) instead of
/// firing once and giving up — that instant give-up is the bug.
/// The device MUST be paired; send_plugin_init_packets short-circuits
/// on unpaired devices (M2 fix: avoid the kdeconnectd unpair storm).
#[tokio::test]
async fn test_init_send_waits_for_a_link_instead_of_firing_blind() {
    let (state, _temp) = test_state().await;

    let device_id = "relink-peer-aaaaaaaaaaaaaaaaaaaa".to_string();
    let device = Device::new(
        device_id.clone(),
        "the phone".to_string(),
        DeviceType::Phone,
        8,
    );
    state.registry.add(device).await.unwrap();
    mark_paired(&state, &device_id).await;

    let started = Instant::now();
    state.send_plugin_init_packets(&device_id).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(500),
        "the send must wait and retry for a link, not give up instantly \
         (took {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the wait must stay bounded so callers cannot hang (took {elapsed:?})"
    );
}

/// The bound must hold even when called repeatedly, so a reconnect storm
/// cannot pile up unbounded waiters. Same paired precondition as above.
#[tokio::test]
async fn test_init_send_stays_bounded_across_repeated_calls() {
    let (state, _temp) = test_state().await;

    let device_id = "relink-peer-bbbbbbbbbbbbbbbbbbbb".to_string();
    let device = Device::new(
        device_id.clone(),
        "the phone".to_string(),
        DeviceType::Phone,
        8,
    );
    state.registry.add(device).await.unwrap();
    mark_paired(&state, &device_id).await;

    let started = Instant::now();
    for _ in 0..3 {
        state.send_plugin_init_packets(&device_id).await;
    }
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "three calls must not compound into an unbounded wait"
    );
}

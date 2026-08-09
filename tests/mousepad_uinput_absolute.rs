//! Real-uinput integration test for mousepad absolute pointer positioning
//! (vk #1010, Task 1.6, Backend A): proves an absolute-only packet reaches
//! the kernel as real ABS_X/ABS_Y events on the second, single-purpose
//! uinput device (`AbsoluteInputDevice`), not just that the pure
//! `absolute_position` decision function returns the right value.
//!
//! Skips cleanly (passes) when /dev/uinput is unavailable — uses the
//! plugin's own uinput-availability seam (`on_connected`'s `state` field,
//! already the public signal MousepadPlugin::new()'s degradation path
//! sets) rather than probing `/dev/uinput` a second, independent way.

use std::path::PathBuf;
use std::time::Duration;

use rust_connect::plugins::mousepad::MousepadPlugin;
use rust_connect::plugins::plugin::Plugin;
use rust_connect::protocol::types::Packet;

const ABS_DEVICE_NAME: &str = "rust-connect-mousepad-absolute";

/// Bounded poll for the lazily-created absolute-pointer device to become
/// enumerable under `/dev/input` after the warm-up packet. uinput device
/// creation (`UI_DEV_CREATE`) is synchronous in the kernel, but this stays
/// defensive against any devtmpfs propagation delay rather than assuming
/// zero.
async fn wait_for_abs_device_node() -> PathBuf {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((path, _)) =
            evdev::enumerate().find(|(_, dev)| dev.name() == Some(ABS_DEVICE_NAME))
        {
            return path;
        }
        if std::time::Instant::now() >= deadline {
            panic!("absolute-pointer uinput device never became enumerable as '{ABS_DEVICE_NAME}'");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Bounded, blocking read of whatever is queued for an already-open
/// device. The blocking `fetch_events` call runs on the blocking pool so a
/// stalled read can't hang the test past the timeout.
async fn read_events_bounded(
    mut device: evdev::Device,
    timeout: Duration,
) -> Vec<evdev::InputEvent> {
    let result = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            device
                .fetch_events()
                .map(|it| it.collect::<Vec<_>>())
                .unwrap_or_default()
        }),
    )
    .await;
    match result {
        Ok(Ok(events)) => events,
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn absolute_packet_emits_real_abs_events() {
    let plugin = MousepadPlugin::new();

    // The plugin's own uinput-availability seam: `on_connected`'s `state`
    // field is false whenever the primary InputDevice failed to open
    // /dev/uinput. No live device access, no test.
    let announced = plugin.on_connected("probe");
    let available =
        announced.first().and_then(|p| p.body.get("state")) == Some(&serde_json::Value::Bool(true));
    if !available {
        eprintln!("uinput unavailable — skipping");
        return;
    }

    // The absolute device is created LAZILY on first use (see
    // AbsoluteInputDevice's doc) — a throwaway warm-up packet forces
    // creation so there is a device node to enumerate and open BEFORE the
    // real test packet is sent. Opening the reader first is load-bearing:
    // a fd opened after an event is emitted never sees that event, only
    // ones emitted from then on.
    let warm_up = Packet::new(
        "kdeconnect.mousepad.request".to_string(),
        serde_json::json!({ "x": 1.0, "y": 1.0 }),
    );
    plugin
        .handle_packet("device1", warm_up)
        .await
        .expect("warm-up absolute packet must be handled");

    let node = wait_for_abs_device_node().await;
    let device = evdev::Device::open(&node).expect("open absolute-pointer device node");

    let packet = Packet::new(
        "kdeconnect.mousepad.request".to_string(),
        serde_json::json!({ "x": 1920.0, "y": 12.0 }),
    );
    plugin
        .handle_packet("device1", packet)
        .await
        .expect("absolute packet must be handled");

    let events = read_events_bounded(device, Duration::from_secs(5)).await;

    let abs_x = events
        .iter()
        .find(|e| {
            e.event_type() == evdev::EventType::ABSOLUTE
                && e.code() == evdev::AbsoluteAxisCode::ABS_X.0
        })
        .map(evdev::InputEvent::value);
    let abs_y = events
        .iter()
        .find(|e| {
            e.event_type() == evdev::EventType::ABSOLUTE
                && e.code() == evdev::AbsoluteAxisCode::ABS_Y.0
        })
        .map(evdev::InputEvent::value);

    assert_eq!(
        abs_x,
        Some(1920),
        "ABS_X must carry the scaled x coordinate"
    );
    assert_eq!(abs_y, Some(12), "ABS_Y must carry the scaled y coordinate");
    assert!(
        events
            .iter()
            .any(|e| e.event_type() == evdev::EventType::SYNCHRONIZATION),
        "the batch must end with SYN_REPORT"
    );
}

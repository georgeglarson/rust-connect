//! Presenter plugin
//!
//! Single Responsibility: Handle kdeconnect.presenter packets from the phone
//! (laser-pointer style relative movement) by injecting real pointer motion
//! via Linux uinput, following the mousepad plugin's degradation pattern.
//!
//! Wire shape (upstream-verified against the Android app, the oracle):
//! - Pointer: `{"dx": <float>, "dy": <float>}` — fractions of the screen
//!   dimension. kdeconnect-android PresenterPlugin.kt:77-82
//!   (`np["dx"] = xDelta.toDouble(); np["dy"] = yDelta.toDouble()`).
//! - Stop: `{"stop": true}` — kdeconnect-android PresenterPlugin.kt:84-88.
//!
//! There are NO next/previous/fullscreen fields in kdeconnect.presenter
//! packets (the pre-cut implementation invented them as booleans). The
//! Android presenter sends slide navigation as `kdeconnect.mousepad.request`
//! packets with an integer `specialKey` (PAGE_DOWN=next, PAGE_UP=previous,
//! F5=fullscreen, ESC=end) — kdeconnect-android PresenterPlugin.kt:53-74 —
//! which the mousepad plugin handles.
//!
//! What desktops do with each action:
//! - dx/dy: GSConnect moves the real pointer by (dx*1000, dy*1000)
//!   (gsconnect src/service/plugins/presenter.js:40-44); kdeconnect-kde
//!   accumulates them into a 0..1 overlay position
//!   (kdeconnect-kde plugins/presenter/presenterplugin.cpp:92-93). We move
//!   the real pointer like GSConnect.
//! - stop: kdeconnect-kde destroys its overlay (presenterplugin.cpp:69-73);
//!   GSConnect ignores it (presenter.js:45-48). We have no overlay, so stop
//!   only resets movement state.
//!
//! Capability honesty: incoming-only. kdeconnect-kde declares
//! SupportedPacketType=["kdeconnect.presenter"], OutgoingPacketType=[]
//! (kdeconnect_presenter.json); GSConnect likewise (presenter.js:15-16).
//! We never send presenter packets, so we must not advertise it outgoing.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use evdev::AttributeSet;
use evdev::{EventType, InputEvent, KeyCode, RelativeAxisCode};
use tracing::{debug, info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// Pixels of pointer travel per full-screen fraction of dx/dy.
/// GSConnect uses dx*1000 (gsconnect src/service/plugins/presenter.js:42-43).
const POINTER_SCALE: f64 = 1000.0;

/// Body of a kdeconnect.presenter packet.
/// Field names/types per kdeconnect-android PresenterPlugin.kt:77-88.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PresenterRequest {
    #[serde(default)]
    pub dx: Option<f64>,
    #[serde(default)]
    pub dy: Option<f64>,
    #[serde(default)]
    pub stop: bool,
}

/// Converts a fractional delta into whole pixels, carrying the sub-pixel
/// remainder across calls so small movements accumulate instead of
/// rounding away to zero.
fn scaled_pixels(delta: f64, remainder: &mut f64) -> i32 {
    // B2 (2026-09-02 audit, corrected 09-03): the remainder is shared by
    // every device on the single uinput handle, so an infinite `total`
    // would turn every later movement from any device into a
    // max-magnitude jump until a `stop`. serde_json never yields a
    // non-finite f64 (`1e400`, `NaN`, `Infinity` are all parse errors),
    // but a finite `1e308` parses and overflows once scaled, so the
    // check has to sit on the scaled sum, not on the raw delta.
    let total = delta * POINTER_SCALE + *remainder;
    if !total.is_finite() {
        return 0;
    }
    let whole = total.round() as i32;
    *remainder = total - f64::from(whole);
    whole
}

struct InputDevice {
    device: evdev::uinput::VirtualDevice,
    x_remainder: f64,
    y_remainder: f64,
}

impl InputDevice {
    fn new() -> Option<Self> {
        let device = evdev::uinput::VirtualDevice::builder()
            .ok()?
            .name("rust-connect-presenter")
            // Buttons are never emitted, but without them the kernel does not
            // classify the device as a pointer: no `mouseN` handler is
            // attached (only the bare `eventN` one), and the desktop session
            // drops its REL_X/REL_Y motion entirely. Mousepad registers
            // BTN_LEFT/RIGHT/MIDDLE and gets `mouse5` (live-verified
            // 2026-07-30 in /proc/bus/input/devices).
            .with_keys(
                &[KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT]
                    .into_iter()
                    .collect::<AttributeSet<KeyCode>>(),
            )
            .ok()?
            .with_relative_axes(
                &[RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y]
                    .into_iter()
                    .collect::<AttributeSet<RelativeAxisCode>>(),
            )
            .ok()?
            .build()
            .ok()?;

        Some(Self {
            device,
            x_remainder: 0.0,
            y_remainder: 0.0,
        })
    }

    fn move_pointer(&mut self, dx: f64, dy: f64) {
        let px = scaled_pixels(dx, &mut self.x_remainder);
        let py = scaled_pixels(dy, &mut self.y_remainder);
        if px == 0 && py == 0 {
            return;
        }
        let events = [
            InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, px),
            InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, py),
            InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
        ];
        if let Err(e) = self.device.emit(&events) {
            warn!(error = %e, event = "presenter_emit_failed", "Failed to emit input event");
        }
    }

    fn reset(&mut self) {
        self.x_remainder = 0.0;
        self.y_remainder = 0.0;
    }
}

pub struct PresenterPlugin {
    events_received: AtomicUsize,
    input_device: Mutex<Option<InputDevice>>,
}

impl Default for PresenterPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl PresenterPlugin {
    pub fn new() -> Self {
        let input_device = InputDevice::new();
        if input_device.is_none() {
            warn!(
                event = "presenter_uinput_unavailable",
                "Could not create uinput device. Presenter pointer injection will be disabled. \
                 Ensure /dev/uinput is accessible (add user to 'input' group or run as root)."
            );
        } else {
            info!(
                event = "presenter_uinput_ready",
                "uinput device created successfully"
            );
        }
        Self {
            events_received: AtomicUsize::new(0),
            input_device: Mutex::new(input_device),
        }
    }

    /// Construct the packet handler without opening `/dev/uinput`.
    pub fn new_without_input() -> Self {
        Self {
            events_received: AtomicUsize::new(0),
            input_device: Mutex::new(None),
        }
    }

    pub fn events_received(&self) -> usize {
        self.events_received.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Plugin for PresenterPlugin {
    fn name(&self) -> &str {
        "presenter"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.presenter".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec![]
    }

    /// The sub-pixel remainder is shared by every device on the single
    /// uinput handle; a device that vanishes mid-gesture must not leave
    /// its fraction behind for the next one (2026-09-02 audit, B2).
    fn on_disconnected(&self, _device_id: &str) {
        if let Ok(mut guard) = self.input_device.lock() {
            if let Some(ref mut dev) = *guard {
                dev.reset();
            }
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let req: PresenterRequest = packet.body_as("presenter")?;

        self.events_received.fetch_add(1, Ordering::SeqCst);

        if req.stop {
            // kdeconnect-kde hides its pointer overlay here
            // (presenterplugin.cpp:69-73); GSConnect ignores stop
            // (presenter.js:45-48). We have no overlay — just reset state.
            debug!(
                device_id = %device_id,
                event = "presenter_stop",
                "Presenter: pointer mode stopped"
            );
            if let Ok(mut guard) = self.input_device.lock() {
                if let Some(ref mut dev) = *guard {
                    dev.reset();
                }
            }
        } else if req.dx.is_some() || req.dy.is_some() {
            let dx = req.dx.unwrap_or(0.0);
            let dy = req.dy.unwrap_or(0.0);
            debug!(
                device_id = %device_id,
                dx = dx,
                dy = dy,
                event = "presenter_pointer",
                "Presenter: pointer movement"
            );
            if let Ok(mut guard) = self.input_device.lock() {
                if let Some(ref mut dev) = *guard {
                    dev.move_pointer(dx, dy);
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_presenter_plugin_name() {
        let plugin = PresenterPlugin::new_without_input();
        assert_eq!(plugin.name(), "presenter");
    }

    #[tokio::test]
    async fn test_presenter_capabilities() {
        let plugin = PresenterPlugin::new_without_input();
        // Incoming-only, matching kdeconnect-kde (kdeconnect_presenter.json)
        // and GSConnect (presenter.js:15-16): we never send presenter packets.
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.presenter".to_string()));
        assert!(plugin.outgoing_capabilities().is_empty());
    }

    #[tokio::test]
    async fn test_handle_pointer_exact_android_shape() {
        // Upstream wire literal — see fixture provenance
        // tests/fixtures/upstream-wire/provenance.yaml: presenter/pointer.json
        // cited against kdeconnect-android PresenterPlugin.kt:77-82.
        let plugin = PresenterPlugin::new_without_input();
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/presenter/pointer.json"),
            )
            .expect("presenter/pointer.json"),
        )
        .expect("presenter/pointer.json parses");
        let packet = Packet::new("kdeconnect.presenter".to_string(), body);
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert_eq!(plugin.events_received(), 1);
    }

    #[tokio::test]
    async fn test_handle_stop_exact_android_shape() {
        // Upstream wire literal — see fixture provenance
        // tests/fixtures/upstream-wire/provenance.yaml: presenter/stop.json
        // cited against kdeconnect-android PresenterPlugin.kt:84-88.
        let plugin = PresenterPlugin::new_without_input();
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/presenter/stop.json"),
            )
            .expect("presenter/stop.json"),
        )
        .expect("presenter/stop.json parses");
        let packet = Packet::new("kdeconnect.presenter".to_string(), body);
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert_eq!(plugin.events_received(), 1);
    }

    #[tokio::test]
    async fn test_bogus_legacy_fields_are_ignored() {
        // The pre-cut implementation invented {"next": true} / {"previous": true}
        // bodies. No upstream version of the Android app has ever sent those in
        // a kdeconnect.presenter packet (slide navigation goes through
        // kdeconnect.mousepad.request specialKey — PresenterPlugin.kt:53-74).
        // Unknown fields must parse cleanly and inject nothing.
        let plugin = PresenterPlugin::new_without_input();
        for body in [
            serde_json::json!({ "next": true }),
            serde_json::json!({ "previous": true }),
            serde_json::json!({}),
        ] {
            let packet = Packet::new("kdeconnect.presenter".to_string(), body);
            assert!(plugin.handle_packet("device1", packet).await.is_ok());
        }
        assert_eq!(plugin.events_received(), 3);
    }

    #[tokio::test]
    async fn test_presenter_request_defaults() {
        let req: PresenterRequest =
            serde_json::from_value(serde_json::json!({})).expect("Value expected to be present");
        assert!(req.dx.is_none());
        assert!(req.dy.is_none());
        assert!(!req.stop);
    }

    /// B2 (2026-09-02 audit, corrected 09-03): the sub-pixel remainder is
    /// shared by every device on the single uinput handle, so one poisoned
    /// value turns every later movement from any device into a
    /// max-magnitude jump until a `stop`. The wire cannot deliver a
    /// non-finite float (serde_json rejects `1e400`, `NaN`, and `Infinity`),
    /// but it delivers `1e308` fine, and `1e308 * POINTER_SCALE` overflows
    /// to infinity after a raw-delta finite check. The guard has to sit on
    /// the scaled sum.
    #[tokio::test]
    async fn test_scaled_pixels_survives_finite_delta_that_overflows_when_scaled() {
        let wire: serde_json::Result<PresenterRequest> = serde_json::from_str(r#"{"dx": 1e400}"#);
        assert!(
            wire.is_err(),
            "serde_json accepted 1e400; the non-finite gate is live after all"
        );
        let wire: PresenterRequest =
            serde_json::from_str(r#"{"dx": 1e308}"#).expect("1e308 is a valid JSON number");
        let delta = wire.dx.expect("dx present");
        assert!(delta.is_finite());

        let mut rem = 0.0;
        let _ = scaled_pixels(delta, &mut rem);
        assert!(
            rem.is_finite(),
            "remainder poisoned by a finite delta: {rem}"
        );
        assert_eq!(scaled_pixels(0.0123, &mut rem), 12);
    }

    /// Boundary case for the same guard: not reachable from the wire (see
    /// above), kept so the guard's contract is stated for direct callers.
    #[tokio::test]
    async fn test_scaled_pixels_ignores_non_finite_delta() {
        let mut rem = 0.0;
        assert_eq!(scaled_pixels(f64::INFINITY, &mut rem), 0);
        assert_eq!(scaled_pixels(f64::NAN, &mut rem), 0);
        assert!(rem.is_finite(), "remainder poisoned: {rem}");
        assert_eq!(scaled_pixels(0.0123, &mut rem), 12);
    }

    #[tokio::test]
    async fn test_scaled_pixels_basic() {
        let mut rem = 0.0;
        // 0.0123 of a screen at 1000px/screen (GSConnect scale) = 12.3px → 12.
        assert_eq!(scaled_pixels(0.0123, &mut rem), 12);
        // Negative direction.
        assert_eq!(scaled_pixels(-0.0456, &mut 0.0), -46);
    }

    #[tokio::test]
    async fn test_scaled_pixels_accumulates_subpixel_remainder() {
        // 0.0004 of a screen = 0.4px per event. Each event alone rounds to 0
        // or 1, but over 5 events the emitted total must be 2px (5 * 0.4)
        // instead of losing the motion to rounding.
        let mut rem = 0.0;
        let emitted: Vec<i32> = (0..5).map(|_| scaled_pixels(0.0004, &mut rem)).collect();
        assert_eq!(emitted[0], 0);
        assert_eq!(emitted.iter().sum::<i32>(), 2);
    }
}

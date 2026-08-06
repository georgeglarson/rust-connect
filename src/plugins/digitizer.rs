//! Digitizer plugin
//!
//! Single Responsibility: Handle incoming KDE Connect digitizer packets and inject
//! them as absolute input events using Linux uinput.

use std::sync::RwLock;

use evdev::{
    uinput::VirtualDevice, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::plugins::plugin::Plugin;
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

/// Tool names AS THEY APPEAR ON THE WIRE. Android sends the Kotlin enum's
/// `.name`, which is capitalized: kdeconnect-android
/// .../plugins/digitizer/ToolEvent.kt:17-20 declares `enum class Tool { Pen,
/// Rubber }` and DigitizerPlugin.kt:69 sends `packet["tool"] = it.name`.
/// kdeconnect-kde matches the same literals in
/// plugins/digitizer/toolevent.h:12-13 (TOOL_PEN "Pen", TOOL_RUBBER
/// "Rubber"). Lowercase spellings have never been on the wire; the
/// comparison stays case-SENSITIVE so that stays visible.
const TOOL_PEN: &str = "Pen";
const TOOL_RUBBER: &str = "Rubber";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigitizerEvent {
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub touching: Option<bool>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub pressure: Option<f64>,
}

struct DigitizerSession {
    device: VirtualDevice,
    width: i32,
    height: i32,
}

pub struct DigitizerPlugin {
    sessions: RwLock<std::collections::HashMap<String, DigitizerSession>>,
}

impl Default for DigitizerPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl DigitizerPlugin {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(std::collections::HashMap::new()),
        }
    }

    fn start_session(
        &self,
        device_id: &str,
        width: i32,
        height: i32,
        resolution_x: i32,
        resolution_y: i32,
    ) -> Result<()> {
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::BTN_TOOL_PEN);
        keys.insert(KeyCode::BTN_TOOL_RUBBER);
        keys.insert(KeyCode::BTN_TOUCH);

        let mut abs_axes = AttributeSet::<AbsoluteAxisCode>::new();
        abs_axes.insert(AbsoluteAxisCode::ABS_X);
        abs_axes.insert(AbsoluteAxisCode::ABS_Y);
        abs_axes.insert(AbsoluteAxisCode::ABS_PRESSURE);

        let mut builder = evdev::uinput::VirtualDevice::builder()?;
        let dev_name = format!("rust-connect digitizer ({})", device_id);
        builder = builder.name(&dev_name);
        builder = builder.with_keys(&keys)?;

        let abs_x_info = evdev::UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            evdev::AbsInfo::new(
                0,
                0,
                (width - 1).clamp(0, 100000),
                0,
                0,
                resolution_x.clamp(0, 100000),
            ),
        );
        let abs_y_info = evdev::UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            evdev::AbsInfo::new(
                0,
                0,
                (height - 1).clamp(0, 100000),
                0,
                0,
                resolution_y.clamp(0, 100000),
            ),
        );
        let abs_pressure_info = evdev::UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_PRESSURE,
            evdev::AbsInfo::new(0, 0, 1023, 0, 0, 0),
        );

        builder = builder.with_absolute_axis(&abs_x_info)?;
        builder = builder.with_absolute_axis(&abs_y_info)?;
        builder = builder.with_absolute_axis(&abs_pressure_info)?;

        let virtual_device = builder.build()?;

        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        sessions.insert(
            device_id.to_string(),
            DigitizerSession {
                device: virtual_device,
                width,
                height,
            },
        );

        info!(device_id = %device_id, "Started digitizer session");
        Ok(())
    }

    fn end_session(&self, device_id: &str) {
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        if sessions.remove(device_id).is_some() {
            info!(device_id = %device_id, "Ended digitizer session");
        }
    }
}

/// Build the uinput event batch for one `kdeconnect.digitizer` body.
///
/// Pure on purpose: no session, no `/dev/uinput`, so the wire-shape mapping
/// is testable on any machine. Returns an empty vec when the body carries
/// nothing actionable; a non-empty batch always ends with SYN_REPORT.
fn build_digitizer_events(body: &serde_json::Value, width: i32, height: i32) -> Vec<InputEvent> {
    let mut events = Vec::new();

    let active = body.get("active").and_then(|v| v.as_bool());
    let touching = body.get("touching").and_then(|v| v.as_bool());
    let tool = body.get("tool").and_then(|v| v.as_str());
    let x = body
        .get("x")
        .and_then(|v| v.as_i64())
        .map(|v| (v as i32).clamp(0, width - 1));
    let y = body
        .get("y")
        .and_then(|v| v.as_i64())
        .map(|v| (v as i32).clamp(0, height - 1));
    let pressure = body.get("pressure").and_then(|v| v.as_f64());

    if active == Some(false) {
        events.push(InputEvent::new(
            EventType::KEY.0,
            KeyCode::BTN_TOOL_PEN.0,
            0,
        ));
        events.push(InputEvent::new(
            EventType::KEY.0,
            KeyCode::BTN_TOOL_RUBBER.0,
            0,
        ));
        events.push(InputEvent::new(EventType::KEY.0, KeyCode::BTN_TOUCH.0, 0));
        events.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_PRESSURE.0,
            0,
        ));
        events.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_X.0,
            0,
        ));
        events.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_Y.0,
            0,
        ));
    } else {
        if let Some(touching) = touching {
            if touching {
                events.push(InputEvent::new(EventType::KEY.0, KeyCode::BTN_TOUCH.0, 1));
            } else {
                events.push(InputEvent::new(EventType::KEY.0, KeyCode::BTN_TOUCH.0, 0));
                events.push(InputEvent::new(
                    EventType::ABSOLUTE.0,
                    AbsoluteAxisCode::ABS_PRESSURE.0,
                    0,
                ));
            }
        }

        if let Some(tool) = tool {
            events.push(InputEvent::new(
                EventType::KEY.0,
                KeyCode::BTN_TOOL_PEN.0,
                i32::from(tool == TOOL_PEN),
            ));
            events.push(InputEvent::new(
                EventType::KEY.0,
                KeyCode::BTN_TOOL_RUBBER.0,
                i32::from(tool == TOOL_RUBBER),
            ));
        }

        if let Some(x) = x {
            events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_X.0,
                x,
            ));
        }

        if let Some(y) = y {
            events.push(InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_Y.0,
                y,
            ));
        }

        if touching.unwrap_or(true) {
            if let Some(pressure) = pressure {
                let pressure_val = (pressure * 1023.0).clamp(0.0, 1023.0) as i32;
                events.push(InputEvent::new(
                    EventType::ABSOLUTE.0,
                    AbsoluteAxisCode::ABS_PRESSURE.0,
                    pressure_val,
                ));
            }
        }
    }

    if !events.is_empty() {
        events.push(InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0));
    }
    events
}

#[async_trait::async_trait]
impl Plugin for DigitizerPlugin {
    fn name(&self) -> &str {
        "digitizer"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.digitizer.session".to_string(),
            "kdeconnect.digitizer".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec![]
    }

    fn on_disconnected(&self, device_id: &str) {
        self.end_session(device_id);
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let body = &packet.body;

        if packet.packet_type == "kdeconnect.digitizer.session" {
            if let Some(action) = body.get("action").and_then(|v| v.as_str()) {
                if action == "start" {
                    let width = body.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let height = body.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let resolution_x = body
                        .get("resolutionX")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;
                    let resolution_y = body
                        .get("resolutionY")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;

                    if width > 0 && height > 0 {
                        if let Err(e) =
                            self.start_session(device_id, width, height, resolution_x, resolution_y)
                        {
                            error!(device_id = %device_id, error = %e, "Failed to start digitizer session");
                        }
                    }
                } else if action == "end" {
                    self.end_session(device_id);
                }
            }
        } else if packet.packet_type == "kdeconnect.digitizer" {
            let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
            if let Some(session) = sessions.get_mut(device_id) {
                let events = build_digitizer_events(body, session.width, session.height);
                if !events.is_empty() {
                    if let Err(e) = session.device.emit(&events) {
                        error!(device_id = %device_id, error = %e, "Failed to emit digitizer events");
                    }
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

    /// Value of a KEY event with this code, or None when the batch omits it.
    fn key_value(events: &[InputEvent], code: u16) -> Option<i32> {
        events
            .iter()
            .find(|e| e.event_type().0 == EventType::KEY.0 && e.code() == code)
            .map(|e| e.value())
    }

    /// Value of an ABSOLUTE event with this code, or None when omitted.
    fn abs_value(events: &[InputEvent], code: u16) -> Option<i32> {
        events
            .iter()
            .find(|e| e.event_type().0 == EventType::ABSOLUTE.0 && e.code() == code)
            .map(|e| e.value())
    }

    /// EXACT body a tablet sends for a pen stroke: `tool` is the Kotlin enum
    /// name, capitalized (kdeconnect-android .../digitizer/ToolEvent.kt:17-20,
    /// DigitizerPlugin.kt:69). The values come from the upstream-derived
    /// fixture tests/fixtures/upstream-wire/digitizer/pen_stroke.json.
    #[tokio::test]
    async fn test_capitalized_pen_activates_pen_tool() {
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/digitizer/pen_stroke.json"),
            )
            .expect("digitizer/pen_stroke.json"),
        )
        .expect("digitizer/pen_stroke.json parses");
        let events = build_digitizer_events(&body, 1000, 1000);
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_PEN.0), Some(1));
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_RUBBER.0), Some(0));
    }

    /// The eraser end of the stylus (ToolEvent.kt:19 `Rubber`; kdeconnect-kde
    /// plugins/digitizer/toolevent.h:13 TOOL_RUBBER "Rubber"). Values from
    /// tests/fixtures/upstream-wire/digitizer/rubber_stroke.json.
    #[tokio::test]
    async fn test_capitalized_rubber_activates_rubber_tool() {
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/digitizer/rubber_stroke.json"),
            )
            .expect("digitizer/rubber_stroke.json"),
        )
        .expect("digitizer/rubber_stroke.json parses");
        let events = build_digitizer_events(&body, 1000, 1000);
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_PEN.0), Some(0));
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_RUBBER.0), Some(1));
    }

    /// Regression: lowercase "pen" is what this plugin used to compare
    /// against, and it has never appeared on the wire. It must activate
    /// nothing, so a future "let's accept both" edit has to argue for itself.
    #[tokio::test]
    async fn test_lowercase_pen_activates_nothing() {
        let body = serde_json::json!({ "touching": true, "tool": "pen" });
        let events = build_digitizer_events(&body, 1000, 1000);
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_PEN.0), Some(0));
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_RUBBER.0), Some(0));
    }

    /// `active: false` ends the stroke: every button released, axes zeroed.
    #[tokio::test]
    async fn test_active_false_releases_everything() {
        let body = serde_json::json!({ "active": false });
        let events = build_digitizer_events(&body, 1000, 1000);
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_PEN.0), Some(0));
        assert_eq!(key_value(&events, KeyCode::BTN_TOOL_RUBBER.0), Some(0));
        assert_eq!(key_value(&events, KeyCode::BTN_TOUCH.0), Some(0));
        assert_eq!(
            abs_value(&events, AbsoluteAxisCode::ABS_PRESSURE.0),
            Some(0)
        );
    }

    /// Coordinates are clamped into the session the phone declared with
    /// `kdeconnect.digitizer.session` (DigitizerPlugin.kt:45-53).
    #[tokio::test]
    async fn test_coordinates_clamped_to_session_bounds() {
        let body = serde_json::json!({ "tool": "Pen", "x": 99999, "y": -5 });
        let events = build_digitizer_events(&body, 800, 600);
        assert_eq!(abs_value(&events, AbsoluteAxisCode::ABS_X.0), Some(799));
        assert_eq!(abs_value(&events, AbsoluteAxisCode::ABS_Y.0), Some(0));
    }

    /// A body with nothing actionable produces no batch, hence no SYN.
    #[tokio::test]
    async fn test_empty_body_produces_no_events() {
        let events = build_digitizer_events(&serde_json::json!({}), 800, 600);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn test_digitizer_capabilities() {
        let plugin = DigitizerPlugin::new();
        assert_eq!(plugin.name(), "digitizer");
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.digitizer".to_string()));
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.digitizer.session".to_string()));
        assert!(plugin.outgoing_capabilities().is_empty());
    }
}

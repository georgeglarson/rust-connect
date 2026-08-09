//! Mousepad plugin
//!
//! Single Responsibility: Handle kdeconnect.mousepad.request packets
//! for remote input (keyboard/mouse) from the phone.
//! Uses Linux uinput via the evdev crate to inject real keyboard/mouse events.

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use evdev::AttributeSet;
use evdev::{AbsoluteAxisCode, EventType, InputEvent, KeyCode, RelativeAxisCode};
use tracing::{debug, info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// Body of a `kdeconnect.mousepad.request` packet.
///
/// Field set and types mirror kdeconnect-kde's reader
/// (plugins/mousepad/x11remoteinput.cpp:92-105, mirrored in
/// waylandremoteinput.cpp:442-456) and kdeconnect-android's senders
/// (plugins/mousepad/MousePadPlugin.kt:77-186 and
/// KeyListenerView.java:129-165).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MousepadRequest {
    #[serde(default)]
    pub key: Option<String>,
    /// Android always sends this as an INTEGER code, never a string —
    /// every set site goes through SpecialKeysMap.get() which returns int
    /// (kdeconnect-android KeyListenerView.java:151-154,
    /// MousePadPlugin.kt:132-180). kdeconnect-kde reads it with
    /// np.get<int> (plugins/mousepad/x11remoteinput.cpp:105).
    #[serde(default)]
    pub special_key: Option<i32>,
    /// Relative pointer delta (kdeconnect-android MousePadPlugin.kt:77-82).
    /// When `scroll` is set these are wheel deltas instead (:124-130).
    #[serde(default)]
    pub dx: Option<f64>,
    #[serde(default)]
    pub dy: Option<f64>,
    /// Absolute pointer position. Only kdeconnect-kde's
    /// shareinputdevicesremote plugin ever produces these
    /// (plugins/shareinputdevicesremote/shareinputdevicesremoteplugin.cpp:74),
    /// and it hands the packet to its local mousepad plugin in-process
    /// rather than transmitting it — no wire producer exercises this path.
    /// See `absolute_position` for the decision and `scale_abs_coord` for
    /// how the values are mapped onto our synthetic absolute-pointer
    /// device.
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    /// The six click booleans, all lowercase on the wire with no camel
    /// humps. kdeconnect-kde reads them at x11remoteinput.cpp:97-102;
    /// kdeconnect-android sends one per packet
    /// (MousePadPlugin.kt:88-122).
    #[serde(default)]
    pub singleclick: bool,
    #[serde(default)]
    pub doubleclick: bool,
    #[serde(default)]
    pub middleclick: bool,
    #[serde(default)]
    pub rightclick: bool,
    #[serde(default)]
    pub singlehold: bool,
    #[serde(default)]
    pub singlerelease: bool,
    /// Reinterprets dx/dy as wheel deltas
    /// (x11remoteinput.cpp:103, :137-144;
    /// kdeconnect-android MousePadPlugin.kt:124-130).
    #[serde(default)]
    pub scroll: bool,
    /// Four INDEPENDENT modifier booleans, not one string
    /// (x11remoteinput.cpp:146-149; KeyListenerView.java:132-149).
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    /// `super` is a Rust keyword, so the wire name is set explicitly.
    /// An explicit `rename` overrides the container's `rename_all`.
    #[serde(default, rename = "super")]
    pub super_key: bool,
    #[serde(default)]
    pub is_ack: bool,
}

/// Modifier keys held around a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Modifiers {
    ctrl: bool,
    alt: bool,
    shift: bool,
    super_key: bool,
}

impl Modifiers {
    fn from_request(req: &MousepadRequest) -> Self {
        Self {
            ctrl: req.ctrl,
            alt: req.alt,
            shift: req.shift,
            super_key: req.super_key,
        }
    }

    /// Modifier keys in the order kdeconnect-kde presses (and releases)
    /// them: ctrl, alt, shift, super — x11remoteinput.cpp:151-158 and
    /// :181-188, mirrored in waylandremoteinput.cpp:486-493 and :511-518.
    /// The LEFT-hand variant is used for each, as upstream does.
    fn keys(self) -> Vec<KeyCode> {
        let mut keys = Vec::new();
        if self.ctrl {
            keys.push(KeyCode::KEY_LEFTCTRL);
        }
        if self.alt {
            keys.push(KeyCode::KEY_LEFTALT);
        }
        if self.shift {
            keys.push(KeyCode::KEY_LEFTSHIFT);
        }
        if self.super_key {
            keys.push(KeyCode::KEY_LEFTMETA);
        }
        keys
    }
}

/// One injectable input event group, decided purely from the packet body.
///
/// Keeping this separate from `InputDevice` is what lets every wire-shape
/// decision be unit-tested without opening `/dev/uinput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAction {
    /// Press then release once.
    Click(KeyCode),
    /// Press then release twice.
    DoubleClick(KeyCode),
    /// Press and hold (drag start).
    ButtonDown(KeyCode),
    /// Release a held button (drag end).
    ButtonUp(KeyCode),
    /// One wheel notch. `clicks` is signed: +1 is up (or right).
    Wheel { horizontal: bool, clicks: i32 },
    /// A key press with modifiers held around it.
    KeyPress { key: KeyCode, mods: Modifiers },
    /// Relative pointer motion in whole pixels.
    Move { dx: i32, dy: i32 },
}

struct InputDevice {
    device: evdev::uinput::VirtualDevice,
}

impl InputDevice {
    fn new() -> Option<Self> {
        let keys: AttributeSet<KeyCode> = [
            KeyCode::KEY_A,
            KeyCode::KEY_B,
            KeyCode::KEY_C,
            KeyCode::KEY_D,
            KeyCode::KEY_E,
            KeyCode::KEY_F,
            KeyCode::KEY_G,
            KeyCode::KEY_H,
            KeyCode::KEY_I,
            KeyCode::KEY_J,
            KeyCode::KEY_K,
            KeyCode::KEY_L,
            KeyCode::KEY_M,
            KeyCode::KEY_N,
            KeyCode::KEY_O,
            KeyCode::KEY_P,
            KeyCode::KEY_Q,
            KeyCode::KEY_R,
            KeyCode::KEY_S,
            KeyCode::KEY_T,
            KeyCode::KEY_U,
            KeyCode::KEY_V,
            KeyCode::KEY_W,
            KeyCode::KEY_X,
            KeyCode::KEY_Y,
            KeyCode::KEY_Z,
            KeyCode::KEY_0,
            KeyCode::KEY_1,
            KeyCode::KEY_2,
            KeyCode::KEY_3,
            KeyCode::KEY_4,
            KeyCode::KEY_5,
            KeyCode::KEY_6,
            KeyCode::KEY_7,
            KeyCode::KEY_8,
            KeyCode::KEY_9,
            KeyCode::KEY_ENTER,
            KeyCode::KEY_ESC,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_TAB,
            KeyCode::KEY_SPACE,
            KeyCode::KEY_LEFT,
            KeyCode::KEY_RIGHT,
            KeyCode::KEY_UP,
            KeyCode::KEY_DOWN,
            KeyCode::KEY_LEFTSHIFT,
            KeyCode::KEY_RIGHTSHIFT,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_RIGHTCTRL,
            KeyCode::KEY_LEFTALT,
            KeyCode::KEY_RIGHTALT,
            KeyCode::KEY_DELETE,
            KeyCode::KEY_HOME,
            KeyCode::KEY_END,
            KeyCode::KEY_PAGEUP,
            KeyCode::KEY_PAGEDOWN,
            KeyCode::KEY_LINEFEED,
            KeyCode::KEY_SYSRQ,
            KeyCode::KEY_SCROLLLOCK,
            KeyCode::KEY_F1,
            KeyCode::KEY_F2,
            KeyCode::KEY_F3,
            KeyCode::KEY_F4,
            KeyCode::KEY_F5,
            KeyCode::KEY_F6,
            KeyCode::KEY_F7,
            KeyCode::KEY_F8,
            KeyCode::KEY_F9,
            KeyCode::KEY_F10,
            KeyCode::KEY_F11,
            KeyCode::KEY_F12,
            KeyCode::KEY_VOLUMEUP,
            KeyCode::KEY_VOLUMEDOWN,
            KeyCode::KEY_MUTE,
            KeyCode::KEY_PLAYPAUSE,
            KeyCode::KEY_NEXTSONG,
            KeyCode::KEY_PREVIOUSSONG,
            KeyCode::KEY_LEFTMETA,
            KeyCode::BTN_LEFT,
            KeyCode::BTN_RIGHT,
            KeyCode::BTN_MIDDLE,
        ]
        .into_iter()
        .collect();

        let device = evdev::uinput::VirtualDevice::builder()
            .ok()?
            .name("rust-connect-mousepad")
            .with_keys(&keys)
            .ok()?
            // REL_WHEEL / REL_HWHEEL are required for scroll packets; without
            // them the kernel drops the wheel events silently.
            .with_relative_axes(
                &[
                    RelativeAxisCode::REL_X,
                    RelativeAxisCode::REL_Y,
                    RelativeAxisCode::REL_WHEEL,
                    RelativeAxisCode::REL_HWHEEL,
                ]
                .into_iter()
                .collect::<AttributeSet<RelativeAxisCode>>(),
            )
            .ok()?
            .build()
            .ok()?;

        Some(Self { device })
    }

    /// Executes one planned action. Every action ends with a SYN report so
    /// the kernel delivers it as a complete event group.
    fn apply(&mut self, action: InputAction) {
        match action {
            InputAction::Click(key) => self.emit(vec![
                InputEvent::new(EventType::KEY.0, key.0, 1),
                InputEvent::new(EventType::KEY.0, key.0, 0),
            ]),
            InputAction::DoubleClick(key) => self.emit(vec![
                InputEvent::new(EventType::KEY.0, key.0, 1),
                InputEvent::new(EventType::KEY.0, key.0, 0),
                InputEvent::new(EventType::KEY.0, key.0, 1),
                InputEvent::new(EventType::KEY.0, key.0, 0),
            ]),
            InputAction::ButtonDown(key) => {
                self.emit(vec![InputEvent::new(EventType::KEY.0, key.0, 1)])
            }
            InputAction::ButtonUp(key) => {
                self.emit(vec![InputEvent::new(EventType::KEY.0, key.0, 0)])
            }
            InputAction::Wheel { horizontal, clicks } => {
                let axis = if horizontal {
                    RelativeAxisCode::REL_HWHEEL
                } else {
                    RelativeAxisCode::REL_WHEEL
                };
                self.emit(vec![InputEvent::new(EventType::RELATIVE.0, axis.0, clicks)]);
            }
            InputAction::KeyPress { key, mods } => {
                let mod_keys = mods.keys();
                let mut events = Vec::with_capacity(mod_keys.len() * 2 + 2);
                for m in &mod_keys {
                    events.push(InputEvent::new(EventType::KEY.0, m.0, 1));
                }
                events.push(InputEvent::new(EventType::KEY.0, key.0, 1));
                events.push(InputEvent::new(EventType::KEY.0, key.0, 0));
                for m in &mod_keys {
                    events.push(InputEvent::new(EventType::KEY.0, m.0, 0));
                }
                self.emit(events);
            }
            InputAction::Move { dx, dy } => self.emit(vec![
                InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, dx),
                InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, dy),
            ]),
        }
    }

    fn emit(&mut self, mut events: Vec<InputEvent>) {
        events.push(InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0));
        if let Err(e) = self.device.emit(&events) {
            warn!(error = %e, event = "mousepad_emit_failed", "Failed to emit input event");
        }
    }
}

/// Maps an Android specialKey integer code to an evdev key.
///
/// The code list is defined by kdeconnect-android's SpecialKeysMap
/// (plugins/mousepad/KeyListenerView.java:26-57). The desktop translation
/// table is "to keep in sync within all the implementations" per kdeconnect-kde
/// (plugins/mousepad/x11remoteinput.cpp:27-61, codes 1..32; 17-20 are the
/// four modifier keys, and the validity guard accepts the whole range at
/// x11remoteinput.cpp:106);
/// GSConnect agrees (src/service/plugins/mousepad.js:45-78). Codes with no
/// upstream mapping (0, >32) are ignored, as kdeconnect-kde does
/// (its guard rejects them at x11remoteinput.cpp:106).
fn special_key_code(code: i32) -> Option<KeyCode> {
    match code {
        1 => Some(KeyCode::KEY_BACKSPACE),
        2 => Some(KeyCode::KEY_TAB),
        3 => Some(KeyCode::KEY_LINEFEED),
        4 => Some(KeyCode::KEY_LEFT),
        5 => Some(KeyCode::KEY_UP),
        6 => Some(KeyCode::KEY_RIGHT),
        7 => Some(KeyCode::KEY_DOWN),
        8 => Some(KeyCode::KEY_PAGEUP),
        9 => Some(KeyCode::KEY_PAGEDOWN),
        10 => Some(KeyCode::KEY_HOME),
        11 => Some(KeyCode::KEY_END),
        12 => Some(KeyCode::KEY_ENTER),
        13 => Some(KeyCode::KEY_DELETE),
        14 => Some(KeyCode::KEY_ESC),
        15 => Some(KeyCode::KEY_SYSRQ),
        16 => Some(KeyCode::KEY_SCROLLLOCK),
        // 17-20 are the four modifier keys, not a gap: XK_Control_L,
        // XK_Alt_L, XK_Shift_L, XK_Super_L (x11remoteinput.cpp:45-48).
        // kdeconnect-android sends 20 for its TV Home button
        // (MousePadPlugin.kt:162-167).
        17 => Some(KeyCode::KEY_LEFTCTRL),
        18 => Some(KeyCode::KEY_LEFTALT),
        19 => Some(KeyCode::KEY_LEFTSHIFT),
        20 => Some(KeyCode::KEY_LEFTMETA),
        21 => Some(KeyCode::KEY_F1),
        22 => Some(KeyCode::KEY_F2),
        23 => Some(KeyCode::KEY_F3),
        24 => Some(KeyCode::KEY_F4),
        25 => Some(KeyCode::KEY_F5),
        26 => Some(KeyCode::KEY_F6),
        27 => Some(KeyCode::KEY_F7),
        28 => Some(KeyCode::KEY_F8),
        29 => Some(KeyCode::KEY_F9),
        30 => Some(KeyCode::KEY_F10),
        31 => Some(KeyCode::KEY_F11),
        32 => Some(KeyCode::KEY_F12),
        _ => None,
    }
}

fn parse_key_code(key: &str) -> Option<KeyCode> {
    match key.to_lowercase().as_str() {
        "enter" | "return" => Some(KeyCode::KEY_ENTER),
        "esc" | "escape" => Some(KeyCode::KEY_ESC),
        "backspace" => Some(KeyCode::KEY_BACKSPACE),
        "tab" => Some(KeyCode::KEY_TAB),
        "space" => Some(KeyCode::KEY_SPACE),
        "delete" | "del" => Some(KeyCode::KEY_DELETE),
        "home" => Some(KeyCode::KEY_HOME),
        "end" => Some(KeyCode::KEY_END),
        "pageup" | "page_up" => Some(KeyCode::KEY_PAGEUP),
        "pagedown" | "page_down" => Some(KeyCode::KEY_PAGEDOWN),
        "left" | "arrowleft" => Some(KeyCode::KEY_LEFT),
        "right" | "arrowright" => Some(KeyCode::KEY_RIGHT),
        "up" | "arrowup" => Some(KeyCode::KEY_UP),
        "down" | "arrowdown" => Some(KeyCode::KEY_DOWN),
        "volumemute" | "mute" => Some(KeyCode::KEY_MUTE),
        "volumeup" => Some(KeyCode::KEY_VOLUMEUP),
        "volumedown" => Some(KeyCode::KEY_VOLUMEDOWN),
        "playpause" | "play" | "pause" => Some(KeyCode::KEY_PLAYPAUSE),
        "nextsong" | "next" => Some(KeyCode::KEY_NEXTSONG),
        "previoussong" | "previous" => Some(KeyCode::KEY_PREVIOUSSONG),
        c if c.len() == 1 => {
            #[allow(clippy::expect_used)]
            let ch = c
                .chars()
                .next()
                .expect("len == 1 guarantees at least one char");
            if ch.is_ascii_lowercase() || ch.is_ascii_uppercase() {
                let upper = ch.to_ascii_uppercase();
                KeyCode::from_str(&format!("KEY_{}", upper)).ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// One wheel notch per scroll packet, in the sign of the delta.
///
/// kdeconnect-kde's X11 backend emits exactly one wheel button
/// press+release per packet regardless of |dy|: dy > 0 is MouseWheelUp
/// (X11 button 4), dy < 0 is MouseWheelDown (button 5), dy == 0 emits
/// nothing (plugins/mousepad/x11remoteinput.cpp:137-144, buttons
/// enumerated at :22-23). No scaling by magnitude appears anywhere: the
/// sender accumulates and thresholds instead
/// (kdeconnect-android MousePadActivity.java:398-403, :418-423).
/// evdev's REL_WHEEL uses the same sign convention as the packet
/// (positive is away from the user), so the sign passes through.
///
/// Horizontal: the X11 backend ignores dx; the Wayland backend forwards it
/// (waylandremoteinput.cpp:479 -> pointerAxis -> ei_device_scroll_delta at
/// :377). kdeconnect-android only ever sends dx = 0 for scroll
/// (MousePadActivity.java:576 calls sendScroll(0, y);
/// MousePadPlugin.kt:124-130), so this arm serves desktop peers.
fn wheel_actions(dx: f64, dy: f64) -> Vec<InputAction> {
    let mut actions = Vec::new();
    if dy > 0.0 {
        actions.push(InputAction::Wheel {
            horizontal: false,
            clicks: 1,
        });
    } else if dy < 0.0 {
        actions.push(InputAction::Wheel {
            horizontal: false,
            clicks: -1,
        });
    }
    if dx > 0.0 {
        actions.push(InputAction::Wheel {
            horizontal: true,
            clicks: 1,
        });
    } else if dx < 0.0 {
        actions.push(InputAction::Wheel {
            horizontal: true,
            clicks: -1,
        });
    }
    actions
}

/// Decides what to inject for one packet, from the packet alone.
///
/// The branch structure mirrors kdeconnect-kde exactly
/// (x11remoteinput.cpp:113-198, mirrored waylandremoteinput.cpp:458-525):
/// a click / scroll / key packet is handled by a strict-priority chain and
/// NEVER also moves the pointer; only a packet with none of those set is
/// treated as pointer movement.
fn plan_actions(req: &MousepadRequest) -> Vec<InputAction> {
    let has_key = req.key.as_deref().is_some_and(|k| !k.is_empty());
    let special = req.special_key.unwrap_or(0);

    // Upstream's guard, verbatim in shape: any click / scroll / key packet
    // takes the priority chain and never also moves the pointer
    // (x11remoteinput.cpp:113, waylandremoteinput.cpp:458).
    if req.singleclick
        || req.doubleclick
        || req.middleclick
        || req.rightclick
        || req.singlehold
        || req.singlerelease
        || req.scroll
        || has_key
        || special != 0
    {
        // Strict priority, in upstream's order
        // (x11remoteinput.cpp:118-145).
        if req.singleclick {
            vec![InputAction::Click(KeyCode::BTN_LEFT)]
        } else if req.doubleclick {
            vec![InputAction::DoubleClick(KeyCode::BTN_LEFT)]
        } else if req.middleclick {
            vec![InputAction::Click(KeyCode::BTN_MIDDLE)]
        } else if req.rightclick {
            vec![InputAction::Click(KeyCode::BTN_RIGHT)]
        } else if req.singlehold {
            // Drag start: press without releasing
            // (x11remoteinput.cpp:132-134).
            vec![InputAction::ButtonDown(KeyCode::BTN_LEFT)]
        } else if req.singlerelease {
            // Drag end (x11remoteinput.cpp:135-136).
            vec![InputAction::ButtonUp(KeyCode::BTN_LEFT)]
        } else if req.scroll {
            // dx/dy are wheel deltas, not pointer motion
            // (x11remoteinput.cpp:137-144).
            wheel_actions(req.dx.unwrap_or(0.0), req.dy.unwrap_or(0.0))
        } else {
            key_actions(req, has_key, special)
        }
    } else if req.dx.is_some() || req.dy.is_some() {
        // Upstream truncates toward zero: XWarpPointer takes (int)dx,
        // (int)dy (x11remoteinput.cpp:192-193).
        let dx = req.dx.unwrap_or(0.0) as i32;
        let dy = req.dy.unwrap_or(0.0) as i32;
        if dx == 0 && dy == 0 {
            vec![]
        } else {
            vec![InputAction::Move { dx, dy }]
        }
    } else {
        vec![]
    }
}

/// Fixed logical coordinate range advertised on the absolute-pointer uinput
/// device (`AbsoluteInputDevice`).
///
/// libinput maps a device's declared ABS_X/ABS_Y min/max linearly across
/// the receiving screen's real pixels — the same mechanism
/// `src/plugins/digitizer.rs` already relies on, there with a
/// phone-declared width/height as the range because the digitizer wire
/// protocol negotiates one first (`kdeconnect.digitizer.session`).
/// Mousepad has no such negotiation and, per `absolute_position`'s doc, no
/// live wire producer to observe, so a fixed constant is the only
/// available choice. 65535 matches the convention used by QEMU's
/// usb-tablet device, VNC, and RDP absolute pointers (16-bit range,
/// "0..65535 == 0%..100% of the target screen").
const ABS_RANGE_MAX: i32 = 65535;

/// Maps a wire coordinate onto the fixed `ABS_RANGE_MAX` logical range.
///
/// DIVERGENCE, documented: upstream's x/y are the *sender's* real screen
/// pixel coordinates (kdeconnect-kde XWarpPointer takes them verbatim —
/// x11remoteinput.cpp:196-197 — and `pointerMotionAbsolute` forwards them
/// unchanged to libei/the portal — waylandremoteinput.cpp:394-401,
/// 523-524). That is only meaningful when sender and receiver share a
/// screen, which is true for upstream's sole in-process producer
/// (shareinputdevicesremote) and never true for a real network peer. We
/// have no screen-geometry query in this codebase to scale against even if
/// a peer did send real pixels, so incoming coordinates are rounded and
/// clamped directly into `[0, ABS_RANGE_MAX]` with no separate
/// normalization step. A peer that sends coordinates already within that
/// range lands correctly; no real display exceeds 65535px in either
/// dimension, so clamping there costs nothing in practice.
fn scale_abs_coord(v: f64) -> i32 {
    v.round().clamp(0.0, ABS_RANGE_MAX as f64) as i32
}

/// Absolute pointer coordinates for a packet, when that is ALL the packet
/// carries — mirrors upstream's branch order: the absolute arm is reached
/// only after the click/key chain and only when dx/dy are zero or absent
/// (x11remoteinput.cpp:191-197, waylandremoteinput.cpp:521-524).
fn absolute_position(req: &MousepadRequest) -> Option<(i32, i32)> {
    if (req.x.is_some() || req.y.is_some()) && plan_actions(req).is_empty() {
        Some((
            scale_abs_coord(req.x.unwrap_or(0.0)),
            scale_abs_coord(req.y.unwrap_or(0.0)),
        ))
    } else {
        None
    }
}

/// A second, single-purpose uinput device for absolute pointer positioning.
///
/// Kept separate from `InputDevice` rather than adding ABS axes onto the
/// same virtual device: mixing REL and ABS axes on one uinput device makes
/// kernel/libinput device classification unreliable and version-dependent.
/// `InputDevice` already carries one hard-won classification lesson of its
/// own kind — a device with no EV_KEY buttons at all gets no `mouseN`
/// handler, only a bare `eventN` one, and its motion is silently dropped
/// (see `src/plugins/presenter.rs`'s `InputDevice::new`, "no mouseN
/// handler"). A second device sidesteps the ambiguity rather than risking
/// a second, harder-to-diagnose variant of the same failure mode.
///
/// Created lazily, on the first absolute packet a device sends: most
/// sessions never send one (see `absolute_position`'s doc), so most
/// daemon runs never need a second virtual input device to exist at all.
struct AbsoluteInputDevice {
    device: evdev::uinput::VirtualDevice,
}

impl AbsoluteInputDevice {
    fn new() -> Option<Self> {
        let device = evdev::uinput::VirtualDevice::builder()
            .ok()?
            .name("rust-connect-mousepad-absolute")
            // No click is ever emitted from this device — clicks stay on
            // the primary REL device above — but at least one EV_KEY is
            // required for the kernel to attach a mouseN handler (same
            // finding as InputDevice::new above and presenter.rs).
            .with_keys(
                &[KeyCode::BTN_LEFT]
                    .into_iter()
                    .collect::<AttributeSet<KeyCode>>(),
            )
            .ok()?
            .with_absolute_axis(&evdev::UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_X,
                evdev::AbsInfo::new(0, 0, ABS_RANGE_MAX, 0, 0, 0),
            ))
            .ok()?
            .with_absolute_axis(&evdev::UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_Y,
                evdev::AbsInfo::new(0, 0, ABS_RANGE_MAX, 0, 0, 0),
            ))
            .ok()?
            .build()
            .ok()?;

        Some(Self { device })
    }

    fn move_to(&mut self, x: i32, y: i32) {
        let events = [
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x),
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y),
            InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
        ];
        if let Err(e) = self.device.emit(&events) {
            warn!(error = %e, event = "mousepad_abs_emit_failed", "Failed to emit absolute pointer event");
        }
    }
}

/// Key injection with modifiers held around it.
///
/// A valid `specialKey` wins over `key`, as upstream does
/// (x11remoteinput.cpp:160-179: `if (validSpecialKey) { ... } else { ...
/// key ... }`). No real client sends both — kdeconnect-android picks one
/// or the other in a single if/else (KeyListenerView.java:151-163).
fn key_actions(req: &MousepadRequest, has_key: bool, special: i32) -> Vec<InputAction> {
    let mods = Modifiers::from_request(req);

    if let Some(key_code) = special_key_code(special) {
        return vec![InputAction::KeyPress {
            key: key_code,
            mods,
        }];
    }

    if has_key {
        if let Some(ref key) = req.key {
            if let Some(key_code) = parse_key_code(key) {
                return vec![InputAction::KeyPress {
                    key: key_code,
                    mods,
                }];
            }
        }
    }

    vec![]
}

pub struct MousepadPlugin {
    events_received: AtomicUsize,
    input_device: Mutex<Option<InputDevice>>,
    /// Lazily created on the first absolute-positioning packet — see
    /// `AbsoluteInputDevice`'s doc.
    abs_input_device: Mutex<Option<AbsoluteInputDevice>>,
    /// Whether this instance may open real uinput devices at all.
    /// `new_without_input()` sets this false: unlike `input_device` (which
    /// is decided once at construction and never retried), the absolute
    /// device is created lazily on first use, so without this flag a
    /// fixture packet routed through `new_without_input()` would still
    /// reach for `/dev/uinput` the first time it carried x/y — defeating
    /// the whole point of that constructor.
    uinput_enabled: bool,
}

impl Default for MousepadPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl MousepadPlugin {
    pub fn new() -> Self {
        let input_device = InputDevice::new();
        if input_device.is_none() {
            warn!(
                event = "mousepad_uinput_unavailable",
                "Could not create uinput device. Mousepad input injection will be disabled. \
                 Ensure /dev/uinput is accessible (add user to 'input' group or run as root)."
            );
        } else {
            info!(
                event = "mousepad_uinput_ready",
                "uinput device created successfully"
            );
        }
        Self {
            events_received: AtomicUsize::new(0),
            input_device: Mutex::new(input_device),
            abs_input_device: Mutex::new(None),
            uinput_enabled: true,
        }
    }

    /// Construct the protocol handler without opening `/dev/uinput`.
    /// Ordinary tests use this explicit seam so fixture packets can never
    /// become real keyboard or pointer events.
    pub fn new_without_input() -> Self {
        Self {
            events_received: AtomicUsize::new(0),
            input_device: Mutex::new(None),
            abs_input_device: Mutex::new(None),
            uinput_enabled: false,
        }
    }

    pub fn events_received(&self) -> usize {
        self.events_received.load(Ordering::SeqCst)
    }

    /// Injects an absolute pointer position, creating the second uinput
    /// device on first use. A creation failure is an environment gap —
    /// logged and dropped, exactly like the primary `InputDevice`'s own
    /// unavailable-uinput path in `new()` — not a code gap.
    fn inject_absolute(&self, device_id: &str, x: i32, y: i32) {
        if !self.uinput_enabled {
            return;
        }
        let Ok(mut guard) = self.abs_input_device.lock() else {
            return;
        };
        if guard.is_none() {
            *guard = AbsoluteInputDevice::new();
            if guard.is_some() {
                info!(
                    device_id = %device_id,
                    event = "mousepad_abs_uinput_ready",
                    "Absolute-pointer uinput device created"
                );
            } else {
                warn!(
                    device_id = %device_id,
                    event = "mousepad_abs_uinput_unavailable",
                    "Could not create the absolute-pointer uinput device; \
                     packet dropped. Ensure /dev/uinput is accessible \
                     (add user to 'input' group or run as root)."
                );
            }
        }
        if let Some(ref mut dev) = *guard {
            dev.move_to(x, y);
            info!(
                device_id = %device_id,
                x = x,
                y = y,
                event = "mousepad_absolute_injected",
                "Injected absolute pointer position"
            );
        }
    }
}

#[async_trait::async_trait]
impl Plugin for MousepadPlugin {
    fn name(&self) -> &str {
        "mousepad"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.mousepad.request".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.mousepad.keyboardstate".to_string()]
    }

    fn on_connected(&self, device_id: &str) -> Vec<Packet> {
        // kdeconnect-kde announces keyboard availability on connect:
        // MousepadPlugin::connected() sets a `state` bool from
        // hasKeyboardSupport() (plugins/mousepad/mousepadplugin.cpp:63-70;
        // the X11 backend returns true unconditionally,
        // x11remoteinput.cpp:203-206). kdeconnect-android reads it into
        // isKeyboardEnabled and greys out its keyboard button when false
        // (MousePadPlugin.kt:26-29).
        //
        // Divergence, deliberate: upstream omits `state` when no backend
        // loaded and Android defaults it to true (MousePadPlugin.kt:27).
        // We report whether the uinput device actually opened, because a
        // daemon that cannot reach /dev/uinput cannot type.
        let state = self
            .input_device
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false);

        debug!(
            device_id = %device_id,
            state = state,
            event = "mousepad_keyboardstate",
            "Announcing keyboard availability"
        );

        vec![Packet::new(
            "kdeconnect.mousepad.keyboardstate".to_string(),
            serde_json::json!({ "state": state }),
        )]
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let req: MousepadRequest = packet.body_as("mousepad")?;

        self.events_received.fetch_add(1, Ordering::SeqCst);

        if req.is_ack {
            debug!(
                device_id = %device_id,
                event = "mousepad_ack",
                "Received mousepad ACK"
            );
        }

        let actions = plan_actions(&req);

        if let Some((x, y)) = absolute_position(&req) {
            self.inject_absolute(device_id, x, y);
        }

        debug!(
            device_id = %device_id,
            actions = actions.len(),
            event = "mousepad_input",
            "Planned mousepad input actions"
        );

        if actions.is_empty() {
            return Ok(None);
        }

        if let Ok(mut guard) = self.input_device.lock() {
            if let Some(ref mut dev) = *guard {
                for action in actions {
                    dev.apply(action);
                }
                info!(
                    device_id = %device_id,
                    event = "mousepad_injected",
                    "Injected mousepad input"
                );
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
    async fn test_mousepad_plugin_name() {
        let plugin = MousepadPlugin::new_without_input();
        assert_eq!(plugin.name(), "mousepad");
    }

    #[tokio::test]
    async fn test_mousepad_capabilities() {
        let plugin = MousepadPlugin::new_without_input();
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.mousepad.request".to_string()));
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.mousepad.keyboardstate".to_string()));
    }

    #[tokio::test]
    async fn test_handle_key_press() {
        // Real Android shape: KeyListenerView.java:142-144 sets a bare
        // "shift": true bool next to "key", not a "modifier" string.
        let plugin = MousepadPlugin::new_without_input();
        let packet = Packet::new(
            "kdeconnect.mousepad.request".to_string(),
            serde_json::json!({
                "key": "a",
                "shift": true
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert_eq!(plugin.events_received(), 1);
    }

    #[tokio::test]
    async fn test_handle_mouse_movement() {
        let plugin = MousepadPlugin::new_without_input();
        let packet = Packet::new(
            "kdeconnect.mousepad.request".to_string(),
            serde_json::json!({
                "dx": 10.5,
                "dy": -3.2
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert_eq!(plugin.events_received(), 1);
    }

    #[tokio::test]
    async fn test_handle_invented_button_packet_is_inert() {
        // The invented "button" field is gone from MousepadRequest. A
        // packet carrying it must still parse (serde ignores unknown
        // fields) and must inject nothing — asserted structurally in
        // test_plan_ignores_invented_button_field.
        let plugin = MousepadPlugin::new_without_input();
        let packet = Packet::new(
            "kdeconnect.mousepad.request".to_string(),
            serde_json::json!({
                "button": 1
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert_eq!(plugin.events_received(), 1);
    }

    #[tokio::test]
    async fn test_handle_special_key() {
        // Exact wire body the phone sends for a special key: an INTEGER code
        // (kdeconnect-android KeyListenerView.java:154 — np.set("specialKey",
        // specialKey) where specialKey comes from the int SpecialKeysMap).
        // Code 12 = ENTER (KeyListenerView.java:30, x11remoteinput.cpp:42).
        let plugin = MousepadPlugin::new_without_input();
        let packet = Packet::new(
            "kdeconnect.mousepad.request".to_string(),
            serde_json::json!({
                "specialKey": 12
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert_eq!(plugin.events_received(), 1);
    }

    /// Fixture: tests/fixtures/upstream-wire/mousepad/presenter_slide_keys.json
    ///   kdeconnect-android@a88f6fa0 PresenterPlugin.kt:53-74 routes these
    ///   through mousepad.request: PAGE_UP=8, PAGE_DOWN=9, F5=25, ESC=14.
    ///   KeyListenerView.java:36-37,48,53 maps the keyEvent keyCodes.
    #[tokio::test]
    async fn test_handle_presenter_slide_keys_exact_wire_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/mousepad/presenter_slide_keys.json");
        let bodies: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read presenter fixture"),
        )
        .expect("parse fixture")
        .as_array()
        .expect("fixture is a JSON array")
        .clone();

        let plugin = MousepadPlugin::new_without_input();
        for body in bodies {
            let code = body["specialKey"].as_i64().unwrap();
            let packet = Packet::new("kdeconnect.mousepad.request".to_string(), body);
            assert!(
                plugin.handle_packet("device1", packet).await.is_ok(),
                "specialKey {code} must parse and be handled"
            );
        }
        assert_eq!(plugin.events_received(), 4);
    }

    #[tokio::test]
    async fn test_special_key_code_mapping_matches_upstream() {
        // Full table, kept in sync with kdeconnect-kde
        // plugins/mousepad/x11remoteinput.cpp:24-58 and GSConnect
        // src/service/plugins/mousepad.js:45-78.
        assert_eq!(special_key_code(1), Some(KeyCode::KEY_BACKSPACE));
        assert_eq!(special_key_code(2), Some(KeyCode::KEY_TAB));
        assert_eq!(special_key_code(3), Some(KeyCode::KEY_LINEFEED));
        assert_eq!(special_key_code(4), Some(KeyCode::KEY_LEFT));
        assert_eq!(special_key_code(5), Some(KeyCode::KEY_UP));
        assert_eq!(special_key_code(6), Some(KeyCode::KEY_RIGHT));
        assert_eq!(special_key_code(7), Some(KeyCode::KEY_DOWN));
        assert_eq!(special_key_code(8), Some(KeyCode::KEY_PAGEUP));
        assert_eq!(special_key_code(9), Some(KeyCode::KEY_PAGEDOWN));
        assert_eq!(special_key_code(10), Some(KeyCode::KEY_HOME));
        assert_eq!(special_key_code(11), Some(KeyCode::KEY_END));
        assert_eq!(special_key_code(12), Some(KeyCode::KEY_ENTER));
        assert_eq!(special_key_code(13), Some(KeyCode::KEY_DELETE));
        assert_eq!(special_key_code(14), Some(KeyCode::KEY_ESC));
        assert_eq!(special_key_code(15), Some(KeyCode::KEY_SYSRQ));
        assert_eq!(special_key_code(16), Some(KeyCode::KEY_SCROLLLOCK));
        assert_eq!(special_key_code(17), Some(KeyCode::KEY_LEFTCTRL));
        assert_eq!(special_key_code(18), Some(KeyCode::KEY_LEFTALT));
        assert_eq!(special_key_code(19), Some(KeyCode::KEY_LEFTSHIFT));
        assert_eq!(special_key_code(20), Some(KeyCode::KEY_LEFTMETA));
        assert_eq!(special_key_code(21), Some(KeyCode::KEY_F1));
        assert_eq!(special_key_code(22), Some(KeyCode::KEY_F2));
        assert_eq!(special_key_code(23), Some(KeyCode::KEY_F3));
        assert_eq!(special_key_code(24), Some(KeyCode::KEY_F4));
        assert_eq!(special_key_code(25), Some(KeyCode::KEY_F5));
        assert_eq!(special_key_code(26), Some(KeyCode::KEY_F6));
        assert_eq!(special_key_code(27), Some(KeyCode::KEY_F7));
        assert_eq!(special_key_code(28), Some(KeyCode::KEY_F8));
        assert_eq!(special_key_code(29), Some(KeyCode::KEY_F9));
        assert_eq!(special_key_code(30), Some(KeyCode::KEY_F10));
        assert_eq!(special_key_code(31), Some(KeyCode::KEY_F11));
        assert_eq!(special_key_code(32), Some(KeyCode::KEY_F12));
    }

    #[tokio::test]
    async fn test_special_key_code_ignores_unmapped_codes() {
        // 0 and anything above 32 fall outside upstream's validity guard
        // (specialKey > 0 && specialKey < 33 — x11remoteinput.cpp:106).
        // 17-20 ARE mapped; see
        // test_special_key_codes_17_to_20_are_modifier_keys.
        for code in [0, 33, -1, 255] {
            assert_eq!(special_key_code(code), None, "code {code} must be unmapped");
        }
        // Unmapped codes must not crash the handler.
        let plugin = MousepadPlugin::new_without_input();
        let packet = Packet::new(
            "kdeconnect.mousepad.request".to_string(),
            serde_json::json!({ "specialKey": 33 }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
    }

    #[tokio::test]
    async fn test_handle_ack() {
        let plugin = MousepadPlugin::new_without_input();
        let packet = Packet::new(
            "kdeconnect.mousepad.request".to_string(),
            serde_json::json!({
                "isAck": true
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert_eq!(plugin.events_received(), 1);
    }

    #[tokio::test]
    async fn test_events_counter_increments() {
        let plugin = MousepadPlugin::new_without_input();
        for _ in 0..5 {
            let packet = Packet::new(
                "kdeconnect.mousepad.request".to_string(),
                serde_json::json!({ "key": "x" }),
            );
            plugin
                .handle_packet("device1", packet)
                .await
                .expect("Value expected to be present");
        }
        assert_eq!(plugin.events_received(), 5);
    }

    #[tokio::test]
    async fn test_mousepad_request_defaults() {
        let req: MousepadRequest =
            serde_json::from_value(serde_json::json!({})).expect("Value expected to be present");
        assert!(req.key.is_none());
        assert!(req.special_key.is_none());
        assert!(!req.ctrl);
        assert!(!req.alt);
        assert!(!req.shift);
        assert!(!req.super_key);
        assert!(!req.singleclick);
        assert!(!req.doubleclick);
        assert!(!req.middleclick);
        assert!(!req.rightclick);
        assert!(!req.singlehold);
        assert!(!req.singlerelease);
        assert!(!req.scroll);
        assert!(req.x.is_none());
        assert!(req.y.is_none());
        assert!(req.dx.is_none());
        assert!(req.dy.is_none());
        assert!(!req.is_ack);
    }

    #[tokio::test]
    async fn test_parse_key_code_lowercase() {
        assert_eq!(parse_key_code("a"), Some(KeyCode::KEY_A));
        assert_eq!(parse_key_code("z"), Some(KeyCode::KEY_Z));
    }

    #[tokio::test]
    async fn test_parse_key_code_special() {
        assert_eq!(parse_key_code("enter"), Some(KeyCode::KEY_ENTER));
        assert_eq!(parse_key_code("esc"), Some(KeyCode::KEY_ESC));
        assert_eq!(parse_key_code("backspace"), Some(KeyCode::KEY_BACKSPACE));
        assert_eq!(parse_key_code("tab"), Some(KeyCode::KEY_TAB));
        assert_eq!(parse_key_code("space"), Some(KeyCode::KEY_SPACE));
        assert_eq!(parse_key_code("delete"), Some(KeyCode::KEY_DELETE));
        assert_eq!(parse_key_code("left"), Some(KeyCode::KEY_LEFT));
        assert_eq!(parse_key_code("right"), Some(KeyCode::KEY_RIGHT));
        assert_eq!(parse_key_code("up"), Some(KeyCode::KEY_UP));
        assert_eq!(parse_key_code("down"), Some(KeyCode::KEY_DOWN));
    }

    #[tokio::test]
    async fn test_parse_key_code_unknown() {
        assert!(parse_key_code("f1").is_none());
        assert!(parse_key_code("unknown").is_none());
        assert!(parse_key_code("123").is_none());
    }

    fn request_from(body: serde_json::Value) -> MousepadRequest {
        serde_json::from_value(body).expect("fixture must deserialize")
    }

    #[tokio::test]
    async fn test_plan_key_with_independent_modifier_bools() {
        // Real Android shape: KeyListenerView.java:132-163 sets "ctrl",
        // "alt", "shift", "super" as four INDEPENDENT booleans alongside
        // "key". kdeconnect-kde reads them the same way
        // (x11remoteinput.cpp:146-149) and presses each one it finds
        // (:151-158). Ctrl+Shift+A is one packet with two bools set.
        let req = request_from(serde_json::json!({
            "key": "a",
            "ctrl": true,
            "shift": true
        }));
        assert_eq!(
            plan_actions(&req),
            vec![InputAction::KeyPress {
                key: KeyCode::KEY_A,
                mods: Modifiers {
                    ctrl: true,
                    alt: false,
                    shift: true,
                    super_key: false,
                },
            }]
        );
    }

    #[tokio::test]
    async fn test_plan_special_key_with_alt_modifier() {
        // Exact body kdeconnect-android sends for its "close window"
        // button: MousePadPlugin.kt:175-180 sets {"alt": true,
        // "specialKey": <F4>}. F4 is code 24 (KeyListenerView.java:52,
        // x11remoteinput.cpp:52).
        let req = request_from(serde_json::json!({
            "alt": true,
            "specialKey": 24
        }));
        assert_eq!(
            plan_actions(&req),
            vec![InputAction::KeyPress {
                key: KeyCode::KEY_F4,
                mods: Modifiers {
                    ctrl: false,
                    alt: true,
                    shift: false,
                    super_key: false,
                },
            }]
        );
    }

    #[tokio::test]
    async fn test_plan_pointer_movement() {
        // kdeconnect-android MousePadPlugin.kt:77-82 — {"dx": f, "dy": f}.
        // kdeconnect-kde warps the pointer by (int)dx, (int)dy
        // (x11remoteinput.cpp:192-193), so the fractional part truncates.
        let req = request_from(serde_json::json!({ "dx": 10.5, "dy": -3.2 }));
        assert_eq!(
            plan_actions(&req),
            vec![InputAction::Move { dx: 10, dy: -3 }]
        );
    }

    #[tokio::test]
    async fn test_plan_ignores_invented_button_field() {
        // Regression guard. No upstream client has ever sent a "button"
        // field in kdeconnect.mousepad.request: `grep -rn
        // 'QStringLiteral("button")' /tmp/kdeconnect-kde` returns nothing,
        // and kdeconnect-android sends clicks as the named booleans
        // (MousePadPlugin.kt:88-122). A packet carrying it must do nothing.
        let req = request_from(serde_json::json!({ "button": 1 }));
        assert!(plan_actions(&req).is_empty());
    }

    #[tokio::test]
    async fn test_plan_click_booleans_exact_android_shapes() {
        // Exact bodies kdeconnect-android sends, one boolean per packet:
        // MousePadPlugin.kt:88-92 (singleclick), :94-98 (doubleclick),
        // :100-104 (middleclick), :106-110 (rightclick), :112-116
        // (singlehold), :118-122 (singlerelease). Button mapping per
        // kdeconnect-kde waylandremoteinput.cpp:459-477.
        let cases: Vec<(serde_json::Value, Vec<InputAction>)> = vec![
            (
                serde_json::json!({ "singleclick": true }),
                vec![InputAction::Click(KeyCode::BTN_LEFT)],
            ),
            (
                serde_json::json!({ "doubleclick": true }),
                vec![InputAction::DoubleClick(KeyCode::BTN_LEFT)],
            ),
            (
                serde_json::json!({ "middleclick": true }),
                vec![InputAction::Click(KeyCode::BTN_MIDDLE)],
            ),
            (
                serde_json::json!({ "rightclick": true }),
                vec![InputAction::Click(KeyCode::BTN_RIGHT)],
            ),
            (
                serde_json::json!({ "singlehold": true }),
                vec![InputAction::ButtonDown(KeyCode::BTN_LEFT)],
            ),
            (
                serde_json::json!({ "singlerelease": true }),
                vec![InputAction::ButtonUp(KeyCode::BTN_LEFT)],
            ),
        ];
        for (body, expected) in cases {
            let req = request_from(body.clone());
            assert_eq!(plan_actions(&req), expected, "body {body}");
        }
    }

    #[tokio::test]
    async fn test_plan_click_never_also_moves_pointer() {
        // A click packet that also carries dx/dy must click and NOT move:
        // upstream's click/key branch and its move branch are mutually
        // exclusive (x11remoteinput.cpp:113 guard vs the :191 else).
        let req = request_from(serde_json::json!({
            "singleclick": true,
            "dx": 40.0,
            "dy": 40.0
        }));
        assert_eq!(
            plan_actions(&req),
            vec![InputAction::Click(KeyCode::BTN_LEFT)]
        );
    }

    #[tokio::test]
    async fn test_plan_scroll_maps_to_wheel_not_movement() {
        // Exact body kdeconnect-android sends when the user two-finger
        // scrolls: MousePadPlugin.kt:124-130 sets {"scroll": true, "dx",
        // "dy"}, and MousePadActivity.java:576 always passes dx = 0.
        // kdeconnect-kde: dy > 0 is wheel-up, dy < 0 is wheel-down
        // (x11remoteinput.cpp:137-144, buttons 4/5 at :22-23), one notch
        // per packet regardless of magnitude. evdev REL_WHEEL uses the
        // same sign (positive = up).
        let up = request_from(serde_json::json!({
            "scroll": true,
            "dx": 0.0,
            "dy": 12.5
        }));
        assert_eq!(
            plan_actions(&up),
            vec![InputAction::Wheel {
                horizontal: false,
                clicks: 1
            }]
        );

        let down = request_from(serde_json::json!({
            "scroll": true,
            "dx": 0.0,
            "dy": -140.0
        }));
        assert_eq!(
            plan_actions(&down),
            vec![InputAction::Wheel {
                horizontal: false,
                clicks: -1
            }],
            "magnitude must not scale the notch count"
        );

        let flat = request_from(serde_json::json!({
            "scroll": true,
            "dx": 0.0,
            "dy": 0.0
        }));
        assert!(plan_actions(&flat).is_empty(), "dy == 0 must emit nothing");
    }

    #[tokio::test]
    async fn test_plan_horizontal_scroll() {
        // kdeconnect-kde's Wayland backend forwards dx for scroll
        // (waylandremoteinput.cpp:479 -> :377). The Android app never
        // sends a nonzero dx here, so this covers desktop peers.
        let req = request_from(serde_json::json!({
            "scroll": true,
            "dx": -8.0,
            "dy": 0.0
        }));
        assert_eq!(
            plan_actions(&req),
            vec![InputAction::Wheel {
                horizontal: true,
                clicks: -1
            }]
        );
    }

    #[tokio::test]
    async fn test_special_key_codes_17_to_20_are_modifier_keys() {
        // kdeconnect-kde maps these four (x11remoteinput.cpp:45-48:
        // XK_Control_L, XK_Alt_L, XK_Shift_L, XK_Super_L) and its validity
        // guard accepts any code in 1..32 (:106). kdeconnect-android
        // assigns the same four (KeyListenerView.java:45-48).
        assert_eq!(special_key_code(17), Some(KeyCode::KEY_LEFTCTRL));
        assert_eq!(special_key_code(18), Some(KeyCode::KEY_LEFTALT));
        assert_eq!(special_key_code(19), Some(KeyCode::KEY_LEFTSHIFT));
        assert_eq!(special_key_code(20), Some(KeyCode::KEY_LEFTMETA));
    }

    #[tokio::test]
    async fn test_plan_android_tv_home_button() {
        // Exact body the TV/bigscreen Home button sends:
        // MousePadPlugin.kt:162-167 sets {"super": true, "specialKey": 20}
        // (code 20 = META_LEFT, KeyListenerView.java:48). This was dropped
        // entirely while 17-20 were treated as unmapped.
        let req = request_from(serde_json::json!({
            "super": true,
            "specialKey": 20
        }));
        assert_eq!(
            plan_actions(&req),
            vec![InputAction::KeyPress {
                key: KeyCode::KEY_LEFTMETA,
                mods: Modifiers {
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_key: true,
                },
            }]
        );
    }

    #[tokio::test]
    async fn test_absolute_position_packet_reaches_the_backend() {
        // Was test_absolute_position_packet_is_dropped_not_treated_as_movement
        // (vk #1010, Task 1.6, Backend A): absolute positioning is now
        // implemented, so a packet carrying only x/y must produce a
        // position rather than being dropped or mistaken for relative
        // movement. The only upstream producer of x/y is
        // kdeconnect-kde's shareinputdevicesremote plugin
        // (shareinputdevicesremoteplugin.cpp:74), delivered in-process
        // (:75), never over the wire — kdeconnect-android never sends x/y
        // at all — but the wire shape is well-defined regardless.
        let req = request_from(serde_json::json!({ "x": 1920.0, "y": 12.0 }));
        assert!(
            plan_actions(&req).is_empty(),
            "an x/y-only packet must not also plan a relative Move"
        );
        assert_eq!(absolute_position(&req), Some((1920, 12)));
    }

    #[tokio::test]
    async fn test_relative_movement_does_not_produce_absolute_position() {
        // Upstream reaches its absolute branch only when dx/dy are zero or
        // absent (x11remoteinput.cpp:192-197). A normal movement or click
        // packet must not report an absolute position.
        let req = request_from(serde_json::json!({ "dx": 10.5, "dy": -3.2 }));
        assert_eq!(absolute_position(&req), None);

        let click = request_from(serde_json::json!({ "singleclick": true }));
        assert_eq!(absolute_position(&click), None);
    }

    #[tokio::test]
    async fn test_absolute_position_ignores_zero_relative_delta() {
        // dx: 0.0 / dy: 0.0 present-but-zero must not block the absolute
        // arm: upstream's guard is `if (dx || dy)` (C++ truthiness — zero
        // is falsy), not "has the field at all"
        // (x11remoteinput.cpp:191-197). plan_actions already treats an
        // explicit zero delta as no relative motion (see
        // test_plan_scroll_maps_to_wheel_not_movement's sibling coverage
        // of the dx/dy==0 arm), so the same packet's x/y must still reach
        // absolute_position.
        let req = request_from(serde_json::json!({ "dx": 0.0, "dy": 0.0, "x": 500.0, "y": 250.0 }));
        assert!(plan_actions(&req).is_empty());
        assert_eq!(absolute_position(&req), Some((500, 250)));
    }

    #[tokio::test]
    async fn test_scale_abs_coord_rounds_and_clamps() {
        // Pins the fixed-range scaling documented on scale_abs_coord:
        // round-to-nearest, then clamp into [0, ABS_RANGE_MAX].
        assert_eq!(scale_abs_coord(0.0), 0);
        assert_eq!(scale_abs_coord(1920.0), 1920);
        assert_eq!(scale_abs_coord(1920.4), 1920);
        assert_eq!(scale_abs_coord(1920.5), 1921); // round-half-away-from-zero
        assert_eq!(scale_abs_coord(-5.0), 0, "negative coordinates clamp to 0");
        assert_eq!(
            scale_abs_coord(f64::from(ABS_RANGE_MAX) + 100.0),
            ABS_RANGE_MAX,
            "coordinates beyond the fixed range clamp to ABS_RANGE_MAX"
        );
        assert_eq!(scale_abs_coord(f64::MAX), ABS_RANGE_MAX);
        assert_eq!(scale_abs_coord(f64::MIN), 0);
        assert_eq!(
            scale_abs_coord(f64::NAN),
            0,
            "NaN must not panic or escape the clamp"
        );
    }

    #[tokio::test]
    async fn test_on_connected_sends_keyboardstate() {
        // kdeconnect-kde sends this the moment a device connects:
        // MousepadPlugin::connected() builds a
        // kdeconnect.mousepad.keyboardstate packet and sets a "state" bool
        // from hasKeyboardSupport() (plugins/mousepad/mousepadplugin.cpp:63-70).
        // kdeconnect-android reads it into isKeyboardEnabled and greys out
        // its keyboard button when false (MousePadPlugin.kt:26-29).
        //
        // new_without_input() has no uinput device, so it cannot type, so
        // it must report state = false. That is a deliberate divergence:
        // upstream omits the field when it has no backend and Android then
        // defaults to true (MousePadPlugin.kt:27).
        let plugin = MousepadPlugin::new_without_input();
        let packets = plugin.on_connected("device1");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.mousepad.keyboardstate");
        assert_eq!(
            packets[0].body.get("state"),
            Some(&serde_json::Value::Bool(false))
        );
    }
}

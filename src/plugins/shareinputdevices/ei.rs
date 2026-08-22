//! EI transport (M3) — receiver over the portal's ConnectToEIS fd.
//!
//! Single Responsibility: own the EI receiver context, drive the
//! handshake, bind seat capabilities, track the xkb state, and pump
//! reis events into the M1 planners (motion / buttons / scroll /
//! keys). The activation-id/sequence ordering dance from
//! `inputcapturesession.cpp:362-366` / `:394-404` lives in
//! `ActivationGate` and is invoked by the PortalSession signal
//! handler.
//!
//! **Crate choice: reis 0.7.1 with the `tokio` feature.** The
//! decision matrix:
//!
//! - reis is the prior-art Rust binding for libei/libeis (lan-mouse
//!   uses it for the same portal half). The upstream cpp uses raw
//!   libei C FFI; reis wraps that in idiomatic async Rust. Every
//!   event variant the cpp's `handleEiEvent` (:344-449) consumes has
//!   a one-to-one reis `EiEvent` variant (PointerMotion, Button,
//!   ScrollDelta, ScrollDiscrete, KeyboardKey, KeyboardModifiers,
//!   DeviceStartEmulating, SeatAdded, DeviceAdded, Disconnected).
//! - Hand-rolled FFI would replicate reis's surface area (≈30 calls,
//!   full event state machine) in `unsafe extern "C"`. The C
//!   "minimal receiver" surface is a misnomer: the cpp is ~120
//!   lines of stateful dispatch in addition to the FFI declarations.
//!   reis is the minimal binding that already matches.
//! - xkbcommon is the Rust binding for libxkbcommon (the cpp wraps
//!   the same library at `inputcapturesession.cpp:43-89`).
//!
//! **Reactor shape.** `EiReceiver::start` is async; it runs the
//! `handshake_tokio`, spawns an event-pump task that consumes the
//! reis `EiConvertEventStream`, and routes each event through:
//!
//! 1. The activation gate (queue or pass-through).
//! 2. The xkb state (keymap from `DeviceAdded`, modifiers from
//!    `KeyboardModifiers`, keycode from `KeyboardKey`).
//! 3. The M1 planners, producing `serde_json::Value` bodies for the
//!    `kdeconnect.mousepad.request` packet.
//!
//! The output is sent on an mpsc channel the caller owns; the
//! receiver is transport-only — wire-shape decisions live in M1.
//!
//! **M4 integration cost.** The drive future this module returns is
//! still `!Send` (reis's `EiConvertEventStream` is the raw-pointer
//! callback registry; the xkb state, now pump-local, inherits that
//! limitation only because the pump future owns it). The daemon's
//! main is `#[tokio::main]` (multithreaded) at `main.rs:10`, so when
//! M4 wires this receiver into the live daemon it still cannot
//! `tokio::spawn` the pump future. It runs the pump on a dedicated
//! thread that hosts a `current_thread` runtime (or an explicit
//! `LocalSet`) and forwards the resulting `WireBody` mpsc across
//! thread boundaries to the connection manager.
//!
//! `EiReceiver` itself IS `Send + Sync` — the xkb state was moved
//! out of the struct into the pump future (the Arc was `!Send` for
//! no reason once we owned the state locally). The PortalSession's
//! D-Bus signal task can now hold an `Arc<EiReceiver>` on the
//! multithread runtime and call `handle_activated` (which never
//! touches the keymap) without an extra `LocalSet` hop. The compile-
//! time `Arc<EiReceiver>: Send` assertion in the test module below
//! pins this property so a future re-introduction of a `!Send`
//! field fails the build.

use std::collections::VecDeque;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use enumflags2::BitFlags;
use futures_util::StreamExt;
use reis::event::{DeviceCapability, EiEvent};
use reis::tokio::EiConvertEventStream;
use reis::{ei, Error as ReisError};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};
use xkbcommon::xkb;

use crate::plugins::shareinputdevices::{
    plan_button, plan_key, plan_motion, plan_scroll, plan_scroll_discrete, Button, ButtonEdge,
};

/// Linux input-event-codes button numbers. The cpp uses the same
/// constants (`inputcapturesession.cpp:80-91`) — `BTN_LEFT = 0x110`,
/// `BTN_RIGHT = 0x111`, `BTN_MIDDLE = 0x112`. Anything outside the
/// `BTN_MOUSE` task range is not a mouse button the producer cares
/// about and is dropped, matching the cpp's switch structure.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// XKB keycode offset the cpp applies at `inputcapturesession.cpp:429`:
/// `auto xkbKey = ei_event_keyboard_get_key(event) + 8`. EI keycodes
/// are evdev-style; libxkbcommon expects the +8 offset when reading
/// them with `xkb_state_key_get_one_sym`.
const XKB_KEYCODE_OFFSET: u32 = 8;

/// Canonical XKB modifier names. The cpp uses `QXkbCommon::modifiers`
/// (inputcapturesession.cpp:434) which names them the same way:
/// "Shift", "Control", "Mod1" (conventional Alt), "Mod4" (conventional
/// Super/Win).
const MOD_SHIFT: &str = "Shift";
const MOD_CONTROL: &str = "Control";
const MOD_ALT: &str = "Mod1";
const MOD_SUPER: &str = "Mod4";

/// The four booleans the cpp populates from xkbcommon. Mirrors the
/// `QXkbCommon::modifiers(state, sym)` result at
/// `inputcapturesession.cpp:434`. Mirrors the `shift`/`ctrl`/`alt`/
/// `super` fields the M1 `plan_key` planner emits on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
}

impl Modifiers {
    /// Project an xkbcommon `State` into our 4-bool shape. Reads
    /// from `STATE_MODS_EFFECTIVE` (depressed | latched | locked)
    /// to match the cpp's `QXkbCommon::modifiers` call at
    /// inputcapturesession.cpp:434, which derives the four booleans
    /// from the state that `xkb_state_update_mask(depressed, latched,
    /// locked, …)` produces. Reading DEPRESSED alone would drop
    /// sticky (latched) and toggle (locked) modifiers while
    /// `key_get_utf8` already emits the shifted text — producing
    /// wire packets whose text and modifiers disagree.
    #[must_use]
    pub fn from_xkb_state(state: &xkb::State) -> Self {
        Self {
            shift: state.mod_name_is_active(MOD_SHIFT, xkb::STATE_MODS_EFFECTIVE),
            ctrl: state.mod_name_is_active(MOD_CONTROL, xkb::STATE_MODS_EFFECTIVE),
            alt: state.mod_name_is_active(MOD_ALT, xkb::STATE_MODS_EFFECTIVE),
            super_key: state.mod_name_is_active(MOD_SUPER, xkb::STATE_MODS_EFFECTIVE),
        }
    }
}

/// Activation-id queue state. Port of inputcapturesession.cpp's
/// `m_currentEisSequence` / `m_currentActivationId` / `queuedEiEvents`
/// triplet (:362-366, :394-404). This is the load-bearing ordering:
/// input events arriving on the EI fd BEFORE the D-Bus `Activated`
/// signal carries the activation_id are held until that signal
/// arrives, then replayed in arrival order.
///
/// The cpp uses the queue to compensate for a race: EIS may deliver
/// pointer-motion events keyed to its own `start_emulating` sequence
/// before the portal's `Activated` signal reaches the daemon
/// carrying the matching `activation_id`. Without the queue, those
/// early events would be applied at the wrong start position.
#[derive(Debug, Default)]
pub struct ActivationGate {
    /// Updated on every `DeviceStartEmulating` event
    /// (`inputcapturesession.cpp:394-396`).
    eis_sequence: u32,
    /// Updated when the D-Bus `Activated` signal arrives
    /// (`inputcapturesession.cpp:288-301`).
    activation_id: u32,
    /// Events held while `eis_sequence > activation_id`
    /// (`inputcapturesession.cpp:362-366`).
    pending: VecDeque<PendingInput>,
}

/// The reconstructed input events the gate holds. Mirrors the cpp's
/// queue of `ei_event*` — except we hold the planner-ready data,
/// not the raw reis event, because the queue is drained when the
/// D-Bus `Activated` signal arrives and the consumers want to call
/// the planners synchronously at that point.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingInput {
    Motion(f64, f64),
    Button(Button, ButtonEdge),
    ScrollDelta(f64, f64),
    ScrollDiscrete(i32, i32),
    Key {
        text: String,
        special_key: i32,
        mods: Modifiers,
    },
}

impl ActivationGate {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Updated on every `DeviceStartEmulating` event. Mirrors
    /// inputcapturesession.cpp:394-396.
    pub fn note_start_emulating(&mut self, sequence: u32) {
        self.eis_sequence = sequence;
    }

    /// The activation_id has been delivered by the D-Bus `Activated`
    /// signal. Mirrors inputcapturesession.cpp:288-301 — record the
    /// id, drain the queue, and return its contents in arrival order
    /// so the caller can re-apply each event.
    pub fn note_activated(&mut self, activation_id: u32) -> Vec<PendingInput> {
        self.activation_id = activation_id;
        std::mem::take(&mut self.pending).into_iter().collect()
    }

    /// The gate condition from `inputcapturesession.cpp:362` —
    /// `m_currentEisSequence > m_currentActivationId`. When true, an
    /// input event should be queued, not passed through.
    #[must_use]
    pub fn should_queue(&self) -> bool {
        self.eis_sequence > self.activation_id
    }

    pub fn queue(&mut self, event: PendingInput) {
        self.pending.push_back(event);
    }

    /// Test seam: peek at the current activation_id.
    #[cfg(test)]
    #[must_use]
    pub fn activation_id(&self) -> u32 {
        self.activation_id
    }

    /// Test seam: peek at the current eis_sequence.
    #[cfg(test)]
    #[must_use]
    pub fn eis_sequence(&self) -> u32 {
        self.eis_sequence
    }

    /// Test seam: peek at the queue length.
    #[cfg(test)]
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Errors surfaced by the EI transport. `Reis` is reis's own error
/// type — protocol errors (handshake failure, unexpected event) and
/// io errors (socket closed). `Xkb` wraps a keymap parse failure
/// from libxkbcommon.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reis handshake error: {0}")]
    Reis(#[from] ReisError),
    #[error("reis io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("xkb keymap parse failed (size={size}, err={err})")]
    Xkb { size: u32, err: String },
}

/// Wire-packet body the transport emits on its output mpsc. The
/// caller decides how to wrap each into a `kdeconnect.mousepad.request`
/// packet (the M1 planner produces the body; this enum just carries
/// it through the receiver's async boundary).
#[derive(Debug, Clone, PartialEq)]
pub enum WireBody {
    Motion(serde_json::Value),
    Button(serde_json::Value),
    Scroll(serde_json::Value),
    Key(serde_json::Value),
}

impl WireBody {
    #[must_use]
    pub fn into_json(self) -> serde_json::Value {
        match self {
            Self::Motion(v) | Self::Button(v) | Self::Scroll(v) | Self::Key(v) => v,
        }
    }
}

/// The EI receiver. Wraps a reis `ei::Context` over the portal's
/// `ConnectToEIS` fd and pumps events through the M1 planners.
///
/// `EiReceiver` is **`Send + Sync`** — the xkb state is owned by
/// the pump future (not the struct), so the Arc<Self> the
/// PortalSession keeps can be shared with the D-Bus signal task
/// running on the multithread runtime. See the M4 integration
/// cost block at the top of the module for the threading shape
/// this enables.
pub struct EiReceiver {
    context: ei::Context,
    gate: Arc<Mutex<ActivationGate>>,
    /// Sender for wire-body events. The pump task installs a fresh
    /// sender at `start()` time; `handle_activated` reuses it to
    /// replay queued events. `None` between construction and start.
    wire_tx: Mutex<Option<mpsc::UnboundedSender<WireBody>>>,
    /// The handshake name we sent to the EIS peer — used for
    /// diagnostic logs.
    handshake_name: String,
    /// The capabilities we bind on the seat at `SeatAdded` — pinned
    /// for tests that want to assert the binding surface.
    bound_caps: BitFlags<DeviceCapability>,
}

impl EiReceiver {
    /// Build a receiver over the ConnectToEIS fd the portal returned.
    /// Performs the reis `Context::new` synchronously (it sets the
    /// socket to non-blocking) and stores the context for the async
    /// `start()`. Returns an Arc so the PortalSession can hold a
    /// shared reference and call `handle_activated` from its D-Bus
    /// signal task — the receiver IS `Send + Sync`, so the D-Bus
    /// signal task can drive `handle_activated` from a multithread
    /// runtime without an additional `LocalSet`.
    ///
    /// `handshake_name` is the EIS "context name" string; cpp uses
    /// the plugin object name. We pass through `shareinputdevices`
    /// so an EIS debug log shows our origin.
    pub fn new(fd: OwnedFd, handshake_name: &str) -> Result<Arc<Self>, Error> {
        let stream = UnixStream::from(fd);
        let context = ei::Context::new(stream)?;
        let caps = DeviceCapability::Keyboard
            | DeviceCapability::Pointer
            | DeviceCapability::Button
            | DeviceCapability::Scroll;
        Ok(Arc::new(Self {
            context,
            gate: Arc::new(Mutex::new(ActivationGate::new())),
            wire_tx: Mutex::new(None),
            handshake_name: handshake_name.to_string(),
            bound_caps: caps,
        }))
    }

    /// Drive the async handshake and return a drive future for the
    /// event-pump. The drive future is `!Send` (reis's
    /// `EiConvertEventStream` carries a callback registry that is
    /// not thread-safe); any executor that requires `Send` futures
    /// (tokio's `rt-multi-thread`) will reject it. The caller must
    /// either spawn it on a `tokio::task::LocalSet` via
    /// `spawn_local`, drive it on a `current_thread` runtime
    /// (`#[tokio::main(flavor = "current_thread")]`), or `await` it
    /// inline. **Do not** `tokio::spawn` it on a multithreaded
    /// runtime — `tokio::spawn` requires `Send`, and the compile
    /// error is the only signal you get.
    ///
    /// Returns `(wire_rx, disconnect_rx, drive)`:
    ///
    /// - `wire_rx` is the receiver for the output mpsc — the caller
    ///   takes ownership and pumps events to the connection manager.
    /// - `disconnect_rx` is a `watch` receiver that flips to `true`
    ///   when the EI peer disconnects (or the socket errors out).
    /// - `drive` is the pump future. Spawn it on the same `LocalSet`
    ///   as the test / consumer task.
    ///
    /// The caller can also simply `await` it inline if it wants
    /// pump work and consumer work to share the same task.
    pub async fn start(
        self: &Arc<Self>,
    ) -> Result<
        (
            mpsc::UnboundedReceiver<WireBody>,
            tokio::sync::watch::Receiver<bool>,
            impl std::future::Future<Output = ()>,
        ),
        Error,
    > {
        // Clone the context cheaply (Backend is Arc-backed) so the
        // pump task can `flush()` it after sending `bind_capabilities`
        // and any other buffered requests — without us consuming
        // `self.context` here. Without this clone the bind would
        // sit in the write buffer indefinitely: reis's
        // `bind_capabilities` buffers (`reis::event::Connection::bind_capabilities`
        // at event.rs:901-907) and the only post-handshake flushes
        // in the crate are the Ping-response path
        // (event.rs:226-233) and explicit `Context::flush` /
        // `Connection::flush` calls (ei.rs:120-127, event.rs:88-94,
        // request.rs:122, handshake.rs:112/192). The receiver's
        // pump never hits any of those on the bind code path, so
        // against a real EIS (mutter / KWin) the EIS never sees the
        // bind — and per the libei contract it never creates the
        // devices the consumer needs. The fix is one explicit flush
        // in the SeatAdded arm; the clone keeps the pump self-contained.
        let context = self.context.clone();
        let (_conn, stream) = self
            .context
            .handshake_tokio(&self.handshake_name, ei::handshake::ContextType::Receiver)
            .await?;
        let (wire_tx, wire_rx) = mpsc::unbounded_channel();
        let (disconnect_tx, disconnect_rx) = tokio::sync::watch::channel(false);
        // Install the sender so handle_activated can drain into it.
        *self.wire_tx.lock().await = Some(wire_tx.clone());
        let gate = self.gate.clone();
        let bound_caps = self.bound_caps;
        let drive = Self::pump(
            context,
            bound_caps,
            gate,
            None, // xkb_state set when DeviceAdded fires
            stream,
            wire_tx,
            disconnect_tx,
        );
        Ok((wire_rx, disconnect_rx, drive))
    }

    /// The event-pump task. Loops until the stream returns `None`
    /// (EOF), an error, or the EI peer sends the terminal
    /// `Disconnected` protocol event; on exit, signals disconnect.
    ///
    /// `xkb_state` is owned by the pump future (not the struct) so
    /// `EiReceiver` itself is `Send + Sync`. `xkb::State` is a raw
    /// pointer (per reis's pinned `!Send` xkb_state convention);
    /// threading it as a plain `Option` through pump→dispatch keeps
    /// the mutability local without a Mutex, and lets `handle_activated`
    /// — which never touches the keymap — run on a different task
    /// wherever the caller wants.
    async fn pump(
        context: ei::Context,
        bound_caps: BitFlags<DeviceCapability>,
        gate: Arc<Mutex<ActivationGate>>,
        mut xkb_state: Option<xkb::State>,
        mut stream: EiConvertEventStream,
        wire_tx: mpsc::UnboundedSender<WireBody>,
        disconnect_tx: tokio::sync::watch::Sender<bool>,
    ) {
        while let Some(event) = stream.next().await {
            match event {
                Ok(EiEvent::Disconnected(d)) => {
                    warn!(
                        reason = ?d.reason,
                        explanation = ?d.explanation,
                        event = "shareinputdevices_ei_disconnect",
                        "Disconnected from EIS"
                    );
                    let _ = disconnect_tx.send(true);
                    // reis does NOT EOF the stream on `Disconnected`
                    // — its `Connection::disconnected` calls
                    // `shutdown_read` on the EIS side, which leaves
                    // the Receiver's read end open. Without this
                    // break the pump would block forever on the next
                    // `stream.next().await` even though the protocol
                    // is over. The cpp gets away with relying on the
                    // portal closing the socket; we don't.
                    break;
                }
                Ok(ei_event) => {
                    Self::dispatch(
                        &context,
                        ei_event,
                        &bound_caps,
                        &gate,
                        &mut xkb_state,
                        &wire_tx,
                    )
                    .await;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        event = "shareinputdevices_ei_event_error",
                        "EI event stream error; receiver stopping"
                    );
                    break;
                }
            }
        }
        let _ = disconnect_tx.send(true);
        debug!(
            event = "shareinputdevices_ei_stream_closed",
            "EI event stream closed"
        );
    }

    /// One-event dispatch. Mirrors the cpp's `handleEiEvent`
    /// (:344-449) minus the `EI_EVENT_DISCONNECT` arm — that is
    /// handled in `pump` directly so the terminal protocol event
    /// ends the pump on its own (reis does not EOF the stream on
    /// `Disconnected`, see pump's break).
    async fn dispatch(
        context: &ei::Context,
        event: EiEvent,
        bound_caps: &BitFlags<DeviceCapability>,
        gate: &Arc<Mutex<ActivationGate>>,
        xkb_state: &mut Option<xkb::State>,
        wire_tx: &mpsc::UnboundedSender<WireBody>,
    ) {
        match event {
            EiEvent::SeatAdded(seat) => {
                // Mirrors inputcapturesession.cpp:375-378.
                seat.seat.bind_capabilities(*bound_caps);
                // reis buffers `bind_capabilities` into the EI
                // context's write buffer; the bytes don't leave the
                // socket until a flush runs. libei's contract is
                // that a server creates devices only in response to
                // a received seat bind, so without this flush the
                // EIS sits forever, no DeviceAdded arrives, and the
                // consumer hangs silently. `Context::flush` is
                // cheap (writes any pending bytes; no-op when the
                // buffer is empty) and the libei handle is single-
                // threaded per context, so this is the only place
                // in the dispatch path that needs it.
                if let Err(e) = context.flush() {
                    warn!(
                        error = %e,
                        event = "shareinputdevices_ei_bind_flush_failed",
                        "Failed to flush seat bind; EIS may never create devices"
                    );
                }
            }
            EiEvent::SeatRemoved(_) => {
                // Mirrors inputcapturesession.cpp:380-381 (no-op).
            }
            EiEvent::DeviceAdded(device) => {
                // Mirrors inputcapturesession.cpp:382-391 — pull the
                // keymap fd for keyboard devices, parse with xkb,
                // stash the state in the pump-local Option.
                if let Some(keymap) = device.device.keymap() {
                    match build_xkb_state(&keymap.fd, keymap.size) {
                        Ok(state) => {
                            *xkb_state = Some(state);
                            debug!(
                                size = keymap.size,
                                event = "shareinputdevices_ei_keymap_loaded",
                                "Keyboard keymap loaded"
                            );
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                event = "shareinputdevices_ei_keymap_parse_failed",
                                "Failed to parse keymap (and fallback failed); keys will not be decoded"
                            );
                        }
                    }
                }
            }
            EiEvent::DeviceRemoved(_) => {
                // Mirrors inputcapturesession.cpp:392-393 (no-op).
            }
            EiEvent::DevicePaused(_) | EiEvent::DeviceResumed(_) => {
                // Mirrors inputcapturesession.cpp:444-446 (logged).
            }
            EiEvent::DeviceStartEmulating(start) => {
                // Mirrors inputcapturesession.cpp:394-396.
                let mut g = gate.lock().await;
                g.note_start_emulating(start.sequence);
            }
            EiEvent::DeviceStopEmulating(_) => {
                // Mirrors inputcapturesession.cpp:397-398 (no-op).
            }
            EiEvent::Frame(_) => {
                // Mirrors inputcapturesession.cpp:399-400 (no-op).
            }
            EiEvent::PointerMotion(motion) => {
                let dx = f64::from(motion.dx);
                let dy = f64::from(motion.dy);
                let mut g = gate.lock().await;
                if g.should_queue() {
                    g.queue(PendingInput::Motion(dx, dy));
                } else {
                    drop(g);
                    let body = plan_motion(dx, dy);
                    let _ = wire_tx.send(WireBody::Motion(body));
                }
            }
            EiEvent::PointerMotionAbsolute(_) => {
                // Mirrors inputcapturesession.cpp:407-408 (no-op —
                // the producer emits relative motion only).
            }
            EiEvent::Button(button) => {
                let mapped = map_button(
                    button.button,
                    button.state == reis::ei::button::ButtonState::Press,
                );
                if let Some((btn, edge)) = mapped {
                    let mut g = gate.lock().await;
                    if g.should_queue() {
                        g.queue(PendingInput::Button(btn, edge));
                    } else {
                        drop(g);
                        let body = plan_button(btn, edge);
                        if body.is_null() {
                            // plan_button returns Null for BTN_RIGHT
                            // release (cpp :85 `pressed` branch).
                            // Drop silently — same as the cpp's no-emit.
                            return;
                        }
                        let _ = wire_tx.send(WireBody::Button(body));
                    }
                }
            }
            EiEvent::ScrollDelta(delta) => {
                let dx = f64::from(delta.dx);
                let dy = f64::from(delta.dy);
                let mut g = gate.lock().await;
                if g.should_queue() {
                    g.queue(PendingInput::ScrollDelta(dx, dy));
                } else {
                    drop(g);
                    let body = plan_scroll(dx, dy, 0, 0);
                    let _ = wire_tx.send(WireBody::Scroll(body));
                }
            }
            EiEvent::ScrollDiscrete(discrete) => {
                let dx = discrete.discrete_dx;
                let dy = discrete.discrete_dy;
                let mut g = gate.lock().await;
                if g.should_queue() {
                    g.queue(PendingInput::ScrollDiscrete(dx, dy));
                } else {
                    drop(g);
                    let body = plan_scroll_discrete(dx, dy);
                    let _ = wire_tx.send(WireBody::Scroll(body));
                }
            }
            EiEvent::ScrollStop(_) | EiEvent::ScrollCancel(_) => {
                // Mirrors inputcapturesession.cpp:418-421 (no-op).
            }
            EiEvent::KeyboardModifiers(mods) => {
                // Mirrors inputcapturesession.cpp:422-427. Apply the
                // full modifier mask to the xkb state; the next
                // KeyboardKey lookup reads the union.
                if let Some(state) = xkb_state.as_mut() {
                    state.update_mask(mods.depressed, mods.latched, mods.locked, 0, 0, mods.group);
                }
            }
            EiEvent::KeyboardKey(key) => {
                let Some(state) = xkb_state.as_mut() else {
                    // No keymap loaded yet — drop the key. Same
                    // effect as the cpp's switch being unreachable
                    // before DeviceAdded.
                    return;
                };
                let is_press = key.state == reis::ei::keyboard::KeyState::Press;
                let xkb_keycode = key.key + XKB_KEYCODE_OFFSET;
                // Faithful port of the cpp's `Xkb::updateKey` →
                // `xkb_state_update_key` call (inputcapturesession.cpp
                // :430): always update the keymap state, emit only on
                // press (`:431` "trigger on press like remotekeyboard").
                //
                // **Caveat the cpp shares.** xkbcommon's docs flag
                // `xkb_state_update_key` and `xkb_state_update_mask` as
                // NOT to be mixed on the same state — each call
                // overwrites the modifiers the other had previously
                // fed in. We mix them anyway because the cpp does:
                // the cpp's `Xkb` exposes both `updateModifiers` and
                // `updateKey` on the same `xkb_state`, and the
                // `handleEiEvent` driver calls `updateModifiers` first
                // (:423-426) then `updateKey` (:430) on every key
                // event. So our modifier bookkeeping is consistent
                // with what the cpp does, and consistent with what
                // xkbcommon warns against. A future cleanup would
                // either keep both modes in lock-step or replace
                // `update_key` with the `mods | keycode` form of
                // `update_mask`. Same tension in both ports, same
                // trade-off; not a divergence to fix here.
                state.update_key(
                    xkb::Keycode::new(xkb_keycode),
                    if is_press {
                        xkb::KeyDirection::Down
                    } else {
                        xkb::KeyDirection::Up
                    },
                );
                if !is_press {
                    return;
                }
                let keysym = state.key_get_one_sym(xkb::Keycode::new(xkb_keycode));
                // Deliberate divergence from the cpp: the cpp emits
                // `key(Key_unknown, mods, "")` for unmapped keys (the
                // Qt::Key 0 placeholder), but with `specialKey`
                // hardcoded 0 here a faithful `Key_unknown` is not
                // representable — emitting a contentless packet would
                // confuse the consumer. Treat NoSymbol like the
                // no-keymap path: warn + drop. M4's keysym→Qt::Key
                // table restores cpp parity by mapping NoSymbol to
                // the literal Key_unknown code.
                if keysym.raw() == 0 {
                    warn!(
                        keycode = xkb_keycode,
                        event = "shareinputdevices_ei_unmapped_keycode",
                        "Keycode has no keysym in the current keymap; dropping event"
                    );
                    return;
                }
                let text = keysym_to_text(keysym);
                // M4-deferred suppression: until the keysym→Qt::Key
                // table restores cpp parity (each named key carries
                // its specialKey code), an empty text + specialKey 0
                // body has nothing for either phone consumer to act
                // on — Escape/Backspace/Tab reach the wire as
                // `{key: "", specialKey: 0, mods…}` and so does every
                // cursor/function key whose UTF-8 representation is
                // empty. Drop them at the source, mirroring the
                // NoSymbol branch above; M4's table is purely
                // additive.
                if text.is_empty() {
                    return;
                }
                let m = Modifiers::from_xkb_state(state);
                // The cpp's :435 derives a Qt::Key + int code via
                // `QXkbCommon::keysymToQtKey(sym, modifiers)`; M1's
                // `plan_key` takes the int directly. The mapping
                // belongs on the consumer side (or a follow-up that
                // adds a keysym→int table here). Until then, the
                // transport emits the text + modifiers and a 0
                // special_key — the planner still produces the right
                // shape, and the consumer side of the wire (the
                // phone) treats `specialKey: 0` as "no special key,
                // use text".
                let mut g = gate.lock().await;
                if g.should_queue() {
                    g.queue(PendingInput::Key {
                        text,
                        special_key: 0,
                        mods: m,
                    });
                } else {
                    drop(g);
                    let body = plan_key(&text, 0, m.shift, m.ctrl, m.alt, m.super_key);
                    let _ = wire_tx.send(WireBody::Key(body));
                }
            }
            EiEvent::TouchDown(_)
            | EiEvent::TouchMotion(_)
            | EiEvent::TouchUp(_)
            | EiEvent::TouchCancel(_) => {
                // Mirrors inputcapturesession.cpp:441-443 (no-op —
                // the producer does not advertise Touch capability).
            }
            _ => {
                // Forward-compat: ignore unknown events.
            }
        }
    }

    /// Receive a D-Bus `Activated` activation_id from the
    /// `PortalSession` signal handler. Updates the gate and replays
    /// any queued events in arrival order. Mirrors
    /// `inputcapturesession.cpp:288-301`.
    pub async fn handle_activated(&self, activation_id: u32) {
        let pending = {
            let mut g = self.gate.lock().await;
            g.note_activated(activation_id)
        };
        let wire_tx_guard = self.wire_tx.lock().await;
        let Some(wire_tx) = wire_tx_guard.as_ref() else {
            debug!(
                activation_id,
                event = "shareinputdevices_ei_activated_no_pump",
                "D-Bus Activated arrived before start(); events dropped"
            );
            return;
        };
        for event in pending {
            let body = match event {
                PendingInput::Motion(dx, dy) => WireBody::Motion(plan_motion(dx, dy)),
                PendingInput::Button(b, e) => {
                    // Mirror the live-path Null-body drop
                    // (mirrors the Button arm of dispatch's
                    // `plan_button.is_null()` drop): the live path's
                    // gate-queuing path dropped the BTN_RIGHT release
                    // before queueing, but since the gate refactor
                    // queues raw and rebuilds at drain, a Null body
                    // can appear here. Skip it so the wire stays
                    // clean.
                    let body = plan_button(b, e);
                    if body.is_null() {
                        continue;
                    }
                    WireBody::Button(body)
                }
                PendingInput::ScrollDelta(dx, dy) => WireBody::Scroll(plan_scroll(dx, dy, 0, 0)),
                PendingInput::ScrollDiscrete(dx, dy) => {
                    WireBody::Scroll(plan_scroll_discrete(dx, dy))
                }
                PendingInput::Key {
                    text,
                    special_key,
                    mods,
                } => {
                    // M4-deferred suppression mirror of the live
                    // dispatch path — a queued key with empty text
                    // and specialKey 0 has nothing for the consumer
                    // to type. The live path drops before queueing
                    // today, so this branch only fires if a caller
                    // hand-queues; keep the safety net so the
                    // replay cannot leak a contentless body to the
                    // wire.
                    if text.is_empty() {
                        continue;
                    }
                    WireBody::Key(plan_key(
                        &text,
                        special_key,
                        mods.shift,
                        mods.ctrl,
                        mods.alt,
                        mods.super_key,
                    ))
                }
            };
            let _ = wire_tx.send(body);
        }
        debug!(
            activation_id,
            event = "shareinputdevices_ei_activated_replayed",
            "D-Bus Activated received; queued events replayed"
        );
    }
}

/// Map a Linux input-event-codes button number + state into our
/// `Button` + `ButtonEdge`. Mirrors the cpp's :80-91 switch — the
/// BTN_MIDDLE quirk (BOTH press and release fire `middleclick`) is
/// replicated by the planner (`plan_button`), not here.
fn map_button(button: u32, is_press: bool) -> Option<(Button, ButtonEdge)> {
    let edge = if is_press {
        ButtonEdge::Press
    } else {
        ButtonEdge::Release
    };
    match button {
        BTN_LEFT => Some((Button::Left, edge)),
        BTN_RIGHT => Some((Button::Right, edge)),
        BTN_MIDDLE => Some((Button::Middle, edge)),
        _ => None,
    }
}

/// Build an xkbcommon `State` from the keymap fd the portal delivered
/// via the EI `Keyboard` interface. The cpp mmaps the fd, parses with
/// `xkb_keymap_new_from_string(KEYMAP_FORMAT_TEXT_V1)`, unmaps.
/// The xkbcommon crate's `new_from_string` takes a `String`; we read
/// the keymap bytes out of the fd into one.
fn build_xkb_state(fd: &OwnedFd, size: u32) -> Result<xkb::State, Error> {
    use std::os::unix::fs::FileExt;
    // Read at offset 0 without mutating the fd's shared seek
    // position. An fd that crossed SCM_RIGHTS shares its open file
    // description with the sender — including the file offset the
    // compositor left behind. A `seek(0)` on our handle would move
    // the compositor's offset too (corrupting any later read it
    // does), and `try_clone` of the OwnedFd only buys us a second
    // reference to the same offset — not independence of the offset
    // itself. The `pread`-via-`read_exact_at` below is the
    // positionless variant: it reads `buf.len()` bytes starting at
    // `offset` and leaves the file's seek position untouched, so the
    // compositor's next read still lands where it expected.
    // `try_clone` here exists only to satisfy `File`'s `From<OwnedFd>`
    // (the reis API hands us `&OwnedFd`, not an owned one); the
    // positionless read is what does the actual safety work.
    let mut buf = vec![0u8; size as usize];
    let file = std::fs::File::from(fd.try_clone().map_err(|e| Error::Xkb {
        size,
        err: format!("keymap fd try_clone: {e}"),
    })?);
    file.read_exact_at(&mut buf, 0).map_err(|e| Error::Xkb {
        size,
        err: format!("keymap fd read: {e}"),
    })?;
    // Load-bearing, not defensive. The Wayland keymap convention
    // compositors reuse for EIS sends `size = strlen + 1` -- the
    // trailing `\0` is part of the byte count. The xkbcommon Rust
    // binding wraps `xkb_keymap_new_from_buffer` (explicit length,
    // NOT NUL-terminated), so a trailing NUL lands inside the
    // keymap text and xkb's parser rejects it with
    // `[XKB-822] Failed to parse input xkb string`. Stripping
    // every trailing `\0` is what makes the production-shape
    // payload work; without it the keymap would never parse and
    // `build_xkb_state` would Err, taking the keymap-less path
    // (see `EiEvent::DeviceAdded` -- the existing fallback that
    // returns Err and currently leaves `xkb_state` None). The
    // strip also tolerates buffers written WITHOUT the NUL (the
    // test fixture's helper), since the loop only fires when the
    // last byte is `\0`.
    while matches!(buf.last(), Some(0)) {
        buf.pop();
    }
    let text = String::from_utf8(buf).map_err(|e| Error::Xkb {
        size,
        err: format!("keymap utf8: {e}"),
    })?;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    // M3 keymap-parse fallback mirrors inputcapturesession.cpp:61-64.
    // The cpp warns + falls back to `xkb_keymap_new_from_names(ctx,
    // nullptr, NO_FLAGS)` (the default RMLVO keymap) when the
    // delivered buffer does not parse; without the fallback the
    // keymap-less path leaves `xkb_state` None and the transport
    // silently drops every key event. The empty strings here are
    // the binding's "use defaults" convention — empty CStrings
    // dereference to `\0`, which libxkbcommon treats identically to
    // NULL for each RMLVO field. Only if the default keymap itself
    // fails do we surface Err to the caller.
    let keymap = match xkb::Keymap::new_from_string(
        &context,
        text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) {
        Some(km) => km,
        None => {
            warn!(
                size,
                event = "shareinputdevices_ei_keymap_parse_failed",
                "Failed to parse keymap; falling back to the default RMLVO keymap"
            );
            xkb::Keymap::new_from_names(
                &context, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .ok_or_else(|| Error::Xkb {
                size,
                err: "default keymap (new_from_names) also failed".into(),
            })?
        }
    };
    Ok(xkb::State::new(&keymap))
}

/// Look up the text representation of a keysym, mirroring
/// `QXkbCommon::lookupStringNoKeysymTransformations(sym)` at
/// `inputcapturesession.cpp:436`. Qt's body is `xkb_keysym_to_utf8`
/// on the BARE keysym; the "NoKeysymTransformations" in the name is
/// the whole point — it deliberately skips the Control and
/// capitalization transformations that `xkb_state_key_get_utf8`
/// applies. Using the state-based lookup here would collapse every
/// Ctrl shortcut: with Control active and unconsumed, xkbcommon
/// turns `c` into `"\x03"`, and the `< 0x20` filter below then
/// erases it, putting `{key: "", ctrl: true}` on the wire — nothing
/// for the consumer to type. Level selection (shift / caps) is
/// already baked into the keysym by `key_get_one_sym`, which is the
/// same split the cpp uses.
///
/// The filter that remains is Qt's, not xkbcommon's:
/// `QKeyEvent::text()` discards the WHOLE string when it contains
/// `char < 0x20` (the `QChar` text codec drops the string rather
/// than passing control bytes through), so Escape / Backspace / Tab
/// reach the wire as empty text. `xkb_keysym_to_utf8` does not
/// filter, so we mirror Qt's all-or-nothing: any byte `< 0x20` in
/// the result drops the entire string. A per-char strip would leave
/// residue if a control byte appeared inside a multi-byte string
/// (e.g. an incomplete UTF-8 sequence under transformation); Qt's
/// codec rejects the whole string in that case.
fn keysym_to_text(keysym: xkb::Keysym) -> String {
    let raw = xkb::keysym_to_utf8(keysym);
    if raw.bytes().any(|b| b < 0x20) {
        String::new()
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn modifiers_default_is_all_false() {
        let m = Modifiers::default();
        assert!(!m.shift);
        assert!(!m.ctrl);
        assert!(!m.alt);
        assert!(!m.super_key);
    }

    #[test]
    fn gate_starts_unarmed() {
        // Initial state mirrors the cpp's
        // `m_currentEisSequence = 0; m_currentActivationId = 0`
        // (inputcapturesession.h:62-63).
        let g = ActivationGate::new();
        assert_eq!(g.eis_sequence(), 0);
        assert_eq!(g.activation_id(), 0);
        assert_eq!(g.pending_len(), 0);
        // 0 > 0 is false — first event passes through (the cpp's
        // guard is the same: with sequence 0 the condition is
        // always false).
        assert!(!g.should_queue());
    }

    #[test]
    fn gate_arms_after_start_emulating() {
        // Mirrors the cpp's START_EMULATING handler:
        // `m_currentEisSequence = ei_event_emulating_get_sequence(event)`.
        let mut g = ActivationGate::new();
        g.note_start_emulating(7);
        assert_eq!(g.eis_sequence(), 7);
        // activation_id still 0; now should_queue is true.
        assert!(g.should_queue());
    }

    #[test]
    fn gate_flushes_on_activated() {
        // Mirrors the cpp's Activated handler: assign activation_id,
        // then drain queuedEiEvents (inputcapturesession.cpp:288-301).
        let mut g = ActivationGate::new();
        g.note_start_emulating(42);
        assert!(g.should_queue());
        g.queue(PendingInput::Motion(1.0, 2.0));
        g.queue(PendingInput::ScrollDiscrete(0, 1));
        assert_eq!(g.pending_len(), 2);
        let flushed = g.note_activated(42);
        assert_eq!(g.activation_id(), 42);
        // After flush, queue is empty and the gate passes through.
        assert_eq!(g.pending_len(), 0);
        assert!(!g.should_queue());
        // Returned in arrival order.
        assert_eq!(
            flushed,
            vec![
                PendingInput::Motion(1.0, 2.0),
                PendingInput::ScrollDiscrete(0, 1),
            ]
        );
    }

    #[test]
    fn gate_flushes_when_activation_id_lags_behind() {
        // Edge case: activation_id < eis_sequence. The cpp's
        // activated handler does NOT gate the drain — it sets
        // m_currentActivationId unconditionally and clears the queue
        // (inputcapturesession.cpp:288-301). The gate condition
        // `m_currentEisSequence > m_currentActivationId` is checked
        // only when ENQUEUEING, not when flushing. Document the
        // divergence from my pre-coding guess: the cpp drains on
        // every Activated signal.
        let mut g = ActivationGate::new();
        g.note_start_emulating(10);
        g.queue(PendingInput::Motion(3.0, 4.0));
        let flushed = g.note_activated(5);
        assert_eq!(flushed.len(), 1);
        assert_eq!(g.pending_len(), 0);
        assert_eq!(g.activation_id(), 5);
    }

    #[test]
    fn gate_flushes_when_activation_id_meets() {
        // The cpp's equality case (activation_id == eis_sequence)
        // drains the queue — `m_currentEisSequence > m_currentActivationId`
        // is false at the enqueue site, so the queue cannot have
        // grown; if it did (start_emulating raced Activated), drain
        // anyway.
        let mut g = ActivationGate::new();
        g.note_start_emulating(100);
        g.queue(PendingInput::Button(Button::Left, ButtonEdge::Press));
        let flushed = g.note_activated(100);
        assert_eq!(flushed.len(), 1);
        assert!(!g.should_queue());
    }

    #[test]
    fn map_button_translates_linux_input_codes() {
        // inputcapturesession.cpp:80-91 — BTN_LEFT press, BTN_RIGHT
        // release, BTN_MIDDLE press. Other buttons are dropped.
        assert_eq!(
            map_button(BTN_LEFT, true),
            Some((Button::Left, ButtonEdge::Press))
        );
        assert_eq!(
            map_button(BTN_LEFT, false),
            Some((Button::Left, ButtonEdge::Release))
        );
        assert_eq!(
            map_button(BTN_RIGHT, true),
            Some((Button::Right, ButtonEdge::Press))
        );
        assert_eq!(
            map_button(BTN_MIDDLE, true),
            Some((Button::Middle, ButtonEdge::Press))
        );
        // Non-mouse buttons are dropped (cpp :87 `else` branch).
        assert_eq!(map_button(0x100, true), None); // BTN_0 — joystick
    }

    #[test]
    fn ei_receiver_is_send() {
        // Compile-time pin for the M4 integration: the PortalSession
        // holds `Arc<EiReceiver>` on the multithread runtime and
        // calls `handle_activated` from a D-Bus signal task. If a
        // future re-introduction of a `!Send` field (e.g. an
        // `xkb::State` back in the struct) makes the assertion fail,
        // the build breaks before any integration test runs. The
        // function is `dead_code` from the runtime's perspective —
        // it exists purely to anchor the bound — hence the allow.
        #[allow(dead_code)]
        fn assert_send<T: Send>() {}
        assert_send::<Arc<EiReceiver>>();
    }

    #[test]
    fn xkb_keycode_offset_is_eight() {
        // inputcapturesession.cpp:429 — `auto xkbKey =
        // ei_event_keyboard_get_key(event) + 8`. Pin the constant
        // explicitly so a reis-version drift that changes the
        // keycode semantics fails loud.
        assert_eq!(XKB_KEYCODE_OFFSET, 8);
    }

    #[test]
    fn wire_body_into_json_passes_through() {
        // WireBody is a transport-level enum; the JSON is whatever
        // the M1 planner produced. Pin the pass-through shape so a
        // future refactor doesn't accidentally wrap the body in an
        // outer object.
        let body = serde_json::json!({"dx": 1.0, "dy": 2.0});
        let v = WireBody::Motion(body.clone()).into_json();
        assert_eq!(v, body);
    }
}

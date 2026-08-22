//! ShareInputDevices plugin — PRODUCER role (M1 + M2 D-Bus half).
//!
//! Single Responsibility: Build the wire packets that a desktop
//! InputCapture session sends to a paired phone — the activation
//! announcement, the relative motion/button/scroll/key event stream, and
//! the parse of the release packet the phone sends back. M1 is the
//! wire-shape layer; M2 is the D-Bus half (portal probe + v1 session
//! lifecycle + Release wiring). The EI transport remains M3.
//!
//! Module layout:
//! - `mod.rs` (this file): the `Plugin` impl, the wire-shape planners
//!   (motion/buttons/scroll/keys/release), the `Edge` enum, and the
//!   pure activation-announcement builder.
//! - `barrier.rs`: the pure barrier-math planner (zone set +
//!   configured edge → one Barrier rectangle). No D-Bus.
//! - `portal.rs`: the `org.freedesktop.portal.InputCapture` zbus
//!   binding, the v1 session lifecycle (CreateSession → ConnectToEIS
//!   → GetZones → SetPointerBarriers → Enable), signal handling,
//!   Release wiring, and the startup probe that gates
//!   `is_backend_available()`.
//! - `ei.rs` (M3): the reis-based receiver that takes ownership of
//!   the ConnectToEIS fd and pumps EI events into the M1 planners,
//!   with the activation-id/sequence queue (:362-366, :394-404 of
//!   inputcapturesession.cpp) ported.
//!
//! Wire shapes (upstream-verified):
//! - **Outgoing `kdeconnect.shareinputdevices.request`**:
//!   `{exitEdge: int, deltax: double, deltay: double}`. Built on every
//!   barrier activation (kdeconnect-kde
//!   plugins/shareinputdevices/shareinputdevicesplugin.cpp:71-75).
//!   `exitEdge` is the configured Qt::Edge cast to int
//!   (shareinputdevicesplugin.cpp:124-127, default Qt::LeftEdge=2;
//!   Qt::Edge numerics: Top=1, Left=2, Right=4, Bottom=8).
//!   `deltax/deltay` is the portal's cursor-position minus the activated
//!   barrier's p1 (inputcapturesession.cpp:295-296).
//! - **Outgoing `kdeconnect.mousepad.request`** stream: the same packet
//!   the phone sends the desktop in the consumer role, reversed
//!   (shareinputdevicesplugin.cpp:76-116). Relative motion: `{dx, dy}`
//!   (:76-79). Buttons (:80-91): BTN_LEFT press→`singlehold`, release
//!   →`singlerelease`; BTN_RIGHT press→`rightclick`; BTN_MIDDLE on BOTH
//!   press and release→`middleclick` (upstream quirk, see
//!   `plan_button`). Scroll (:92-103): smooth delta passes through as
//!   `{scroll: true, dx, dy}` (:95); discrete applies `anglePer120Step =
//!   15/120` AND negates y (:100-101) — upstream asymmetry, see
//!   `plan_scroll`. Keys (:104-116): `{key, specialKey, shift, ctrl, alt,
//!   super}` with the specialKey codes from the Qt::Key→int map at :28-63
//!   (same 1..32 table our receiver already implements, see
//!   `crate::plugins::mousepad::special_key_code`).
//! - **Incoming `kdeconnect.shareinputdevices`** release: `{releaseDeltax,
//!   releaseDeltay}` (kdeconnect-android
//!   .../inputdevicesreceiver/InputDevicesReceiver.kt:60-68). Consumed
//!   upstream at shareinputdevicesplugin.cpp:129-138: barrier.p1() +
//!   releaseDelta becomes the position passed to portal Release. The
//!   producer's release seam here is a stored callback; the portal
//!   release wiring is M2.
//!
//! Capability honesty (M1): incoming `kdeconnect.shareinputdevices`,
//! outgoing `kdeconnect.shareinputdevices.request`. The
//! `kdeconnect.mousepad.request` outgoing capability is already
//! advertised by the remotekeyboard plugin (see
//! src/plugins/remotekeyboard.rs:76-78) and the registry dedups at
//! src/daemon.rs:102-116, so we add only the shareinputdevices-request
//! delta here. **The plugin is NOT registered with the loader in M1**:
//! M2's portal presence probe is the gate, and there is no probe yet.
//! The plugin is constructed by `with_connection_manager`/tests and
//! direct callers, and the capability assertion is exercised in unit
//! tests only — see `tests_shareinputdevices_plugin_*` and the final
//! message that records the gating.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

pub mod barrier;
pub mod ei;
pub mod portal;

/// Qt::Edge numerics, taken verbatim from the Qt 6 header
/// /usr/include/qt6/QtCore/qnamespace.h (verified 2026-08-20 against
/// the host qt6 install). The producer-side field is the raw integer
/// on the wire; the Android consumer's inputdevicesreceiver maps it
/// onto its own INVERTED edge constants (InputDevicesReceiver.kt:83-108,
/// :123-129) at the consumer, not at the producer. See `task-1042-brief`
/// § Wire contract item 1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "i32", from = "i32")]
pub enum Edge {
    Top = 1,
    #[default]
    Left = 2,
    Right = 4,
    Bottom = 8,
}

impl From<Edge> for i32 {
    fn from(e: Edge) -> i32 {
        e as i32
    }
}

impl From<i32> for Edge {
    /// Upstream accepts whatever integer `config()->getInt` returns
    /// (shareinputdevicesplugin.cpp:126) and casts it to Qt::Edge; the
    /// Qt runtime treats any out-of-range value as undefined. We mirror
    /// by accepting any i32 here and treating unknown values as the
    /// default — the gate in M2 will refuse to start a barrier session
    /// with an unrecognized edge.
    fn from(v: i32) -> Self {
        match v {
            1 => Edge::Top,
            2 => Edge::Left,
            4 => Edge::Right,
            8 => Edge::Bottom,
            _ => Edge::default(),
        }
    }
}

/// Capability names a peer must advertise in its `incomingCapabilities`
/// to count as a consumer of our shareinputdevices / mousepad stream.
/// The phone's `InputDevicesReceiver` (Android) advertises both
/// (InputDevicesReceiver.kt:120 `supportedPacketTypes`). We send these
/// packet types to the phone, so they're incoming on its side.
///
/// The matching is OR — a peer with either capability counts. The
/// `shareinputdevices.request` cap proves the phone runs the
/// activation-arm consumer; `mousepad.request` proves it runs the
/// motion-arm consumer (Qt mousepad plugin and remote-keyboard plugin
/// both deliver to it on Android). A phone that has only the latter
/// can still consume the wire stream — the activated event is rare
/// (one per barrier crossing) and the gate's only requirement is "at
/// least one peer can drain this stuff".
pub const CONSUMER_INCOMING_CAPS: &[&str] = &[
    "kdeconnect.shareinputdevices.request",
    "kdeconnect.mousepad.request",
];

/// True iff `device_id` is currently a connected peer AND advertises
/// at least one of `CONSUMER_INCOMING_CAPS` as an incoming capability.
/// The plugin's activation gate runs this against a freshly-Connected
/// peer; the deactivation gate runs an equivalent snapshot across the
/// whole `connections` map.
#[allow(dead_code)] // kept for symmetry with `capable_consumer_ids`; the
                    // gate uses the snapshot form in production.
pub(crate) async fn is_capable_consumer(
    cm: &crate::protocol::ConnectionManager,
    device_id: &str,
) -> bool {
    cm.has_incoming_capability_any(device_id, CONSUMER_INCOMING_CAPS)
        .await
}

/// Snapshot the set of currently-connected, capability-advertising
/// consumers. Used both by the gate's last-capable-leaves transition
/// (count drops to 0 → deactivate) and by the wire consumer's
/// per-packet fan-out filter (capability-filtered fan-out, brief item
/// 4 — send only to peers that can consume what we're sending).
pub(crate) async fn capable_consumer_ids(cm: &crate::protocol::ConnectionManager) -> Vec<String> {
    cm.capable_consumer_ids(CONSUMER_INCOMING_CAPS).await
}

/// Body of a `kdeconnect.shareinputdevices.request` packet.
///
/// Field set and types mirror the cpp producer at
/// shareinputdevicesplugin.cpp:71-75. `exitEdge` is the raw Qt::Edge
/// integer on the wire — see `Edge`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShareInputDevicesRequest {
    pub exit_edge: Edge,
    pub deltax: f64,
    pub deltay: f64,
}

/// Body of a `kdeconnect.shareinputdevices` release packet from the
/// phone, mirroring InputDevicesReceiver.kt:60-68. deltas are integers
/// on the wire (Android's `np.getInt` at :85-86).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShareInputDevicesRelease {
    pub release_deltax: i32,
    pub release_deltay: i32,
}

/// Pure: event→packet-body for the activation announcement.
///
/// Mirrors the cpp producer at shareinputdevicesplugin.cpp:71-75.
/// `exit_edge` is the configured barrier edge; `deltax/deltay` is the
/// portal's cursor-position minus the activated barrier's p1.
pub fn plan_shareinputdevices_request(
    exit_edge: Edge,
    deltax: f64,
    deltay: f64,
) -> ShareInputDevicesRequest {
    ShareInputDevicesRequest {
        exit_edge,
        deltax,
        deltay,
    }
}

/// Pure: input-capture motion → mousepad.request body.
///
/// Mirrors the cpp producer at shareinputdevicesplugin.cpp:76-79.
pub fn plan_motion(dx: f64, dy: f64) -> serde_json::Value {
    serde_json::json!({ "dx": dx, "dy": dy })
}

/// Pure: button event → mousepad.request body.
///
/// Mirrors the cpp producer at shareinputdevicesplugin.cpp:80-91. Two
/// upstream quirks are reproduced exactly:
///
/// 1. BTN_LEFT release goes through as `singlerelease` even though the
///    comment at :82 says that's "not entirely correct" — the upstream
///    author acknowledged the asymmetry and kept it. Mousepad consumers
///    treat `singlerelease` as drag-end, so the release event on its
///    own (no drag prior) is inert except as a forced-up.
/// 2. BTN_MIDDLE on BOTH press and release fires `middleclick` — the
///    cpp's :87-89 branch is `if (button == BTN_MIDDLE)` with no press
///    discriminator. The phone therefore sees a middle-click on every
///    middle click, and another on every release. Replicated exactly;
///    if a phone-side consumer ever breaks on the duplicate we will
///    know to revisit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonEdge {
    Press,
    Release,
}

pub fn plan_button(button: Button, edge: ButtonEdge) -> serde_json::Value {
    match (button, edge) {
        // BTN_LEFT: press→singlehold, release→singlerelease
        // (shareinputdevicesplugin.cpp:83-84).
        (Button::Left, ButtonEdge::Press) => serde_json::json!({ "singlehold": true }),
        (Button::Left, ButtonEdge::Release) => serde_json::json!({ "singlerelease": true }),
        // BTN_RIGHT: press only — release is dropped (the cpp's
        // :85 elif tests `pressed && button == BTN_RIGHT`).
        (Button::Right, ButtonEdge::Press) => serde_json::json!({ "rightclick": true }),
        (Button::Right, ButtonEdge::Release) => serde_json::Value::Null,
        // BTN_MIDDLE: BOTH press and release fire middleclick (:87-89).
        // Upstream quirk — see module doc.
        (Button::Middle, _) => serde_json::json!({ "middleclick": true }),
    }
}

/// Pure: scroll delta → mousepad.request body.
///
/// `discrete_notches` is the portal's integer click count (1 click per
/// packet, like the upstream mousepad consumer's own contract). The
/// smooth `dx/dy` pass through verbatim — see the :94 comment "scroll
/// direction in kdeconnect is inverted" — meaning the wire sign matches
/// what the portal delivers, not what the consumer expects; the
/// consumer flips the sign (MouseReceiverService.java:157-160).
///
/// `discrete` (the high-res wheel path) applies `anglePer120Step =
/// 15/120` AND negates y. The y-negation is asymmetric with the
/// smooth path's passthrough. Upstream records no rationale — likely a
/// bug; replicated exactly for wire compatibility. Will validate
/// against the phone in M4b.
pub fn plan_scroll(
    delta_dx: f64,
    delta_dy: f64,
    discrete_x: i32,
    discrete_y: i32,
) -> serde_json::Value {
    // Discrete path uses `anglePer120Step = 15/120` and negates y
    // (shareinputdevicesplugin.cpp:100-101). The smooth path passes
    // dx/dy through (:95).
    //
    // The two paths are OR'd in the wire: a single packet carries
    // both the smooth delta AND the discrete clicks, because upstream
    // builds the packet once and the consumer ignores the one it
    // doesn't recognize (Android's mousepad consumer does not
    // distinguish — MouseReceiverPlugin.kt:51-121 always reads
    // dx/dy). This is the cleanest mirror of upstream's
    // single-emit-per-source semantics.
    const ANGLE_PER_120_STEP: f64 = 15.0 / 120.0;
    serde_json::json!({
        "scroll": true,
        "dx": delta_dx + f64::from(discrete_x) * ANGLE_PER_120_STEP,
        "dy": delta_dy + f64::from(-discrete_y) * ANGLE_PER_120_STEP,
    })
}

/// Pure: scroll-discrete-only with the upstream asymmetry pinned to
/// the test (discrete path negates y but not x).
///
/// The combined path above is the producer-side wire shape
/// (shareinputdevicesplugin.cpp:95/101 both emit a packet with the
/// same keys, depending on which signal the EI receiver delivered).
/// This discrete-only helper is what the discrete branch emits
/// verbatim from the lambda at :98-103, useful for tests that pin
/// the y-negation independently.
pub fn plan_scroll_discrete(discrete_x: i32, discrete_y: i32) -> serde_json::Value {
    const ANGLE_PER_120_STEP: f64 = 15.0 / 120.0;
    serde_json::json!({
        "scroll": true,
        "dx": f64::from(discrete_x) * ANGLE_PER_120_STEP,
        "dy": f64::from(-discrete_y) * ANGLE_PER_120_STEP,
    })
}

/// Pure: key event → mousepad.request body.
///
/// Mirrors the cpp producer at shareinputdevicesplugin.cpp:104-116.
/// `text` is the xkb keysym text (may be empty), `special_key` is the
/// 1..32 integer from the Qt::Key→code map at :28-63 (0 when unmapped),
/// and the four modifiers are the four INDEPENDENT booleans Android
/// also sends (KeyListenerView.java:132-163).
pub fn plan_key(
    text: &str,
    special_key: i32,
    shift: bool,
    ctrl: bool,
    alt: bool,
    super_key: bool,
) -> serde_json::Value {
    serde_json::json!({
        "key": text,
        "specialKey": special_key,
        "shift": shift,
        "ctrl": ctrl,
        "alt": alt,
        "super": super_key,
    })
}

/// Release-seam callback type. The M1 seam stores the most recent
/// release delta; the M2 portal half turns it into a `Release()`
/// D-Bus call. The signature is `f(release_deltax, release_deltay)`
/// so the callback can be a free function or a closure that captures
/// the portal session.
pub type ReleaseCallback = Arc<dyn Fn(i32, i32) + Send + Sync>;

pub struct ShareInputDevicesPlugin {
    edge: Edge,
    /// Optional connection manager for the M2 send path. M1 holds it
    /// but never calls it; the optional is here so M2 wires send
    /// without changing the struct.
    connection_manager: Option<Arc<crate::protocol::ConnectionManager>>,
    /// Optional device-event broadcaster (Task #1042 fix lane B). The
    /// capability-gate task subscribes once and reacts to
    /// `StateChanged(Connected)` / `StateChanged(Disconnected)` to
    /// activate on the first capable peer and deactivate on the
    /// last one leaving. Wired by the loader at construction; tests
    /// that need the gate wire it via `with_event_broadcaster`.
    broadcaster: Option<Arc<crate::device::EventBroadcaster>>,
    /// One-shot guard so `spawn_capability_gate` runs exactly once
    /// per plugin instance. The gate is idempotent (it just spawns
    /// a task), but a re-spawn would leak subscribers on every
    /// reconnect-driven reset path.
    gate_spawned: Arc<AtomicBool>,
    /// The most recent release delta observed, if any. M1 stores it
    /// (handlers may inspect it in tests/observability); M2 turns it
    /// into a portal Release() call.
    last_release: Arc<Mutex<Option<ShareInputDevicesRelease>>>,
    /// Optional release-seam callback. M2 wires this to the portal; M1
    /// stores it for tests and lets unit tests swap a recording
    /// callback.
    release_callback: Arc<Mutex<Option<ReleaseCallback>>>,
    /// The live M2 portal session, if the probe gate passed and
    /// `enable_session_backend` ran. The release callback closes
    /// over this and calls `release()`. `None` until bootstrap
    /// succeeds (or for the whole process lifetime when the probe
    /// fails).
    portal_session: Arc<Mutex<Option<Arc<portal::PortalSession>>>>,
    /// The session-bus connection the probe ran on, stashed for
    /// `activate_portal_session` (M3's entry point) so the EI
    /// transport attaches on the SAME connection the probe used.
    portal_conn: Arc<Mutex<Option<zbus::Connection>>>,
    /// Probe result — `true` after the portal probe gate has
    /// confirmed the InputCapture interface is present and
    /// capable. The Plugin trait's `is_backend_available()` reads
    /// this so capability advertisement is gated by reality, not
    /// just the existence of a build artefact.
    backend_available: Arc<AtomicBool>,
    /// Set TRUE for the duration of `do_activate`. Used to
    /// serialise the eager re-eval task against the broadcast
    /// subscription: both can race to drive v1 if the boot
    /// happens to subscribe BEFORE the first peer connect (the
    /// subscription sees the event; the eager re-eval runs
    /// concurrently because both are spawned without
    /// coordination). Without this flag the two arms would
    /// interleave CreateSession/ConnectToEIS calls on the same
    /// bus connection — the portal only allows one session per
    /// handle. Reset on completion or failure so the next
    /// disconnect→reconnect cycle can re-activate.
    activation_in_flight: Arc<AtomicBool>,
}

impl Default for ShareInputDevicesPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl ShareInputDevicesPlugin {
    pub fn new() -> Self {
        Self {
            edge: Edge::default(),
            connection_manager: None,
            broadcaster: None,
            gate_spawned: Arc::new(AtomicBool::new(false)),
            last_release: Arc::new(Mutex::new(None)),
            release_callback: Arc::new(Mutex::new(None)),
            portal_session: Arc::new(Mutex::new(None)),
            portal_conn: Arc::new(Mutex::new(None)),
            backend_available: Arc::new(AtomicBool::new(false)),
            activation_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Replace the configured edge. Mirrors the `with_execution_timeout`
    /// pattern from `crate::plugins::runcommand::RuncommandPlugin`
    /// (runcommand.rs:97-100). Settings plumbing is a later decision —
    /// see task-1042-brief § Repo reuse points; this seam is the
    /// caller-side hookup point.
    pub fn with_edge(mut self, edge: Edge) -> Self {
        self.edge = edge;
        self
    }

    /// Wire the connection manager. Mirrors the same pattern in
    /// clipboard.rs:736-741 and share.rs:218-224. M1 holds it but
    /// never calls it.
    pub fn with_connection_manager(mut self, cm: Arc<crate::protocol::ConnectionManager>) -> Self {
        self.connection_manager = Some(cm);
        self
    }

    /// Wire the device-event broadcaster used by the capability gate
    /// (Task #1042 fix lane B). The gate does not start running until
    /// `enable_session_backend` has run a passed probe AND the
    /// plugin has at least one wired `connection_manager` — the
    /// gate's activation side needs both. Tests that exercise the
    /// gate directly wire this; production wires it in the loader.
    pub fn with_event_broadcaster(
        mut self,
        broadcaster: Arc<crate::device::EventBroadcaster>,
    ) -> Self {
        self.broadcaster = Some(broadcaster);
        self
    }

    /// Wire the release-seam callback. M2 wires this to the portal
    /// `Release()` D-Bus call; M1 stores it so unit tests can swap a
    /// recording callback and observe the parsed delta.
    pub fn with_release_callback(self, cb: ReleaseCallback) -> Self {
        *self
            .release_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cb);
        self
    }

    /// Read the last release delta the plugin observed, if any.
    /// Surfaces the release seam for testing and observability.
    pub fn last_release(&self) -> Option<ShareInputDevicesRelease> {
        self.last_release
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Read the configured edge. Mirrors `configuredEdge()` at
    /// shareinputdevicesplugin.cpp:124-127.
    pub fn edge(&self) -> Edge {
        self.edge
    }

    /// Test/inspection seam: whether the M2 portal backend is wired.
    /// Mirrors clipboard.rs / mpris / screensaver_inhibit pattern —
    /// `false` until `enable_session_backend()` has run AND the
    /// portal probe passed.
    pub fn portal_backend_available(&self) -> bool {
        self.backend_available.load(Ordering::SeqCst)
    }

    /// Test/inspection seam: whether the cross-armed
    /// `activation_in_flight` guard is clear (no activation
    /// currently in progress). The activation path's RAII guard
    /// clears this on every early-return / error / completion
    /// exit; a stuck-TRUE reading after a long timeout window
    /// means the activation hung without the boot-path timeout
    /// firing. The integration test
    /// `activation_times_out_on_silent_eis_peer` reads this to
    /// distinguish "timeout fired" from "activation wedged".
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn activation_in_flight_is_clear(&self) -> bool {
        !self.activation_in_flight.load(Ordering::SeqCst)
    }

    /// Inject a pre-built `PortalSession` for integration tests.
    /// Production wiring goes through `enable_session_backend()` —
    /// which connects to the session bus, probes the portal, and
    /// starts the session in one shot. The test bypass is needed
    /// because a fake-portal session is built against a
    /// test-supplied `Connection`, not the live session bus.
    #[cfg(test)]
    pub fn with_portal_session(self, session: Arc<portal::PortalSession>) -> Self {
        *self
            .portal_session
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
        self.backend_available.store(true, Ordering::SeqCst);
        // Wire release callback to the injected session.
        let session_for_cb = session.clone();
        let cb: ReleaseCallback = Arc::new(move |deltax, deltay| {
            let s = session_for_cb.clone();
            tokio::spawn(async move {
                if let Err(e) = s.release(deltax, deltay).await {
                    warn!(
                        error = %e,
                        event = "shareinputdevices_release_dbus_failed",
                        "Portal Release() failed"
                    );
                }
            });
        });
        *self
            .release_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cb);
        self
    }

    /// Production entry point. Called ONLY from
    /// `bootstrap.rs::create_state` (mirrors
    /// screensaver_inhibit.rs::enable_session_backend /
    /// pausemusic.rs / mpris). The flow:
    ///
    /// 1. Connect to the session D-Bus.
    /// 2. Probe `org.freedesktop.portal.InputCapture` (interface
    ///    present + `SupportedCapabilities` has keyboard|pointer +
    ///    `version` >= 1).
    /// 3. If probe fails, log loudly and leave the plugin inert —
    ///    `is_backend_available()` returns false, capability
    ///    advertisement gates off, no session is started.
    /// 4. If probe passes, stash the connection. **Do NOT
    ///    activate** — Task #1042 fix lane B (panel M4 round 1)
    ///    decoupled activation from the probe: arming the barrier
    ///    on a peerless desktop would trap the cursor with no
    ///    consumer (M2-era panel decision 0e230438/573c501e, since
    ///    overwritten, warned about exactly this). The session is
    ///    started lazily, by the capability-gate subscription,
    ///    when the first connected peer advertises the consumer
    ///    capability (`kdeconnect.shareinputdevices.request` or
    ///    `kdeconnect.mousepad.request`).
    /// 5. Spawn the capability-gate task once (idempotent guard
    ///    via `gate_spawned`). The gate's first event is also
    ///    re-evaluated eagerly so a peer that is already
    ///    connected-capable at boot (e.g. an inbound that beat the
    ///    daemon to publishing the state event) is detected.
    ///
    /// A failed activation logs a warn and stays inert (no
    /// `backend_available` flip, no capability advertisement); the
    /// plugin never panics or fails daemon boot on a portal-side
    /// error.
    pub async fn enable_session_backend(&self) {
        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    error = %e,
                    event = "shareinputdevices_session_bus_unavailable",
                    "Cannot connect to session D-Bus; shareinputdevices plugin stays inert"
                );
                return;
            }
        };

        if !portal::probe_portal_available(&conn).await {
            // Probe already logged the reason; just leave backend
            // unavailable and return. Capability advertisement is
            // gated by is_backend_available(), which still returns
            // false because we never set backend_available.
            return;
        }

        *self.portal_conn.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn);

        // Spawn the capability gate once. The gate's lifetime is
        // tied to the plugin instance; the spawned task holds
        // self-clones via Arc-shared fields, not the plugin's
        // outer ownership, so it survives until the daemon
        // itself shuts down. See `spawn_capability_gate`.
        self.spawn_capability_gate();
    }

    /// Spawn the peer-connect/disconnect subscription that drives
    /// activation and deactivation. Idempotent — `gate_spawned`
    /// makes a second call a no-op (a re-spawn would leak one
    /// receiver per call).
    ///
    /// **The seam this introduces.** The brief's landmark list
    /// named `record_peer_capabilities` / `incoming_caps` /
    /// `device_connected`/`device_disconnected` events. The codebase
    /// has the per-peer capability map
    /// (`ConnectionManager::peer_capabilities`) and a per-device
    /// lifecycle broadcaster that fires `DeviceEvent::StateChanged`
    /// on every transition (`src/device/lifecycle.rs:77`). That
    /// pair — read peer caps from the CM, read live state from
    /// the broadcaster — is the minimal seam. No new
    /// registration/registry callback was needed.
    ///
    /// **Ordering.** `record_peer_capabilities` is called BEFORE
    /// `ensure_and_transition(Connected)` on both the inbound and
    /// outbound paths (`inbound.rs:175`, `outbound.rs:321`,
    /// `listener.rs:227`), so by the time the gate sees a
    /// `StateChanged{new_state=Connected}` event, the peer's
    /// capability map entry is live. The gate can do
    /// `is_capable_consumer(cm, device_id)` against the same CM
    /// snapshot without a separate retry path.
    ///
    /// **One peer → activate, last peer out → deactivate.** On a
    /// `Connected` transition the gate checks the peer; if capable
    /// AND no session is live, it kicks `activate_portal_session`.
    /// On a `Disconnected` (or any non-Connected) transition the
    /// gate re-snapshots the consumer set; if it's empty AND a
    /// session is live, it calls `deactivate_portal_session`.
    /// Both calls are idempotent (the session slot and the
    /// `backend_available` flag form the re-entry guards).
    pub fn spawn_capability_gate(&self) {
        // Idempotent: the gate's only side effect is spawning one
        // tokio task that holds Arc-shared handles; respawning
        // would leak subscribers.
        if self.gate_spawned.swap(true, Ordering::SeqCst) {
            return;
        }

        let Some(broadcaster) = self.broadcaster.clone() else {
            warn!(
                event = "shareinputdevices_gate_no_broadcaster",
                "Capability gate skipped: no EventBroadcaster wired (test-only construction without the gate is fine; production wires it via the loader)"
            );
            return;
        };

        let Some(cm) = self.connection_manager.clone() else {
            warn!(
                event = "shareinputdevices_gate_no_connection_manager",
                "Capability gate skipped: no ConnectionManager wired"
            );
            return;
        };

        // Arc clones of the plugin's mutable fields. Captured by
        // the spawned task; the plugin's outer lifetime is
        // 'static via the daemon's `state`, so the task's
        // references are valid for as long as the daemon runs.
        let backend_available = self.backend_available.clone();
        let portal_session = self.portal_session.clone();
        let portal_conn = self.portal_conn.clone();
        let release_callback = self.release_callback.clone();
        let activation_in_flight = self.activation_in_flight.clone();

        // `self` in the spawn closure is `&ShareInputDevicesPlugin`
        // — every method we need is `&self`. We rebuild an
        // activate/deactivate call site as plain `self.method()`
        // calls via the helper closures below, but the cleanest
        // path is to use the methods through a typed trait
        // object... actually the methods are inherent so a
        // self-clone would work, but plugin construction is
        // locked behind `new()` not `Arc::new()`. The right
        // shape here: capture `self`'s pieces we need into Arcs
        // (already done above), and rebuild the activate call
        // site by reaching into the same `Arc`-wrapped state.
        //
        // For activate/deactivate we call back into the plugin's
        // inherent methods on a clone; the methods themselves are
        // `&self`, so cloning via Arc is the only way. We don't
        // have an `Arc<ShareInputDevicesPlugin>` yet — add a
        // helper that takes `&self` and returns an `Arc` only
        // if a probe has stashed state, OR have activate/
        // deactivate take `Arc<Self>`.
        //
        // Simplest path: split activate/deactivate so each takes
        // a shared bundle of Arc fields rather than `self`. That
        // keeps the gate's spawned task free of plugin-Arc
        // ownership. We do that by adding free-function helpers
        // keyed on the Arc bundle; the plugin methods become thin
        // wrappers. See `do_activate` / `do_deactivate` below.

        // Capture the configured edge — the gate is a free
        // function call site, not `self.method()`, so it can't
        // read `self.edge` at evaluate time. The brief says one
        // plugin instance, one configured edge; capturing it
        // here at gate-spawn time is the right shape (settings
        // changes would need a re-spawn, but that's a future
        // concern).
        let edge = self.edge;

        // Eager re-evaluation on boot: a peer that connected
        // BEFORE the gate subscribed may already have published
        // its StateChanged event (the broadcast channel does not
        // replay missed events to late subscribers). Run the
        // gate's activate arm once against the current
        // capability snapshot so we don't miss a peer that
        // connected during `enable_session_backend`. The check
        // is a no-op if no capable peer is connected; if one
        // is, this races the eventual StateChanged event but
        // `activation_in_flight` + `backend_available` +
        // `portal_session` guard against double activation.
        let cm_for_eager = cm.clone();
        let portal_conn_for_eager = portal_conn.clone();
        let portal_session_for_eager = portal_session.clone();
        let release_callback_for_eager = release_callback.clone();
        let backend_available_for_eager = backend_available.clone();
        let activation_in_flight_for_eager = activation_in_flight.clone();
        tokio::spawn(async move {
            do_evaluate_after_event(
                None,
                &cm_for_eager,
                &portal_conn_for_eager,
                &portal_session_for_eager,
                &release_callback_for_eager,
                &backend_available_for_eager,
                &activation_in_flight_for_eager,
                edge,
            )
            .await;
        });

        let mut rx = broadcaster.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // React to every StateChanged. The
                        // evaluation helper handles both edges
                        // (Connected → activate, non-Connected →
                        // deactivate) and is idempotent.
                        let device_id = match &event {
                            crate::device::types::DeviceEvent::StateChanged {
                                device_id, ..
                            } => Some(device_id.clone()),
                            _ => None,
                        };
                        if device_id.is_none() {
                            continue;
                        }
                        do_evaluate_after_event(
                            device_id.as_deref(),
                            &cm,
                            &portal_conn,
                            &portal_session,
                            &release_callback,
                            &backend_available,
                            &activation_in_flight,
                            edge,
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Slow consumer; missed events. The next
                        // event will be the latest state; the
                        // eager re-evaluation above already
                        // covered the boot case. Log and continue.
                        debug!(
                            event = "shareinputdevices_gate_lagged",
                            "Capability gate lagged on the broadcast channel; skipping missed events"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(
                            event = "shareinputdevices_gate_closed",
                            "Capability gate exiting: broadcaster closed"
                        );
                        return;
                    }
                }
            }
        });
    }

    /// Activate the portal session. The `&self` wrapper is the
    /// entry point callers (tests, the gate's spawned task) reach;
    /// it just delegates to the free function `do_activate` that
    /// captures only the Arc-shared fields. See `do_activate` for
    /// the full contract.
    pub async fn activate_portal_session(&self) {
        do_activate(
            self.connection_manager.as_ref(),
            &self.portal_conn,
            &self.portal_session,
            &self.release_callback,
            &self.backend_available,
            &self.activation_in_flight,
            self.edge,
        )
        .await;
    }

    /// Deactivate the portal session (Task #1042 fix lane B). Called
    /// by the capability gate when the last capable consumer
    /// disconnects. Idempotent: if no session is live, returns
    /// silently. Tears down the session on the bus (Disable +
    /// Session.Close via `PortalSession::close`), drops the release
    /// callback, flips `backend_available=false`, and resets
    /// `portal_session=None` so a future capable peer can
    /// re-activate.
    ///
    /// **Known limitation, documented not solved** (brief item 5):
    /// the capability advertisement pushed at activation has no
    /// retraction API (`src/protocol/connection/mod.rs` has `add`
    /// but no `remove`) — peers connected before deactivation keep
    /// seeing the stale capability until their next capability
    /// sync. A future lane can add `remove_capabilities` to the CM
    /// and call it here; for now, the next re-activation pushes a
    /// refresh (the `add_capabilities` path is dedup-safe), and the
    /// disconnects-then-reconnects cycle that the gate already
    /// handles will eventually clean up.
    pub async fn deactivate_portal_session(&self) {
        do_deactivate(
            &self.portal_session,
            &self.release_callback,
            &self.backend_available,
        )
        .await;
    }
}

/// Run the gate's per-event evaluation. `event_device_id` is the
/// device whose state changed (or `None` for the eager
/// re-evaluation on boot — evaluate against the full consumer
/// snapshot). Idempotent: activate is a no-op if a session is
/// already live; deactivate is a no-op if no session is live.
///
/// **Connected edge.** If `event_device_id` is in the consumer set,
/// activate if not already live. This is the brief's "first capable
/// peer connects" arm.
///
/// **Disconnect edge.** If, AFTER the event, the consumer set is
/// empty AND a session is live, deactivate. We don't reason about
/// the specific device that left — we re-snapshot. The snapshot is
/// cheap (a HashMap key scan) and re-evaluating on every
/// non-Connected transition avoids a "did this device have the
/// cap?" round trip that the brief's `peer_capabilities` map would
/// otherwise need.
#[allow(clippy::too_many_arguments)]
async fn do_evaluate_after_event(
    event_device_id: Option<&str>,
    cm: &Arc<crate::protocol::ConnectionManager>,
    portal_conn: &Arc<std::sync::Mutex<Option<zbus::Connection>>>,
    portal_session: &Arc<std::sync::Mutex<Option<Arc<portal::PortalSession>>>>,
    release_callback: &Arc<std::sync::Mutex<Option<ReleaseCallback>>>,
    backend_available: &Arc<AtomicBool>,
    activation_in_flight: &Arc<AtomicBool>,
    edge: Edge,
) {
    let consumers = capable_consumer_ids(cm).await;
    let session_live = portal_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();

    if !session_live {
        // Activation arm. Skip if no probe passed yet (we'd need
        // a stashed conn for `activate_portal_session` to do
        // anything); the gate normally only runs after
        // `enable_session_backend` stashed one.
        let probe_passed = portal_conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if !probe_passed {
            return;
        }
        // First capable peer — the transition we care about is
        // the connect that took the consumer count from 0 to 1.
        // If event_device_id is None (eager boot eval) OR the
        // device is now in the consumer set, activate. A
        // double-activate is guarded inside `do_activate` (and
        // cross-armed at `activation_in_flight`).
        let should_activate = match event_device_id {
            None => !consumers.is_empty(),
            Some(id) => consumers.iter().any(|c| c == id),
        };
        if should_activate {
            do_activate(
                Some(cm),
                portal_conn,
                portal_session,
                release_callback,
                backend_available,
                activation_in_flight,
                edge,
            )
            .await;
        }
        return;
    }

    // Deactivation arm. A session is live; check whether the
    // consumer set has emptied.
    if consumers.is_empty() {
        do_deactivate(portal_session, release_callback, backend_available).await;
    }
}

/// RAII guard that resets `activation_in_flight` to false on
/// drop. The guard is taken at the top of `do_activate` AFTER
/// the in-flight swap succeeds, so a drop fires whether the
/// function exits via early return, error path, or normal
/// completion. Critical for the post-failure retry path: if
/// `PortalSession::start` errors out, the slot stays empty and
/// `activation_in_flight` must be cleared so a future capable
/// peer can retry activation cleanly.
struct ActivationGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> ActivationGuard<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        Self { flag }
    }
}

impl Drop for ActivationGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Run the v1 session sequence end-to-end on the stashed
/// session-bus connection. Stores the session Arc + release
/// callback + `backend_available=true` on success.
///
/// Re-entry guard: refuses if a session is already live. The gate
/// relies on this — without it, a fast Connect/Disconnect/Connect
/// sequence could fire activate twice.
///
/// Idempotent on failure: any failure path logs and returns inert;
/// the slot stays `None`, `backend_available` stays `false`, and
/// the next call can retry cleanly.
#[allow(clippy::too_many_arguments)]
async fn do_activate(
    cm: Option<&Arc<crate::protocol::ConnectionManager>>,
    portal_conn: &Arc<std::sync::Mutex<Option<zbus::Connection>>>,
    portal_session: &Arc<std::sync::Mutex<Option<Arc<portal::PortalSession>>>>,
    release_callback: &Arc<std::sync::Mutex<Option<ReleaseCallback>>>,
    backend_available: &Arc<AtomicBool>,
    activation_in_flight: &Arc<AtomicBool>,
    edge: Edge,
) {
    // Re-entry guard — two layers. (a) Cross-armed
    // `activation_in_flight`: set TRUE for the duration of the
    // v1 sequence, reset on completion or failure. The eager
    // re-eval task and the broadcast subscription loop can both
    // decide to activate for the same peer-connect window; without
    // this flag the two arms race into a double CreateSession on
    // the same bus connection (the portal refuses — exactly the
    // interleaved-call shape the peer-gated test exposed
    // pre-fix). (b) `portal_session` slot check: when the FIRST
    // call finishes, the slot is populated and any subsequent
    // call bails here. Together: concurrent arms are serialised;
    // post-success re-entries are guarded.
    //
    // The guard MUST be installed BEFORE the swap: an early-bail
    // path that returns without resetting the flag would leave
    // `activation_in_flight` stuck at TRUE, and no future
    // `do_activate` call could ever succeed again (the swap would
    // always observe the stuck TRUE and bail). The guard's Drop
    // runs whether the function exits via early return, error, or
    // normal completion — by binding the guard first, every path
    // is covered.
    let _guard = ActivationGuard::new(activation_in_flight);
    if activation_in_flight.swap(true, Ordering::SeqCst) {
        warn!(
            event = "shareinputdevices_activate_in_flight",
            "activate_portal_session called while a previous activation is still in flight; \
             refusing (single-activation contract)"
        );
        return;
    }

    let already_active = portal_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    if already_active {
        warn!(
            event = "shareinputdevices_activate_reentry",
            "activate_portal_session called while a portal session is already live; \
             refusing (single-activation contract)"
        );
        return;
    }

    let conn = portal_conn
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(conn) = conn else {
        warn!(
            event = "shareinputdevices_activate_without_probe",
            "activate_portal_session called before a successful probe; refusing"
        );
        return;
    };

    // Build a channel for Activated events. The PortalSession
    // owns the sender; we own the receiver and turn each event
    // into a kdeconnect.shareinputdevices.request packet.
    let (activated_tx, mut activated_rx) = mpsc::unbounded_channel::<portal::ActivatedEvent>();
    // The session is `mut` so we can call `take_ei_fd`
    // (`PortalSession::take_ei_fd` takes `&mut self` and an
    // `Arc<PortalSession>` doesn't implement `DerefMut`). We
    // wrap into `Arc` only after the M4 wiring finishes taking
    // the fd and populating the receiver slot.
    let mut session = match portal::PortalSession::start(conn, edge, activated_tx).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                error = %e,
                event = "shareinputdevices_session_start_failed",
                "PortalSession::start failed; shareinputdevices plugin stays inert"
            );
            return;
        }
    };

    // M4: take the ConnectToEIS fd and construct the EI
    // receiver. The fd is the hand-off boundary between the
    // portal half (M2) and the EI half (M3 → M4) — see portal.rs
    // module doc §2. Once the receiver holds the fd, dropping
    // either the session or the receiver closes the EIS stream.
    let ei_fd = session.take_ei_fd();
    // `EiReceiver::new` returns `Arc<Self>` directly — the
    // receiver is `Send + Sync` (compile-pinned in ei.rs) so the
    // Arc can be shared between the signal handler (multithread
    // runtime, calls `handle_activated`) and the dedicated EI
    // pump thread (current-thread runtime, awaits `drive`).
    let receiver = match ei::EiReceiver::new(ei_fd, "shareinputdevices") {
        Ok(r) => r,
        Err(e) => {
            warn!(
                error = %e,
                event = "shareinputdevices_ei_receiver_new_failed",
                "EiReceiver::new failed; shareinputdevices plugin stays inert"
            );
            return;
        }
    };
    // Late-bind one Arc into the signal handler's slot BEFORE
    // the second Arc is moved into the dedicated thread. The
    // signal handler was spawned inside PortalSession::start
    // with the slot empty; populating it now arms the
    // Activated-side drain. Safe across the gap because the
    // gate's `should_queue()` condition holds the line: any EI
    // events that arrived in the window stay queued and replay
    // when the D-Bus Activated signal arrives. The handler
    // clones the Arc out of the slot and releases the
    // std::sync lock before any `.await` — see portal.rs
    // `spawn_signal_handler` Activated arm.
    session.populate_ei_receiver(Arc::clone(&receiver)).await;

    // M4: drive the EI pump on a dedicated thread. The daemon's
    // main runtime is `#[tokio::main]` (multithread), which
    // requires `Send` futures. The drive is `!Send` — verified
    // by the `ei_receiver_is_send` compile-time pin in ei.rs
    // (reis's `EiConvertEventStream` carries a raw-pointer
    // callback registry; the xkb state, now pump-local,
    // inherits that limitation). We can't `tokio::spawn` the
    // drive on the main runtime, and we can't `move` it into a
    // `std::thread::spawn` closure (the closure's `Send` bound
    // would reject the !Send future). The shape the brief
    // mandates is the same one ei.rs' module doc calls out: a
    // dedicated thread hosting a `current_thread` runtime.
    // The thread's runtime calls `receiver.start()` ITSELF,
    // so the drive future is created on the dedicated thread
    // and never crosses a thread boundary. The mpsc receiver +
    // watch receiver (both Send) come back to the main thread
    // via oneshot channels; the `!Send` drive stays where it
    // was born. Verified by the build (`cargo build --lib` +
    // `cargo test`).
    let (wire_rx_tx, wire_rx_rx) =
        tokio::sync::oneshot::channel::<mpsc::UnboundedReceiver<ei::WireBody>>();
    let (disconnect_rx_tx, disconnect_rx_rx) =
        tokio::sync::oneshot::channel::<tokio::sync::watch::Receiver<bool>>();
    let drive_thread = match std::thread::Builder::new()
        .name("shareinputdevices-ei-pump".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    warn!(
                        error = %e,
                        event = "shareinputdevices_ei_pump_runtime_build_failed",
                        "EI pump current-thread runtime build failed"
                    );
                    return;
                }
            };
            // The pump future lives entirely on this thread.
            // `receiver` is `Send + Sync` (compile-pinned in
            // ei.rs), so moving it into the thread is safe; the
            // resulting `drive` future is `!Send` and is
            // awaited here, never crossing the thread boundary.
            rt.block_on(async move {
                let (wire_rx, disconnect_rx, drive) = match receiver.start().await {
                    Ok(parts) => parts,
                    Err(e) => {
                        warn!(
                            error = %e,
                            event = "shareinputdevices_ei_pump_start_failed",
                            "EI pump start failed on dedicated thread"
                        );
                        return;
                    }
                };
                // Ship the wire + disconnect receivers back to
                // the main thread. The channels are oneshot so
                // the senders drop after one send — if the main
                // thread has already given up (recv returned
                // Err), the send is a no-op and the drive
                // continues to run until the EI peer hangs up.
                let _ = wire_rx_tx.send(wire_rx);
                let _ = disconnect_rx_tx.send(disconnect_rx);
                drive.await;
            });
        }) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                error = %e,
                event = "shareinputdevices_ei_pump_thread_spawn_failed",
                "EI pump thread spawn failed; shareinputdevices plugin stays inert"
            );
            return;
        }
    };
    // Detach: dropping the `JoinHandle` does NOT join the
    // thread — the comment used to claim "exits and joins
    // implicitly", which is wrong (`JoinHandle::drop`
    // detaches; nothing waits for the OS thread). The pump
    // drive future ends when the pump's
    // `disconnect_tx.send(true)` lands (EOF / error /
    // Disconnected), at which point the dedicated thread's
    // runtime exits and the OS reclaims the thread. We do
    // not hold the JoinHandle because the consumer task below
    // has no business waiting on the pump and the disconnect
    // watcher has its own signal path.
    drop(drive_thread);
    // Pull the wire + disconnect receivers back from the
    // dedicated thread. `start()` succeeded before we got
    // here, so the sends inside the thread will land; if the
    // dedicated thread's runtime build failed earlier the
    // channels would have closed and these recvs would error,
    // which we treat as the same inert-outcome as start()
    // failure above.
    //
    // Both awaits get a bounded timeout (panel M4 panel round
    // 1 fix — P2 — for the boot-path hang): the dedicated
    // thread runs `receiver.start()` (EIS handshake + pump),
    // and a portal that hands back a valid-but-silent EIS fd
    // can leave the handshake stuck forever. The boot path
    // would otherwise park here, blocking `bootstrap → create
    // _state → Daemon::new` from ever returning, which means
    // listeners, the API, and the watchdog never start. The
    // pump thread keeps running to its own end (the
    // `drop(drive_thread)` detach below); only the boot path
    // stops waiting. The constant is named so the rationale is
    // auditable without grep.
    const BOOT_PATH_PUMP_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
    let mut wire_rx = match tokio::time::timeout(BOOT_PATH_PUMP_DELIVERY_TIMEOUT, wire_rx_rx).await
    {
        Ok(Ok(rx)) => rx,
        Ok(Err(_)) => {
            warn!(
                event = "shareinputdevices_ei_pump_wire_rx_dropped",
                "EI pump did not deliver wire_rx; shareinputdevices plugin stays inert"
            );
            return;
        }
        Err(_) => {
            warn!(
                timeout_secs = BOOT_PATH_PUMP_DELIVERY_TIMEOUT.as_secs(),
                event = "shareinputdevices_ei_pump_wire_rx_timeout",
                "EI pump did not deliver wire_rx within the boot-path timeout; \
                 shareinputdevices plugin stays inert (pump thread continues independently)"
            );
            return;
        }
    };
    let disconnect_rx =
        match tokio::time::timeout(BOOT_PATH_PUMP_DELIVERY_TIMEOUT, disconnect_rx_rx).await {
            Ok(Ok(rx)) => rx,
            Ok(Err(_)) => {
                warn!(
                    event = "shareinputdevices_ei_pump_disconnect_rx_dropped",
                    "EI pump did not deliver disconnect_rx; shareinputdevices plugin stays inert"
                );
                return;
            }
            Err(_) => {
                warn!(
                    timeout_secs = BOOT_PATH_PUMP_DELIVERY_TIMEOUT.as_secs(),
                    event = "shareinputdevices_ei_pump_disconnect_rx_timeout",
                    "EI pump did not deliver disconnect_rx within the boot-path timeout; \
                 shareinputdevices plugin stays inert (pump thread continues independently)"
                );
                return;
            }
        };

    // M4: watch `disconnect_rx` per the cpp oracle. The cpp
    // at inputcapturesession.cpp:372-374 only logs the
    // disconnect — it does NOT close the session (the
    // destructor still holds `m_session` for explicit
    // Session.Close; the portal `Disabled` signal is the
    // session-side teardown trigger, see :281-286). We mirror
    // that: log the disconnect and flip
    // `backend_available=false` so the plugin stops advertising
    // the capability to newly-connecting peers, but we do NOT
    // drop the portal session — `release()` and `close()` keep
    // working.
    //
    // **Ordering (panel M4 panel round 1 fix — P2):** the
    // `backend_available.store(true)` below runs BEFORE this
    // watcher is spawned. The pre-fix order (watcher first,
    // store(true) later) allowed an EOF in the gap to flip
    // `false` and have `store(true)` overwrite it, advertising
    // the capability on a dead transport. With the fix, the
    // capability is only ever advertised after the watcher is
    // ready to retract it; an EOF that races ahead of the
    // watcher's spawn lands in the watch channel, the watcher
    // reads it on first `.changed().await`, and the final
    // value is `false` — never the silent overwrite. The
    // brief's drain-before-first-relayed-body contract is
    // unaffected: nothing the consumer emits depends on the
    // watcher's spawn point.
    let backend_available = backend_available.clone();
    // `disconnect_rx` is not used after this `tokio::spawn`,
    // so we move the watch receiver rather than clone —
    // saves one `watch::Receiver::clone` (cheap but not
    // free) and matches the single-consumer semantic.
    let mut disconnect_rx_watcher = disconnect_rx;

    // Wrap the session in `Arc` now that the M4 wiring is done
    // — `take_ei_fd` and `populate_ei_receiver` both required
    // `&mut` / `&self` access on the bare struct; the rest of
    // the lifecycle (release callback closure, portal_session
    // stash, info! log) only needs a shared handle.
    let session = Arc::new(session);

    // Wire release callback to session.release(). The callback
    // is async-required (D-Bus call) so we spawn a tokio task.
    let session_for_cb = session.clone();
    let cb: ReleaseCallback = Arc::new(move |deltax, deltay| {
        let s = session_for_cb.clone();
        tokio::spawn(async move {
            if let Err(e) = s.release(deltax, deltay).await {
                warn!(
                    error = %e,
                    event = "shareinputdevices_release_dbus_failed",
                    "Portal Release() failed"
                );
            }
        });
    });
    *release_callback.lock().unwrap_or_else(|e| e.into_inner()) = Some(cb);

    *portal_session.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());

    // Store `true` BEFORE spawning the watcher (the fix).
    // Pre-fix, this happened AFTER the watcher was spawned,
    // leaving a window where an EOF could flip `false` and
    // be silently overwritten by `true`.
    backend_available.store(true, Ordering::SeqCst);
    // The daemon's capability collection ran at boot, before this
    // activation — push the delta or the advertisement never goes
    // out (panel b152dcc0). The capability advertisement pushed
    // at activation has no retraction API (brief item 5,
    // `record_peer_capabilities` has add but no remove) — peers
    // connected before deactivation keep seeing the stale
    // capability until their next capability sync. Documented,
    // not solved.
    if let Some(cm) = cm {
        cm.add_capabilities(&[], &["kdeconnect.shareinputdevices.request".to_string()]);
    }
    info!(
        event = "shareinputdevices_backend_enabled",
        session_handle = session.session_handle().as_str(),
        edge = ?edge,
        "shareinputdevices session backend wired"
    );

    tokio::spawn(async move {
        if disconnect_rx_watcher.changed().await.is_ok() {
            warn!(
                event = "shareinputdevices_ei_disconnect_backend_flip",
                "EI transport disconnected; shareinputdevices backend no longer available"
            );
            backend_available.store(false, Ordering::SeqCst);
        }
    });

    // Spawn the unified wire consumer. The connection manager is
    // wired once via `with_connection_manager` at construction;
    // `enable_session_backend` is called from bootstrap AFTER the
    // plugin is built (and after bootstrap may wire the CM), so a
    // late-binding accessor was originally attractive — but the
    // accessor was never actually late-bound (the consumer used
    // whatever was on the plugin at activation time and never
    // re-read). The Mutex is therefore dead weight; capture the
    // `Option<Arc<ConnectionManager>>` clone directly. The brief
    // hygiene pass (panel M4 panel round 1) collapses the
    // `Arc<Mutex<Option<Arc<CM>>>>` to a plain
    // `Option<Arc<CM>>`. We feed BOTH the Activated events
    // (→ `kdeconnect.shareinputdevices.request`) and the EI
    // wire bodies (→ `kdeconnect.mousepad.request`) through the
    // same task. `tokio::select!` with `biased;` guarantees the
    // shareinputdevices.request is processed before any
    // mousepad.request on every select iteration — the
    // ordering the cpp emits (started signal first, queued
    // events after) and the ordering the brief mandates (drain
    // before first relayed body).
    let cm_for_consumer = cm.cloned();
    tokio::spawn(async move {
        let mut activated_closed = false;
        let mut wire_closed = false;
        loop {
            if activated_closed && wire_closed {
                debug!(
                    event = "shareinputdevices_wire_consumer_exit",
                    "Wire consumer task ended (both channels closed)"
                );
                return;
            }
            // **Capability-filtered fan-out (brief item 4).** Both
            // consumer arms relay to the
            // CAPABLE-CONNECTED peer set, not the full
            // `connected_device_ids()` list. Single-peer reality
            // today; this keeps multi-peer honest so we don't
            // double-drive phones that didn't ask for the
            // stream.
            let consumer_peers: Vec<String> = match cm_for_consumer.as_ref() {
                Some(cm) => capable_consumer_ids(cm).await,
                None => Vec::new(),
            };
            tokio::select! {
                biased;
                event = activated_rx.recv(), if !activated_closed => {
                    match event {
                        Some(event) => {
                            let body = plan_shareinputdevices_request(
                                edge,
                                event.deltax,
                                event.deltay,
                            );
                            let packet = Packet::new(
                                "kdeconnect.shareinputdevices.request".to_string(),
                                serde_json::to_value(&body).unwrap_or_else(|e| {
                                    warn!(
                                        error = %e,
                                        event = "shareinputdevices_request_serialize_failed",
                                        "ShareInputDevicesRequest serialize failed"
                                    );
                                    serde_json::json!({})
                                }),
                            );
                            if consumer_peers.is_empty() {
                                debug!(
                                    event = "shareinputdevices_activated_no_peers",
                                    "Activated: no capable consumers connected; request dropped"
                                );
                                continue;
                            }
                            let Some(cm) = cm_for_consumer.as_ref() else {
                                continue;
                            };
                            for device_id in &consumer_peers {
                                if let Err(e) = cm.send_packet(device_id, &packet).await {
                                    warn!(
                                        device_id = %device_id,
                                        error = %e,
                                        event = "shareinputdevices_activated_send_failed",
                                        "Send of shareinputdevices.request failed"
                                    );
                                }
                            }
                        }
                        None => {
                            debug!(
                                event = "shareinputdevices_activated_channel_closed",
                                "Activated channel closed"
                            );
                            activated_closed = true;
                        }
                    }
                }
                body = wire_rx.recv(), if !wire_closed => {
                    match body {
                        Some(wire_body) => {
                            let packet = Packet::new(
                                "kdeconnect.mousepad.request".to_string(),
                                wire_body.into_json(),
                            );
                            if consumer_peers.is_empty() {
                                debug!(
                                    event = "shareinputdevices_wire_no_peers",
                                    "Wire body: no capable consumers connected; packet dropped"
                                );
                                continue;
                            }
                            let Some(cm) = cm_for_consumer.as_ref() else {
                                continue;
                            };
                            for device_id in &consumer_peers {
                                if let Err(e) = cm.send_packet(device_id, &packet).await {
                                    warn!(
                                        device_id = %device_id,
                                        error = %e,
                                        event = "shareinputdevices_wire_send_failed",
                                        "Send of mousepad.request failed"
                                    );
                                }
                            }
                        }
                        None => {
                            debug!(
                                event = "shareinputdevices_wire_channel_closed",
                                "Wire channel closed"
                            );
                            wire_closed = true;
                        }
                    }
                }
            }
        }
    });
}

/// Tear down the live portal session, if any. Idempotent. Mirrors
/// the cpp destructor (inputcapturesession.cpp:116-124): explicit
/// Disable + Session.Close on the bus. We take the session out of
/// the slot first (sets it back to `None`) so a re-entrant
/// deactivate — e.g. the EI-disconnect watcher firing while the
/// gate's deactivate runs — is a no-op.
async fn do_deactivate(
    portal_session: &Arc<std::sync::Mutex<Option<Arc<portal::PortalSession>>>>,
    release_callback: &Arc<std::sync::Mutex<Option<ReleaseCallback>>>,
    backend_available: &Arc<AtomicBool>,
) {
    // Drop the release callback first — it holds an Arc clone of
    // the session, which prevents `Arc::try_unwrap` from succeeding
    // below. The callback's `tokio::spawn` may still be in flight
    // (an outstanding Release call); dropping the closure kills
    // future calls, but a Release already mid-D-Bus-call will
    // complete and log its own failure. This is fine for the
    // "no consumer, drop the barrier" semantics.
    *release_callback.lock().unwrap_or_else(|e| e.into_inner()) = None;

    let session = portal_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(session) = session else {
        return;
    };

    // Flip the backend-available flag BEFORE the D-Bus Close so
    // any concurrent path that observes the slot already-empty
    // sees `backend_available=false` immediately. The gate's
    // disconnect edge is `backend_available` + `portal_session`
    // both empty; this ordering keeps a fast
    // disconnect/connect/disconnect cycle from observing a
    // transient "slot empty but backend_available=true" state.
    backend_available.store(false, Ordering::SeqCst);

    info!(
        event = "shareinputdevices_backend_disabled",
        session_handle = session.session_handle().as_str(),
        "shareinputdevices session backend deactivating (last capable peer left)"
    );

    // Try to consume the Arc directly (the only clone was the
    // release callback we just dropped). If any other clone
    // somehow exists, fall back to the Drop-driven best-effort
    // close by simply dropping — `Drop` for `PortalSession`
    // (portal.rs:905) spawns `close_session_best_effort` on the
    // current runtime. We do not block the deactivate path on
    // the D-Bus call: the gate's next activate is independent.
    match Arc::try_unwrap(session) {
        Ok(session) => {
            if let Err(e) = session.close().await {
                warn!(
                    error = %e,
                    event = "shareinputdevices_close_failed",
                    "Portal session close failed"
                );
            }
        }
        Err(arc_session) => {
            // Other Arc clones exist. The release_callback was
            // the only expected one; if it lingers it's the
            // mid-flight Release() spawn captured before our
            // None-set. Drop the Arc — its Drop spawns
            // best-effort Close on the runtime.
            drop(arc_session);
        }
    }
}

#[async_trait::async_trait]
impl Plugin for ShareInputDevicesPlugin {
    fn name(&self) -> &str {
        "shareinputdevices"
    }

    /// Incoming: the release packet from the phone. Same delta-only
    /// delta as Android's inputdevicesreceiver (`supportedPacketTypes`
    /// at InputDevicesReceiver.kt:120).
    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.shareinputdevices".to_string()]
    }

    /// Outgoing: the activation-announcement request. The
    /// `kdeconnect.mousepad.request` outgoing capability is already
    /// advertised by the remotekeyboard plugin (see
    /// src/plugins/remotekeyboard.rs:76-78) and the registry dedups at
    /// src/daemon.rs:102-116, so we add only the shareinputdevices-
    /// request delta here.
    ///
    /// Gated on the portal probe (systemvolume/mod.rs:519-530
    /// pattern): the daemon's capability aggregation
    /// (src/daemon.rs:102-116) does NOT filter on
    /// `is_backend_available()`, so the honesty contract lives here —
    /// on a portal-less desktop (X11, Sway, Hyprland) this returns
    /// empty and nothing is advertised.
    fn outgoing_capabilities(&self) -> Vec<String> {
        if self.backend_available.load(Ordering::SeqCst) {
            vec!["kdeconnect.shareinputdevices.request".to_string()]
        } else {
            Vec::new()
        }
    }

    /// M2 wires the probe gate from `enable_session_backend()` —
    /// the override returns the live probe state, mirroring
    /// clipboard / mpris / screensaver_inhibit / pausemusic. The
    /// plugin IS loader-registered; probe-failure on a portal-less
    /// desktop keeps the capability un-advertised via the gated
    /// `outgoing_capabilities()` above.
    fn is_backend_available(&self) -> bool {
        self.backend_available.load(Ordering::SeqCst)
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let release: ShareInputDevicesRelease = packet.body_as("shareinputdevices release")?;
        debug!(
            device_id = %device_id,
            release_deltax = release.release_deltax,
            release_deltay = release.release_deltay,
            event = "shareinputdevices_release_received",
            "Received release packet from peer"
        );
        *self.last_release.lock().unwrap_or_else(|e| e.into_inner()) = Some(release.clone());
        let cb = self
            .release_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            cb(release.release_deltax, release.release_deltay);
        } else {
            warn!(
                device_id = %device_id,
                event = "shareinputdevices_release_unwired",
                "Release packet parsed but no M2 portal callback is wired; \
                 delta is stored but not released. Wire with_release_callback \
                 in M2."
            );
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn fixture_path(suffix: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/shareinputdevices")
            .join(suffix)
    }

    fn load_bodies(suffix: &str) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(fixture_path(suffix))
            .unwrap_or_else(|e| panic!("read fixture {suffix}: {e}"));
        let v: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse fixture {suffix}: {e}"));
        match v {
            serde_json::Value::Array(items) => items,
            serde_json::Value::Object(_) => vec![v["body"].clone()],
            _ => panic!("fixture {suffix} is not a JSON object or array"),
        }
    }

    #[test]
    fn edge_default_is_left() {
        // shareinputdevicesplugin.cpp:126 — the in-kde config key
        // `edge` defaults to Qt::LeftEdge.
        assert_eq!(Edge::default(), Edge::Left);
        assert_eq!(i32::from(Edge::default()), 2);
    }

    #[test]
    fn edge_qt_numerics_match_upstream_header() {
        // /usr/include/qt6/QtCore/qnamespace.h: Top=1, Left=2, Right=4,
        // Bottom=8. The wire value is the raw integer; the Android
        // consumer maps it onto its own INVERTED constants at the
        // consumer side (InputDevicesReceiver.kt:125-128).
        assert_eq!(i32::from(Edge::Top), 1);
        assert_eq!(i32::from(Edge::Left), 2);
        assert_eq!(i32::from(Edge::Right), 4);
        assert_eq!(i32::from(Edge::Bottom), 8);
    }

    #[test]
    fn edge_from_unknown_int_falls_back_to_default() {
        // Upstream accepts whatever `config()->getInt` returns and
        // casts to Qt::Edge — Qt treats out-of-range as undefined.
        // We mirror by treating unknown ints as the default
        // (LeftEdge) at the seam; the M2 portal gate refuses to
        // start a session with an unrecognized edge.
        assert_eq!(Edge::from(0), Edge::Left);
        assert_eq!(Edge::from(3), Edge::Left);
        assert_eq!(Edge::from(16), Edge::Left);
    }

    #[test]
    fn edge_serializes_as_wire_integer() {
        // The wire field is the raw integer; serde's `into`/`from`
        // makes the JSON shape identical to the cpp producer.
        let body = serde_json::to_value(Edge::Right).expect("edge→json");
        assert_eq!(body, serde_json::json!(4));
    }

    #[test]
    fn plan_shareinputdevices_request_default_edge_wire_shape() {
        // Fixture: tests/fixtures/upstream-wire/shareinputdevices/
        //   shareinputdevices_request_default_edge.json
        //   kdeconnect-kde (master shallow clone, line locations match; pin f5ed3ed8 in provenance.yaml) shareinputdevicesplugin.cpp:71-75
        let bodies = load_bodies("shareinputdevices_request_default_edge.json");
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        let req: ShareInputDevicesRequest = serde_json::from_value(body.clone())
            .expect("fixture must deserialize into ShareInputDevicesRequest");
        assert_eq!(req.exit_edge, Edge::Left);
        assert!((req.deltax - 12.5).abs() < f64::EPSILON);
        assert!((req.deltay - -3.25).abs() < f64::EPSILON);
    }

    #[test]
    fn plan_shareinputdevices_request_all_qt_edges() {
        // Fixture: tests/fixtures/upstream-wire/shareinputdevices/
        //   shareinputdevices_request_edge_variants.json
        //   kdeconnect-kde (master shallow clone, line locations match; pin f5ed3ed8 in provenance.yaml) shareinputdevicesplugin.cpp:71-75,124-127.
        //   Android inputdevicesreceiver.kt:125-128 has the INVERTED
        //   mapping; the wire value we send is the Qt-edge integer,
        //   not the Android edge.
        let bodies = load_bodies("shareinputdevices_request_edge_variants.json");
        assert_eq!(bodies.len(), 4);
        let expected = [Edge::Top, Edge::Left, Edge::Right, Edge::Bottom];
        for (body, exp) in bodies.iter().zip(expected.iter()) {
            let req: ShareInputDevicesRequest =
                serde_json::from_value(body.clone()).expect("each edge-variant must deserialize");
            assert_eq!(&req.exit_edge, exp);
        }
    }

    #[test]
    fn plan_shareinputdevices_request_builder_returns_exact_shape() {
        // The builder surface is what the M2 portal half calls on
        // activation. Mirrors the cpp lambda at :71-75.
        let req = plan_shareinputdevices_request(Edge::Right, 7.0, -2.0);
        assert_eq!(req.exit_edge, Edge::Right);
        assert_eq!(req.deltax, 7.0);
        assert_eq!(req.deltay, -2.0);
        let body = serde_json::to_value(&req).expect("request→json");
        assert_eq!(
            body,
            serde_json::json!({
                "exitEdge": 4,
                "deltax": 7.0,
                "deltay": -2.0,
            })
        );
    }

    #[test]
    fn plan_mousepad_motion_wire_shape() {
        // Fixture: tests/fixtures/upstream-wire/shareinputdevices/
        //   mousepad_request_motion.json
        //   kdeconnect-kde (master shallow clone, line locations match; pin f5ed3ed8 in provenance.yaml) shareinputdevicesplugin.cpp:76-79.
        let bodies = load_bodies("mousepad_request_motion.json");
        for body in &bodies {
            let dx = body["dx"].as_f64().unwrap();
            let dy = body["dy"].as_f64().unwrap();
            let out = plan_motion(dx, dy);
            assert_eq!(out, body.clone(), "motion body must round-trip");
        }
    }

    #[test]
    fn plan_mousepad_motion_handles_zero_delta() {
        // dx/dy = 0 still produces a packet (the cpp lambda
        // unconditionally builds and sends). The phone consumer
        // drops it.
        let out = plan_motion(0.0, 0.0);
        assert_eq!(out, serde_json::json!({ "dx": 0.0, "dy": 0.0 }));
    }

    #[test]
    fn plan_mousepad_buttons_wire_shape() {
        // Fixture: tests/fixtures/upstream-wire/shareinputdevices/
        //   mousepad_request_buttons.json
        //   kdeconnect-kde (master shallow clone, line locations match; pin f5ed3ed8 in provenance.yaml) shareinputdevicesplugin.cpp:80-91.
        let bodies = load_bodies("mousepad_request_buttons.json");
        let expectations = [
            serde_json::json!({ "singlehold": true }),
            serde_json::json!({ "singlerelease": true }),
            serde_json::json!({ "rightclick": true }),
            serde_json::json!({ "middleclick": true }),
            serde_json::json!({ "middleclick": true }),
        ];
        for (body, expected) in bodies.iter().zip(expectations.iter()) {
            assert_eq!(body, expected, "button body must match upstream");
        }
    }

    #[test]
    fn plan_left_button_press_and_release_match_upstream() {
        // Pin the BTN_LEFT pair: press→singlehold, release→singlerelease
        // (shareinputdevicesplugin.cpp:83-84).
        assert_eq!(
            plan_button(Button::Left, ButtonEdge::Press),
            serde_json::json!({ "singlehold": true })
        );
        assert_eq!(
            plan_button(Button::Left, ButtonEdge::Release),
            serde_json::json!({ "singlerelease": true })
        );
    }

    #[test]
    fn plan_right_button_press_only_release_silently_dropped() {
        // Pin the BTN_RIGHT asymmetry: the cpp's :85 elif tests
        // `pressed && button == BTN_RIGHT`, so release is dropped
        // (no packet). The builder returns Null so the M2 caller
        // can skip the send.
        assert_eq!(
            plan_button(Button::Right, ButtonEdge::Press),
            serde_json::json!({ "rightclick": true })
        );
        assert_eq!(
            plan_button(Button::Right, ButtonEdge::Release),
            serde_json::Value::Null
        );
    }

    #[test]
    fn plan_middle_button_fires_on_both_press_and_release() {
        // UPSTREAM QUIRK, replicated: BTN_MIDDLE on BOTH press and
        // release fires `middleclick` (shareinputdevicesplugin.cpp:87-89
        // — the `if` has no press check). If a phone-side consumer
        // ever breaks on the duplicate we will know to revisit.
        assert_eq!(
            plan_button(Button::Middle, ButtonEdge::Press),
            serde_json::json!({ "middleclick": true })
        );
        assert_eq!(
            plan_button(Button::Middle, ButtonEdge::Release),
            serde_json::json!({ "middleclick": true })
        );
    }

    #[test]
    fn plan_mousepad_scroll_delta_passthrough() {
        // Fixture: tests/fixtures/upstream-wire/shareinputdevices/
        //   mousepad_request_scroll_delta.json
        //   kdeconnect-kde@f5ed3ed8 shareinputdevicesplugin.cpp:92-97
        //   — the smooth path passes dx/dy through verbatim. The :94
        //   comment "scrollDirection in kdeconnect is inverted" is
        //   upstream-recorded; the wire sign matches what the portal
        //   delivers, not what the consumer expects.
        let bodies = load_bodies("mousepad_request_scroll_delta.json");
        for body in &bodies {
            let dx = body["dx"].as_f64().unwrap();
            let dy = body["dy"].as_f64().unwrap();
            // No discrete clicks → only the smooth path contributes.
            let out = plan_scroll(dx, dy, 0, 0);
            assert_eq!(out, body.clone(), "scroll delta must round-trip");
        }
    }

    #[test]
    fn plan_mousepad_scroll_discrete_negates_y() {
        // UPSTREAM ASYMMETRY, replicated: the discrete path applies
        // `anglePer120Step = 15/120` AND negates y
        // (shareinputdevicesplugin.cpp:98-103). The smooth path
        // passes through (no negation). Pin the discrete-only
        // helper explicitly so the test fails loudly if either
        // side of the asymmetry drifts.
        //   tests/fixtures/upstream-wire/shareinputdevices/
        //   mousepad_request_scroll_discrete.json
        let bodies = load_bodies("mousepad_request_scroll_discrete.json");
        // Pairs are (input_x, input_y, expected_wire_dx, expected_wire_dy) —
        // the wire dy is -input_y * anglePer120Step (the upstream negation).
        let expected = [(0, 1, 0.0, -0.125), (0, -1, 0.0, 0.125), (1, 0, 0.125, 0.0)];
        for (body, (dx, dy, exp_dx, exp_dy)) in bodies.iter().zip(expected.iter()) {
            let out = plan_scroll_discrete(*dx, *dy);
            let got = (out["dx"].as_f64().unwrap(), out["dy"].as_f64().unwrap());
            let want = (*exp_dx, *exp_dy);
            assert!(
                (got.0 - want.0).abs() < 1e-9 && (got.1 - want.1).abs() < 1e-9,
                "discrete({dx},{dy}) = {got:?}, want {want:?}, body {body}",
            );
        }
    }

    #[test]
    fn plan_mousepad_scroll_discrete_does_not_negate_x() {
        // Companion to the y-negation: the discrete path's x is
        // signed normally. The asymmetry is the y-only negation —
        // catch a regression that would accidentally flip x too.
        let out = plan_scroll_discrete(4, 0);
        assert!(
            (out["dx"].as_f64().unwrap() - 4.0 * (15.0 / 120.0)).abs() < 1e-9,
            "discrete x must NOT be negated"
        );
    }

    #[test]
    fn plan_mousepad_keys_wire_shape() {
        // Fixture: tests/fixtures/upstream-wire/shareinputdevices/
        //   mousepad_request_keys.json
        //   kdeconnect-kde (master shallow clone, line locations match; pin f5ed3ed8 in provenance.yaml) shareinputdevicesplugin.cpp:104-116.
        //   The body shape is {key, specialKey, shift, ctrl, alt, super}.
        //   SpecialKey codes are the 1..32 set the cpp map at :28-63
        //   and our receiver already implements (mousepad.rs:340-380).
        let bodies = load_bodies("mousepad_request_keys.json");
        let expected = [
            ("a", 0_i32, false, false, false, false),
            ("A", 0_i32, true, false, false, false),
            ("", 12_i32, false, false, false, false),
            ("", 24_i32, false, false, true, false),
            ("c", 0_i32, false, true, false, false),
        ];
        for (body, (key, special, shift, ctrl, alt, sup)) in bodies.iter().zip(expected.iter()) {
            let out = plan_key(key, *special, *shift, *ctrl, *alt, *sup);
            assert_eq!(out, body.clone(), "key body must round-trip");
        }
    }

    #[test]
    fn plan_mousepad_keys_super_modifier_serializes_as_super_key() {
        // The cpp uses the literal key `super` (MetaModifier) at :113.
        // Our receiver mirrors it with `rename = "super"`
        // (remotekeyboard.rs:48). The producer must do the same.
        let out = plan_key("k", 0, false, false, false, true);
        assert_eq!(out["super"], serde_json::Value::Bool(true));
    }

    #[tokio::test]
    async fn test_handle_release_packet_parses_release_delta() {
        // Fixture: tests/fixtures/upstream-wire/shareinputdevices/
        //   shareinputdevices_release.json
        //   kdeconnect-android@e4a5f9a inputdevicesreceiver/InputDevicesReceiver.kt:60-68.
        let raw = std::fs::read_to_string(fixture_path("shareinputdevices_release.json"))
            .expect("read release fixture");
        let body: serde_json::Value = serde_json::from_str::<serde_json::Value>(&raw)
            .expect("parse release fixture")["body"]
            .clone();
        let release: ShareInputDevicesRelease =
            serde_json::from_value(body.clone()).expect("release must deserialize");
        assert_eq!(release.release_deltax, 8);
        assert_eq!(release.release_deltay, -3);
    }

    #[tokio::test]
    async fn test_handle_release_packet_invokes_callback_with_parsed_delta() {
        // The M1 release seam: the parsed delta is stored AND
        // forwarded to the optional callback. M2 wires the callback
        // to the portal Release() D-Bus call. M1 captures the
        // delta so unit tests can assert what would have been
        // released.
        use std::sync::atomic::{AtomicI32, Ordering};
        let observed = Arc::new((AtomicI32::new(0), AtomicI32::new(0)));
        let observed_clone = observed.clone();
        let cb: ReleaseCallback = Arc::new(move |dx, dy| {
            observed_clone.0.store(dx, Ordering::SeqCst);
            observed_clone.1.store(dy, Ordering::SeqCst);
        });

        let plugin = ShareInputDevicesPlugin::new().with_release_callback(cb);
        let packet = Packet::new(
            "kdeconnect.shareinputdevices".to_string(),
            serde_json::json!({
                "releaseDeltax": 12,
                "releaseDeltay": -7,
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("handler must succeed");

        assert_eq!(observed.0.load(Ordering::SeqCst), 12);
        assert_eq!(observed.1.load(Ordering::SeqCst), -7);
        let last = plugin.last_release().expect("release must be stored");
        assert_eq!(last.release_deltax, 12);
        assert_eq!(last.release_deltay, -7);
    }

    #[tokio::test]
    async fn test_handle_release_packet_stores_delta_even_without_callback() {
        // Until M2 wires the callback, the release delta is stored
        // anyway — observability, not discard. The plugin logs a
        // warning at the seam so an unwired build shows up in the
        // dashboard rather than silently dropping releases.
        let plugin = ShareInputDevicesPlugin::new();
        let packet = Packet::new(
            "kdeconnect.shareinputdevices".to_string(),
            serde_json::json!({
                "releaseDeltax": 1,
                "releaseDeltay": 2,
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("handler must succeed");
        let last = plugin.last_release().expect("release must be stored");
        assert_eq!(last.release_deltax, 1);
        assert_eq!(last.release_deltay, 2);
    }

    #[test]
    fn test_capability_advertisement_delta_only() {
        // Incoming: kdeconnect.shareinputdevices — the release
        // packet (Android's inputdevicesreceiver outgoing; cpp is
        // the receiver per shareinputdevicesplugin.cpp:129-138).
        // Outgoing: kdeconnect.shareinputdevices.request — the
        // activation announcement (cpp producer at :71-75; Android
        // consumer at InputDevicesReceiver.kt:120).
        //
        // The kdeconnect.mousepad.request outgoing capability is
        // ALREADY advertised by the remotekeyboard plugin
        // (src/plugins/remotekeyboard.rs:76-78) and the registry
        // dedups at src/daemon.rs:102-116, so the only delta from
        // adding this plugin is the two shareinputdevices types.
        let plugin = ShareInputDevicesPlugin::new();
        assert_eq!(
            plugin.incoming_capabilities(),
            vec!["kdeconnect.shareinputdevices".to_string()]
        );
        // Capability honesty: outgoing is gated on the M2 portal
        // probe. A fresh plugin (probe not run / probe failed)
        // advertises nothing outgoing...
        assert!(plugin.outgoing_capabilities().is_empty());
        // ...and once the backend is available the request delta
        // appears (systemvolume/mod.rs:519-530 pattern).
        plugin.backend_available.store(true, Ordering::SeqCst);
        assert_eq!(
            plugin.outgoing_capabilities(),
            vec!["kdeconnect.shareinputdevices.request".to_string()]
        );
    }

    #[test]
    fn test_is_backend_available_is_false_until_m2_probe() {
        // M1 carries no portal presence probe. Until M2's probe
        // exists, the plugin must report unavailable so the
        // capability-honesty contract holds: the registry must
        // surface `false` from `is_backend_available()` and the
        // caller must NOT advertise the capability to the network.
        let plugin = ShareInputDevicesPlugin::new();
        assert!(!plugin.is_backend_available());
    }

    #[test]
    fn test_with_edge_replaces_configured_edge() {
        // Mirrors the `with_execution_timeout` builder pattern
        // used in runcommand.rs:97-100. M2's settings plumbing
        //   will call this with the configured edge.
        let plugin = ShareInputDevicesPlugin::new().with_edge(Edge::Top);
        assert_eq!(plugin.edge(), Edge::Top);
    }

    #[test]
    fn test_plugin_name() {
        let plugin = ShareInputDevicesPlugin::new();
        assert_eq!(plugin.name(), "shareinputdevices");
    }
}

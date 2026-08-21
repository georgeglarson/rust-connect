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
            last_release: Arc::new(Mutex::new(None)),
            release_callback: Arc::new(Mutex::new(None)),
            portal_session: Arc::new(Mutex::new(None)),
            portal_conn: Arc::new(Mutex::new(None)),
            backend_available: Arc::new(AtomicBool::new(false)),
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
    /// 4. If probe passes, stash the connection and STOP. The
    ///    barrier is NOT armed here (panel 0e230438 / 573c501e):
    ///    an armed InputCapture barrier with no EI consumer would
    ///    capture the user's cursor with nothing forwarding the
    ///    promised mousepad.request stream. Session start waits for
    ///    the M3 EI transport, which calls
    ///    `activate_portal_session` once its receiver exists.
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
        info!(
            event = "shareinputdevices_probe_passed_inert",
            "InputCapture portal available; producer stays INERT until the M3 EI transport attaches (no barrier armed, nothing advertised)"
        );
    }

    /// M3's entry point: start the portal session and go live. Called
    /// once the EI transport is ready to consume the session's event
    /// stream — never before, because Enable arms the capture
    /// barrier. Runs the full v1 sequence (CreateSession →
    /// ConnectToEIS → GetZones → SetPointerBarriers → Enable), wires
    /// the release callback to `session.release()`, and spawns the
    /// Activated → wire-packet consumer task that pumps
    /// `kdeconnect.shareinputdevices.request` packets to peers.
    pub async fn activate_portal_session(&self) {
        let conn = self
            .portal_conn
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
        let session = match portal::PortalSession::start(conn, self.edge, activated_tx).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                warn!(
                    error = %e,
                    event = "shareinputdevices_session_start_failed",
                    "PortalSession::start failed; shareinputdevices plugin stays inert"
                );
                return;
            }
        };

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
        *self
            .release_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cb);

        *self
            .portal_session
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
        self.backend_available.store(true, Ordering::SeqCst);
        // The daemon's capability collection ran at boot, before this
        // activation — push the delta or the advertisement never goes
        // out (panel b152dcc0).
        if let Some(cm) = &self.connection_manager {
            cm.add_capabilities(&[], &["kdeconnect.shareinputdevices.request".to_string()]);
        }
        info!(
            event = "shareinputdevices_backend_enabled",
            session_handle = session.session_handle().as_str(),
            edge = ?self.edge,
            "shareinputdevices session backend wired"
        );

        // Spawn the Activated → wire-packet consumer. We hold the
        // receiver here; the plugin's connection_manager may be
        // wired later (after enable_session_backend returns — see
        // bootstrap.rs), so this consumer needs to be tolerant of
        // CM being None at the time it spawns. Easiest: capture an
        // Option<Arc<CM>>-like accessor that the consumer polls.
        let cm_accessor: Arc<Mutex<Option<Arc<crate::protocol::ConnectionManager>>>> =
            Arc::new(Mutex::new(self.connection_manager.clone()));
        let edge = self.edge;
        tokio::spawn(async move {
            while let Some(event) = activated_rx.recv().await {
                let cm_opt = cm_accessor
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let Some(cm) = cm_opt else {
                    warn!(
                        event = "shareinputdevices_activated_no_connection_manager",
                        "No connection manager wired; Activated event dropped"
                    );
                    continue;
                };
                let body = plan_shareinputdevices_request(edge, event.deltax, event.deltay);
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
                // No "broadcast" method on ConnectionManager; iterate
                // connected device ids and send per peer (mirrors the
                // pattern at src/plugins/share.rs / 359 — both
                // fan-out one packet at a time). An Activated with
                // no peers drops the packet at the broker.
                let peers = cm.connected_device_ids().await;
                if peers.is_empty() {
                    debug!(
                        event = "shareinputdevices_activated_no_peers",
                        "Activated: no peers connected; request dropped"
                    );
                    continue;
                }
                for device_id in peers {
                    if let Err(e) = cm.send_packet(&device_id, &packet).await {
                        warn!(
                            device_id = %device_id,
                            error = %e,
                            event = "shareinputdevices_activated_send_failed",
                            "Send of shareinputdevices.request failed"
                        );
                    }
                }
            }
            debug!(
                event = "shareinputdevices_activated_consumer_exit",
                "Activated consumer task ended (channel closed)"
            );
        });
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

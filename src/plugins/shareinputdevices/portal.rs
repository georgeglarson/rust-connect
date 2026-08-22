//! Portal half of the ShareInputDevices producer (M2).
//!
//! Single Responsibility: drive the v1 InputCapture portal session
//! lifecycle over D-Bus and surface activation/deactivation/zones-
//! changed signals back to the M1 wire-shape planners.
//!
//! **D-Bus binding: raw zbus 5.14** (mirrors the mpris plugin's
//! `zbus_backend.rs`), **NOT ashpd**. Decision recorded in the M2
//! final message. Two reasons drove the call:
//!
//! 1. **Testability of the exact call sequence.** The brief mandates
//!    asserting `CreateSession caps=3 -> ConnectToEIS -> GetZones ->
//!    SetPointerBarriers -> Enable` with `ConnectToEIS strictly
//!    before Enable`. With raw zbus we own the call ordering inside
//!    `PortalSession::start`, and a fake D-Bus service on a private
//!    bus records the calls as they arrive. An ashpd wrapper would
//!    bundle the calls inside its `create_session`/`connect_to_eis`/
//!    `enable` methods, leaving the test to assert "did the wrapper
//!    drive these calls" — a layer further from the wire than the
//!    brief asks for.
//! 2. **Dependency weight.** ashpd 0.13 pulls in many additional
//!    transitive crates (screen-cast, secret-portal, etc.) for the
//!    ONE feature we need; the project explicitly minimises deps
//!    (`docs/functional-coverage.md` records this discipline).
//!    zbus 5.14 is already in the tree for the mpris plugin.
//!
//! Upstream producer implementation cited throughout:
//! kdeconnect-kde `plugins/shareinputdevices/inputcapturesession.cpp`,
//! pinned f5ed3ed8 in the M1 provenance. Spec citations are to
//! `/usr/share/dbus-1/interfaces/org.freedesktop.portal.InputCapture.xml`
//! (v1 of the interface, the only documented version on this host).
//!
//! **v1 sequence** (`inputcapturesession.cpp:91-263`):
//!   1. CreateSession("", {handle_token, session_handle_token,
//!      capabilities:3}) → reply is the Request object path; the
//!      Response signal carries the actual results
//!      (session_handle = `o`). We subscribe-then-wait so the same
//!      code works against an old xdp that doesn't support
//!      token-deterministic paths.
//!   2. ConnectToEIS(session_handle, {}) → `h` (fd). MUST precede
//!      Enable (spec InputCapture.xml:359-360). M2 stashes the fd
//!      into `PortalSession::_ei_fd`; M3 will wrap it in a libei /
//!      reis receiver.
//!   3. GetZones(session_handle, {handle_token}) → request → Response
//!      with results.zones = `a(uuii)`, results.zone_set = `u`.
//!   4. Compute the one barrier (pure: `barrier::plan_barrier`),
//!      then SetPointerBarriers(session_handle, {handle_token},
//!      [{barrier_id:1, position:[x1,y1,x2,y2] as `ai`}], zone_set) →
//!      request → Response with results.failed_barriers = `au`. Empty
//!      ⇒ success; non-empty ⇒ portal refused; the cpp logs and
//!      continues (inputcapturesession.cpp:248-250) — same policy
//!      here: log loudly, do not Enable.
//!   5. Enable(session_handle, {}). Now capture is armed.
//!
//! **Signals** we listen on the InputCapture interface (one stream,
//! filtered by path/iface/member):
//! - `Activated(o, a{sv})` → options carry `activation_id u` +
//!   `cursor_position (dd)` + `barrier_id u`. Forward
//!   `(cursor_position - barrier.p1())` to the M1 planner seam —
//!   that's what becomes the wire's exitEdge/deltax/deltay
//!   (inputcapturesession.cpp:295-296). The M1 planner already
//!   knows the configured edge so the caller composes the final
//!   `ShareInputDevicesRequest` body.
//! - `Deactivated(o, a{sv})` → log; no wire side-effect on M2.
//! - `Disabled(o, a{sv})` → same.
//! - `ZonesChanged(o, a{sv})` → re-GetZones + re-SetPointerBarriers
//!   (inputcapturesession.cpp:321-329). The portal says zones may be
//!   invalidated by a monitor change; the cpp's only filter is
//!   `options.zone_set >= m_currentZoneSet` (we follow the same:
//!   discard if stale).
//!
//! **Release path** (inputcapturesession.cpp:275-279): the M1
//! release callback invokes `PortalSession::release(deltax, deltay)`
//! which calls `Release(session_handle, {cursor_position:
//! barrier.p1() + (deltax, deltay)})`. The M2 release is
//! position-only — the cpp does not read its own activation_id at
//! release either (:275-279).
//!
//! **Teardown** (inputcapturesession.cpp:116-124): Session.Close +
//! ei_unref. M2 does Close only; ei_unref is M3. M4 owns the receiver
//! across the dedicated-thread boundary; the disconnect watcher flips
//! `backend_available=false` but does NOT close the session (mirrors
//! the cpp's `~InputCaptureSession` keeping `m_session` alive across
//! EI death).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use zbus::names::InterfaceName;
use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MessageStream};

use crate::plugins::shareinputdevices::barrier::{self, Zone};
use crate::plugins::shareinputdevices::ei::EiReceiver;
use crate::plugins::shareinputdevices::Edge;
use crate::utils::errors::{Error, Result};

fn internal(msg: impl Into<String>) -> Error {
    Error::Internal(msg.into())
}

/// Portal destination service name (the desktop multiplexer) and
/// object path. Mirrors the kdeconnect-kde `portalName()` /
/// `portalPath()` at `inputcapturesession.cpp:27-35`.
pub(crate) const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
pub(crate) const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub(crate) const INPUT_CAPTURE_IFACE: &str = "org.freedesktop.portal.InputCapture";
pub(crate) const SESSION_IFACE: &str = "org.freedesktop.portal.Session";
pub(crate) const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

/// Probe the portal at startup: interface present +
/// `SupportedCapabilities` has keyboard|pointer (bitmask 1|2) +
/// `version` >= 1. Returns true on the gate, false otherwise. This
/// is the gate the M1 plugin waited for: the plugin is
/// loader-registered unconditionally, and this probe's result (via
/// `backend_available`) is what gates the outgoing
/// `kdeconnect.shareinputdevices.request` advertisement.
///
/// Pure against a `Connection` (no `Arc`, no `Mutex`) so a fake
/// portal fixture (used by tests) can be probed with the same code
/// path. The brief's probe gate (lines 129-136) is the only caller.
pub async fn probe_portal_available(conn: &Connection) -> bool {
    // 1. version >= 1.
    let version = match read_property_u32(
        conn,
        PORTAL_DESTINATION,
        PORTAL_PATH,
        INPUT_CAPTURE_IFACE,
        "version",
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(
                error = %e,
                event = "shareinputdevices_portal_unavailable",
                "InputCapture portal: interface or version property unreadable; \
                 shareinputdevices plugin stays inert"
            );
            return false;
        }
    };
    if version < 1 {
        warn!(
            version,
            event = "shareinputdevices_portal_unavailable",
            "InputCapture portal reports version < 1; shareinputdevices plugin stays inert"
        );
        return false;
    }

    // 2. SupportedCapabilities bitmask has keyboard (1) | pointer (2).
    let caps = match read_property_u32(
        conn,
        PORTAL_DESTINATION,
        PORTAL_PATH,
        INPUT_CAPTURE_IFACE,
        "SupportedCapabilities",
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                error = %e,
                event = "shareinputdevices_portal_unavailable",
                "InputCapture portal: SupportedCapabilities unreadable; \
                 shareinputdevices plugin stays inert"
            );
            return false;
        }
    };
    if caps & 0b11 != 0b11 {
        warn!(
            capabilities = caps,
            event = "shareinputdevices_portal_unavailable",
            "InputCapture portal: SupportedCapabilities missing keyboard (1) or pointer (2); \
                 shareinputdevices plugin stays inert"
        );
        return false;
    }
    info!(
        version,
        capabilities = caps,
        event = "shareinputdevices_portal_available",
        "InputCapture portal probe passed; shareinputdevices plugin may advertise"
    );
    true
}

/// Read a single u32 D-Bus property via the standard
/// org.freedesktop.DBus.Properties.Get call. The reply body is a
/// D-Bus variant `v` per the spec; we unwrap it to u32. Raw zbus
/// keeps this self-contained — no `#[zbus::proxy]` macro required.
async fn read_property_u32(
    conn: &Connection,
    destination: &str,
    path: &str,
    interface: &str,
    property: &str,
) -> Result<u32> {
    let body: (String, String) = (interface.to_string(), property.to_string());
    let msg = conn
        .call_method(
            Some(destination),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &body,
        )
        .await
        .map_err(|e| internal(format!("Properties.Get({interface}.{property}): {e}")))?;
    // Spec: reply is a D-Bus variant. Unwrap it, then try to coerce
    // to u32 (covers u and possibly i).
    let v: zbus::zvariant::OwnedValue = msg.body().deserialize().map_err(|e| {
        internal(format!(
            "Properties.Get({interface}.{property}) reply not a variant: {e}"
        ))
    })?;
    u32::try_from(v).map_err(|e| {
        internal(format!(
            "Properties.Get({interface}.{property}) variant not u32: {e}"
        ))
    })
}

/// A vardict option for InputCapture method calls. Outgoing only;
/// we build `a{sv}` directly. Keyed by owned `String` and carrying
/// `Value<'static>` (so the body type is `HashMap<String, Value<'static>>`
/// — no lifetime headaches at the call site).
///
/// The portal's vardicts carry a small fixed key set per call:
/// `handle_token` (s), `session_handle_token` (s), `capabilities`
/// (u), `cursor_position` ((dd)), `activation_id` (u). Keeping the
/// shape explicit at the type level means a regression in the wire
/// signature fails the test, not just the runtime.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pairs: Vec<(String, Value<'static>)>,
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert_str(mut self, key: &str, value: &str) -> Self {
        self.pairs
            .push((key.to_string(), Value::from(value.to_string())));
        self
    }
    pub fn insert_u32(mut self, key: &str, value: u32) -> Self {
        self.pairs.push((key.to_string(), Value::U32(value)));
        self
    }
    /// `(dd)` shape — a tuple of two f64s, encoded as a Structure.
    /// The portal's `cursor_position` is exactly this (spec
    /// InputCapture.xml:337-345, cpp :278 with `QPointF`).
    pub fn insert_doubles(mut self, key: &str, x: f64, y: f64) -> Self {
        let tuple = Value::Structure(
            zbus::zvariant::StructureBuilder::new()
                .add_field(Value::F64(x))
                .add_field(Value::F64(y))
                .build()
                .expect("static (f64, f64) tuple cannot fail to build"),
        );
        self.pairs.push((key.to_string(), tuple));
        self
    }
    pub(crate) fn into_body(self) -> HashMap<String, Value<'static>> {
        let mut m = HashMap::with_capacity(self.pairs.len());
        for (k, v) in self.pairs {
            m.insert(k, v);
        }
        m
    }
}

/// Wait for the Response signal for a SPECIFIC request path on the
/// shared `stream` (which has a portal-wide Response match rule
/// pre-registered; see `start()`). The match rule is path-less
/// because the request path is unknown until the method call
/// returns its Request object, and the portal emits the Response
/// signal INLINE inside the method handler — by the time we have
/// the path, the signal has already gone out. Registering a
/// path-specific rule after `call_method` would miss it.
///
/// Filters by path/iface/member in the loop. Returns the
/// `(response_code, results_vardict)` pair when the matching
/// Response signal arrives, or an Err when the wait times out.
async fn await_request_response(
    stream: &mut MessageStream,
    request_path: &str,
    timeout: Duration,
) -> Result<(u32, HashMap<String, OwnedValue>)> {
    let iface: InterfaceName<'static> = REQUEST_IFACE
        .try_into()
        .map_err(|e| internal(format!("InterfaceName parse: {e}")))?;
    let member: zbus::names::MemberName<'static> = "Response"
        .try_into()
        .map_err(|e| internal(format!("MemberName parse: {e}")))?;
    let request_path = zbus::zvariant::ObjectPath::from_string_unchecked(request_path.to_string());
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(internal(format!(
                "Request Response timeout on {request_path}"
            )));
        }
        let remaining = deadline - now;
        let next = tokio::time::timeout(remaining, stream.next()).await;
        let msg = match next {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(internal(format!("MessageStream: {e}"))),
            Ok(None) => return Err(internal("MessageStream closed")),
            Err(_) => {
                return Err(internal(format!(
                    "Request Response timeout on {request_path}"
                )))
            }
        };
        let header = msg.header();
        if header.path().map(|p| p != &request_path).unwrap_or(true) {
            continue;
        }
        if header.interface().map(|i| i != &iface).unwrap_or(true) {
            continue;
        }
        if header.member().map(|m| m != &member).unwrap_or(true) {
            continue;
        }
        // Body: (u response, a{sv} results). Use OwnedValue so the
        // results outlive the body — we drop the body at the end of
        // the loop iteration and the caller still needs the map.
        let body = msg.body();
        let (code, results): (u32, HashMap<String, OwnedValue>) = body
            .deserialize()
            .map_err(|e| internal(format!("Request.Response body deserialize: {e}")))?;
        return Ok((code, results));
    }
}

/// Decode the `a(uuii)` zones vardict from a Response.results map.
///
/// The D-Bus signature is `(uuii)` per zone, which zvariant decodes
/// as a 4-element tuple `(width, height, x_offset, y_offset)`. We
/// re-pack it into the planner's `(x, y, width, height)` order.
fn decode_zones(results: &HashMap<String, OwnedValue>) -> Result<Zones> {
    let zone_set = results
        .get("zone_set")
        .ok_or_else(|| internal("GetZones Response: missing zone_set"))?;
    let zone_set = u32::try_from(
        zone_set
            .try_clone()
            .map_err(|e| internal(format!("GetZones Response: zone_set try_clone: {e}")))?,
    )
    .map_err(|e| internal(format!("GetZones Response: zone_set is not u32: {e}")))?;
    let zones_v = results
        .get("zones")
        .ok_or_else(|| internal("GetZones Response: missing zones"))?
        .try_clone()
        .map_err(|e| internal(format!("GetZones Response: zones try_clone: {e}")))?;
    // zones is an array of `(uuii)` tuples — we decode as Vec<(u32,u32,i32,i32)>.
    let raw: Vec<(u32, u32, i32, i32)> = Vec::try_from(zones_v)
        .map_err(|e| internal(format!("GetZones Response: zones is not a(uuii): {e}")))?;
    let zones = raw
        .into_iter()
        .map(|(width, height, x, y)| Zone::new(x, y, width, height))
        .collect();
    Ok(Zones { zone_set, zones })
}

/// The `position` vardict value of a barrier entry: an `ai` array of
/// `[x1, y1, x2, y2]`. Upstream sends `QVariant::fromValue(QList<int>
/// {x1, y1, x2, y2})` (inputcapturesession.cpp:230), and Qt marshals
/// QList<int> as a D-Bus ARRAY, not a struct — the StructureBuilder
/// route is wrong here: it wraps each field in a variant, putting
/// `(vvvv)` on the wire instead of `ai`.
fn barrier_position(rect: &barrier::Barrier) -> Value<'static> {
    Value::Array(zbus::zvariant::Array::from(vec![
        rect.x1, rect.y1, rect.x2, rect.y2,
    ]))
}

/// The portal's failure code for a non-zero Response is a u32:
/// 0 = success, 1 = user cancelled, 2 = "other". The cpp logs and
/// continues at non-zero (inputcapturesession.cpp:128-131, :169-172,
/// :243-246); we mirror — log loudly, do NOT advance the lifecycle.
fn require_success(code: u32, where_: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(internal(format!(
            "{where_} Response non-zero: {code} (1=user-cancelled, 2=other)"
        )))
    }
}

/// Generate a unique-per-call token. The portal uses this as the
/// last segment of the Request object's path, so collisions would
/// race two callers' subscriptions onto one object. Random u64
/// suffices — collision probability is ~2^-64 per call.
fn unique_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let v: u64 = rng.gen();
    format!("rustconnect_shareinputdevices_{v:x}")
}

#[derive(Debug, Clone)]
pub struct Zones {
    pub zone_set: u32,
    pub zones: Vec<Zone>,
}

/// The v1 session. Created by `PortalSession::start`; drives the
/// full CreateSession → ConnectToEIS → GetZones → SetPointerBarriers
/// → Enable sequence, then arms a signal-handling task that feeds
/// Activated/Deactivated/Disabled/ZonesChanged back into the M1
/// planners. Drop closes the session via Session.Close.
///
/// `#[allow(dead_code)]` covers the resources stashed for the
/// signal-handler task's lifetime — the task owns its own
/// clones (Arc / clone of the sender), so the struct's own
/// copies are unused after `start()` returns. Keeping them on
/// the struct makes the resource ownership visible at the
/// type level — they live as long as the session does.
#[allow(dead_code)]
pub struct PortalSession {
    conn: Connection,
    session_handle: OwnedObjectPath,
    /// Set by `close()` so Drop's best-effort Close doesn't fire a
    /// second time (the explicit path already Disabled + Closed).
    closed: bool,
    /// The current zone_set id, kept for the stale-discard filter on
    /// ZonesChanged (inputcapturesession.cpp:326).
    current_zone_set: Arc<Mutex<u32>>,
    /// The fd returned by ConnectToEIS. M4 moves ownership into the
    /// `EiReceiver` via `take_ei_fd()`; before that call the field
    /// is `Some` and the fd is alive (dropping the OwnedFd would
    /// close the socket and break the EI receiver the caller is
    /// about to install). After `take_ei_fd` returns, the field is
    /// `None`; the receiver owns the fd for the rest of its life.
    ei_fd: Option<std::os::unix::io::OwnedFd>,
    /// Late-binding slot for the M4 EI receiver. The signal handler
    /// reads it on every `Activated` and drains the gate if it is
    /// populated; until populate-time (activate_portal_session
    /// constructs the receiver from the fd) the slot is `None` and
    /// the drain step is a no-op. A late populate is safe because
    /// the gate's `should_queue()` condition (eis_sequence >
    /// activation_id) remains armed while slot is empty: any EI
    /// events that arrive between session start and receiver
    /// construction queue and replay when the receiver finally
    /// attaches AND the D-Bus Activated signal arrives. The
    /// `std::sync::Mutex` is held only long enough to clone the
    /// `Arc`; the async `handle_activated` runs without the guard
    /// (the M3 lock-ordering contract — `ei::EiReceiver::handle_activated`
    /// is self-contained once the `Arc` is in hand).
    ei_receiver_slot: Arc<Mutex<Option<Arc<EiReceiver>>>>,
    edge: Edge,
    /// Sender for Activated events. The owner translates each into
    /// the M1 planner's `ShareInputDevicesRequest` body and pushes
    /// it as a `kdeconnect.shareinputdevices.request` packet to the
    /// peer.
    activated_tx: mpsc::UnboundedSender<ActivatedEvent>,
    /// Shutdown signal — the signal-handling task exits when this
    /// sender is dropped (i.e. on `close()` or on `PortalSession`
    /// drop).
    _shutdown_tx: Option<oneshot::Sender<()>>,
    /// The barrier rectangle we set on `start()` / `rearm_barriers()`.
    /// Used to compute the relative delta on Activated
    /// (`cursor_position - barrier.topLeft()`,
    /// inputcapturesession.cpp:295-296). Updated on every rearm so
    /// that a ZonesChanged-driven rearm uses the new coordinates.
    barrier_origin: Arc<Mutex<(i32, i32)>>,
}

#[derive(Debug, Clone, Copy)]
pub struct ActivatedEvent {
    pub activation_id: u32,
    /// `cursor_position.x - barrier.x1` — the relative x delta
    /// that becomes `deltax` on the wire (inputcapturesession.cpp:295-296).
    pub deltax: f64,
    /// `cursor_position.y - barrier.y1` — the relative y delta.
    pub deltay: f64,
    pub barrier_id: u32,
}

impl std::fmt::Debug for PortalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalSession")
            .field("session_handle", &self.session_handle.as_str())
            .field("edge", &self.edge)
            .finish_non_exhaustive()
    }
}

impl PortalSession {
    /// Drive the v1 sequence end-to-end. Returns a session ready to
    /// release/teardown. The signal-handling task is spawned inside.
    pub async fn start(
        conn: Connection,
        edge: Edge,
        activated_tx: mpsc::UnboundedSender<ActivatedEvent>,
    ) -> Result<Self> {
        // Subscribe to ALL Response signals from the portal BEFORE
        // any method call. The match rule is path-less because the
        // Request path is unknown until the call returns its reply —
        // and the portal emits the Response signal INLINE inside the
        // method handler, so a path-specific rule registered after
        // `call_method` would miss it. `await_request_response`
        // filters by the path it was given.
        //
        // The match rule is what makes the bus daemon deliver the
        // signal at all: `MessageStream::from(&conn)` activates the
        // default `msg_receiver` clone, which has no rule, and the
        // reference dbus-daemon drops signals at the routing layer
        // when no rule on the connection matches them. The
        // `for_match_rule` constructor issues
        // `org.freedesktop.DBus.AddMatch` and queues the
        // corresponding RemoveMatch on stream drop — no manual
        // bookkeeping.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(PORTAL_DESTINATION)
            .map_err(|e| internal(format!("MatchRule sender: {e}")))?
            .interface(REQUEST_IFACE)
            .map_err(|e| internal(format!("MatchRule interface: {e}")))?
            .member("Response")
            .map_err(|e| internal(format!("MatchRule member: {e}")))?
            .build();
        let mut stream = MessageStream::for_match_rule(rule, &conn, Some(16))
            .await
            .map_err(|e| internal(format!("MessageStream::for_match_rule: {e}")))?;

        // 1. CreateSession — capabilities = keyboard | pointer = 3.
        let token = unique_token();
        let create_opts = Options::new()
            .insert_str("handle_token", &token)
            .insert_str("session_handle_token", &token)
            .insert_u32("capabilities", 3);
        let create_body = ("" as &str, create_opts.into_body());
        let request_handle: OwnedObjectPath =
            call_input_capture(&conn, "CreateSession", &create_body)
                .await
                .map_err(|e| internal(format!("CreateSession call: {e}")))?;
        let request_path = request_handle.as_str().to_string();

        let (code, results) =
            await_request_response(&mut stream, &request_path, Duration::from_secs(5))
                .await
                .map_err(|e| internal(format!("CreateSession Response: {e}")))?;
        require_success(code, "CreateSession")?;
        let session_handle_owned = results
            .get("session_handle")
            .ok_or_else(|| internal("CreateSession Response: missing session_handle"))?
            .try_clone()
            .map_err(|e| {
                internal(format!(
                    "CreateSession Response: session_handle try_clone: {e}"
                ))
            })?;
        let session_handle: OwnedObjectPath = OwnedObjectPath::try_from(session_handle_owned)
            .map_err(|e| {
                internal(format!(
                    "CreateSession Response: session_handle is not o: {e}"
                ))
            })?;

        // From here on, every error return must not leak the portal
        // session (panel 1a18cf7b) — the guard fires Session.Close
        // best-effort unless `defuse()` runs on the success path.
        let close_guard = SessionCloseGuard {
            conn: conn.clone(),
            session_handle: session_handle.clone(),
            armed: true,
        };

        // 2. ConnectToEIS — MUST precede Enable (spec InputCapture.xml:359-360).
        //    Inlined (not via `call_input_capture`) because the
        //    `h` handle is a borrowed `Fd<'m>` tied to the message
        //    body; the generic `R: DeserializeOwned` bound on
        //    `call_input_capture` can't carry that borrow.
        let connect_body = (session_handle.clone(), Options::new().into_body());
        let msg = conn
            .call_method(
                Some(PORTAL_DESTINATION),
                PORTAL_PATH,
                Some(INPUT_CAPTURE_IFACE),
                "ConnectToEIS",
                &connect_body,
            )
            .await
            .map_err(|e| internal(format!("ConnectToEIS call: {e}")))?;
        let ei_fd: std::os::unix::io::OwnedFd = {
            let body = msg.body();
            let fd_z: Fd<'_> = body
                .deserialize::<Fd<'_>>()
                .map_err(|e| internal(format!("ConnectToEIS deserialize fd: {e}")))?;
            // `TryFrom<Fd<'_>> for std::os::fd::OwnedFd` (zvariant
            // 5.10 fd.rs:56) transfers ownership: an `Fd::Owned`
            // variant yields the owned fd directly; an
            // `Fd::Borrowed` clone-to-owned. The portal returns
            // owned fds, so we get the underlying handle and drop
            // the zvariant::Fd wrapper without double-closing.
            std::os::fd::OwnedFd::try_from(fd_z)
                .map_err(|e| internal(format!("ConnectToEIS OwnedFd::try_from: {e}")))?
        };

        // 3. GetZones.
        let zones_token = unique_token();
        let get_zones_opts = Options::new().insert_str("handle_token", &zones_token);
        let get_zones_body = (session_handle.clone(), get_zones_opts.into_body());
        let zones_request: OwnedObjectPath = call_input_capture(&conn, "GetZones", &get_zones_body)
            .await
            .map_err(|e| internal(format!("GetZones call: {e}")))?;
        let (code, results) =
            await_request_response(&mut stream, zones_request.as_str(), Duration::from_secs(5))
                .await
                .map_err(|e| internal(format!("GetZones Response: {e}")))?;
        require_success(code, "GetZones")?;
        let zones = decode_zones(&results)?;
        if zones.zones.is_empty() {
            return Err(internal(
                "GetZones returned empty zones (spec: 'no pointer barriers can be set')",
            ));
        }

        // 4. Barrier math (pure planner).
        let barrier_id: u32 = 1;
        let barrier_rect = barrier::plan_barrier(&zones.zones, edge, barrier_id)
            .ok_or_else(|| internal("barrier::plan_barrier returned None"))?;
        let mut barrier_entry = HashMap::new();
        barrier_entry.insert("barrier_id".to_string(), Value::U32(barrier_id));
        barrier_entry.insert("position".to_string(), barrier_position(&barrier_rect));
        let barriers_arg: Vec<HashMap<String, Value<'static>>> = vec![barrier_entry];

        // 5. SetPointerBarriers.
        let barriers_token = unique_token();
        let set_opts = Options::new().insert_str("handle_token", &barriers_token);
        let set_body = (
            session_handle.clone(),
            set_opts.into_body(),
            barriers_arg,
            zones.zone_set,
        );
        let barriers_request: OwnedObjectPath =
            call_input_capture(&conn, "SetPointerBarriers", &set_body)
                .await
                .map_err(|e| internal(format!("SetPointerBarriers call: {e}")))?;
        let (code, results) = await_request_response(
            &mut stream,
            barriers_request.as_str(),
            Duration::from_secs(5),
        )
        .await
        .map_err(|e| internal(format!("SetPointerBarriers Response: {e}")))?;
        require_success(code, "SetPointerBarriers")?;
        let failed_v = results
            .get("failed_barriers")
            .map(|v| v.try_clone())
            .transpose()
            .map_err(|e| internal(format!("SetPointerBarriers failed_barriers try_clone: {e}")))?;
        let failed: Vec<u32> = match failed_v {
            Some(v) => Vec::<u32>::try_from(v)
                .map_err(|e| internal(format!("SetPointerBarriers failed_barriers decode: {e}")))?,
            None => Vec::new(),
        };
        if !failed.is_empty() {
            warn!(
                failed_barriers = ?failed,
                event = "shareinputdevices_barriers_failed",
                "Portal refused one or more barriers"
            );
            return Err(internal(format!(
                "SetPointerBarriers reported failed_barriers={failed:?}"
            )));
        }

        // 6. Subscribe to the session signals BEFORE Enable (panel
        // 66ae8992): the portal can emit Activated/Disabled/
        // ZonesChanged immediately on enable, and a match rule
        // registered after the call would miss them. Creating the
        // stream here (not inside the spawned task) is what makes the
        // ordering real — the AddMatch is issued before Enable goes
        // on the wire.
        let signal_stream = session_signal_stream(&conn).await?;

        // 7. Enable.
        let enable_body = (session_handle.clone(), Options::new().into_body());
        let _: () = call_input_capture(&conn, "Enable", &enable_body)
            .await
            .map_err(|e| internal(format!("Enable call: {e}")))?;

        info!(
            session_handle = session_handle.as_str(),
            zone_set = zones.zone_set,
            num_zones = zones.zones.len(),
            edge = ?edge,
            event = "shareinputdevices_session_enabled",
            "InputCapture portal session enabled; barrier armed"
        );

        let current_zone_set = Arc::new(Mutex::new(zones.zone_set));
        let barrier_origin = Arc::new(Mutex::new((barrier_rect.x1, barrier_rect.y1)));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let ei_receiver_slot: Arc<Mutex<Option<Arc<EiReceiver>>>> = Arc::new(Mutex::new(None));
        spawn_signal_handler(
            signal_stream,
            conn.clone(),
            session_handle.clone(),
            edge,
            current_zone_set.clone(),
            barrier_origin.clone(),
            activated_tx.clone(),
            ei_receiver_slot.clone(),
            shutdown_rx,
        );

        close_guard.defuse();
        Ok(Self {
            conn,
            session_handle,
            closed: false,
            current_zone_set,
            ei_fd: Some(ei_fd),
            ei_receiver_slot,
            edge,
            activated_tx,
            _shutdown_tx: Some(shutdown_tx),
            barrier_origin,
        })
    }

    /// Wire path for the phone's release packet — invoked from the
    /// M1 release callback with the peer-supplied delta. Mirrors
    /// inputcapturesession.cpp:275-279: cursor_position =
    /// barrier.p1() + release_delta — the peer delta is RELATIVE to
    /// the barrier origin; sending it raw would release the cursor
    /// at (deltax, deltay) absolute, wrong on any barrier away from
    /// (0,0) (panel be6019eb). M2 is position-only.
    pub async fn release(&self, deltax: i32, deltay: i32) -> Result<()> {
        let (bx, by) = *self
            .barrier_origin
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let opts = Options::new().insert_doubles(
            "cursor_position",
            f64::from(bx + deltax),
            f64::from(by + deltay),
        );
        let body = (self.session_handle.clone(), opts.into_body());
        let _: () = call_input_capture(&self.conn, "Release", &body)
            .await
            .map_err(|e| internal(format!("Release call: {e}")))?;
        debug!(
            deltax,
            deltay,
            event = "shareinputdevices_portal_release_sent",
            "Portal Release sent"
        );
        Ok(())
    }

    /// Disable + Close. Idempotent. The cpp's destructor calls Close
    /// unconditionally (inputcapturesession.cpp:118-124); we mirror.
    pub async fn close(mut self) -> Result<()> {
        self.closed = true;
        let _ = self._shutdown_tx.take();
        // Disable first (best-effort; the portal may have already
        // disabled itself).
        let _: () = call_input_capture(
            &self.conn,
            "Disable",
            &(self.session_handle.clone(), Options::new().into_body()),
        )
        .await
        .unwrap_or(());
        // Close the session object (inputcapturesession.cpp:118-120).
        let session_proxy = zbus::proxy::Proxy::new(
            &self.conn,
            PORTAL_DESTINATION,
            self.session_handle.as_str(),
            SESSION_IFACE,
        )
        .await
        .map_err(|e| internal(format!("Session proxy: {e}")))?;
        let reply = session_proxy
            .call_method("Close", &())
            .await
            .map_err(|e| internal(format!("Session.Close: {e}")))?;
        let _: () = reply
            .body()
            .deserialize()
            .map_err(|e| internal(format!("Session.Close reply: {e}")))?;
        debug!(
            event = "shareinputdevices_session_closed",
            "InputCapture portal session closed"
        );
        Ok(())
    }

    /// Read-only access to the session handle path, for diagnostics
    /// and for tests asserting the session object is exposed.
    pub fn session_handle(&self) -> &OwnedObjectPath {
        &self.session_handle
    }

    /// Move the ConnectToEIS fd out of the session. After this call
    /// the session no longer owns the fd; the caller has. M4 wiring
    /// in `activate_portal_session` calls this exactly once and hands
    /// the fd to `EiReceiver::new`. Dropping the fd would close the
    /// EIS stream — the receiver takes ownership so the socket stays
    /// open for the lifetime of the EI handshake + event pump. A
    /// second call panics: the session cannot resurrect an fd it no
    /// longer holds, and an unrecorded call site would silently leave
    /// the receiver side broken.
    pub fn take_ei_fd(&mut self) -> std::os::unix::io::OwnedFd {
        self.ei_fd
            .take()
            .expect("PortalSession::take_ei_fd called twice — second call has no fd to hand")
    }

    /// Late-binding slot for the M4 EI receiver. M4 wiring in
    /// `activate_portal_session` calls this once after building
    /// the receiver, populating the slot the signal handler's
    /// `Activated` arm reads. A second call replaces — the test
    /// seam can swap a fake receiver without disturbing the
    /// handler's lifetime, and a production race where two
    /// `activate_portal_session` calls overlap is a programming
    /// error (the plugin guards against re-entry elsewhere; here
    /// we just keep the slot's most-recent value).
    pub fn populate_ei_receiver(&self, receiver: Arc<EiReceiver>) {
        *self
            .ei_receiver_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(receiver);
    }

    /// Read-only access to the receiver slot — used by tests that
    /// want to observe whether the populate path has run. The
    /// production code never reads the slot (only the signal
    /// handler task spawned in `start` does).
    #[cfg(test)]
    pub fn ei_receiver_slot(&self) -> Arc<Mutex<Option<Arc<EiReceiver>>>> {
        self.ei_receiver_slot.clone()
    }
}

impl Drop for PortalSession {
    /// Backstop for the explicit `close()` (panel 1a18cf7b — the type
    /// doc promised this and nothing delivered it): a dropped session
    /// must not leak its portal object in xdp until the bus
    /// connection dies. Best-effort, skipped after `close()`.
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let conn = self.conn.clone();
            let handle = self.session_handle.clone();
            rt.spawn(async move {
                close_session_best_effort(&conn, &handle).await;
            });
        }
    }
}

/// Best-effort `Session.Close` guard for `PortalSession::start`'s
/// error paths (panel 1a18cf7b): every early return between
/// CreateSession succeeding and the session being armed would
/// otherwise leak the portal session. `defuse()` on the success path.
struct SessionCloseGuard {
    conn: Connection,
    session_handle: OwnedObjectPath,
    armed: bool,
}

impl SessionCloseGuard {
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for SessionCloseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let conn = self.conn.clone();
            let handle = self.session_handle.clone();
            rt.spawn(async move {
                close_session_best_effort(&conn, &handle).await;
            });
        }
    }
}

/// Fire-and-forget `Session.Close` (inputcapturesession.cpp:118-120).
/// Errors are logged, never propagated — this runs from Drop, where
/// there is no caller left to disappoint.
async fn close_session_best_effort(conn: &Connection, session_handle: &OwnedObjectPath) {
    let result = async {
        let session_proxy = zbus::proxy::Proxy::new(
            conn,
            PORTAL_DESTINATION,
            session_handle.as_str(),
            SESSION_IFACE,
        )
        .await
        .map_err(|e| internal(format!("Session proxy: {e}")))?;
        session_proxy
            .call_method("Close", &())
            .await
            .map_err(|e| internal(format!("Session.Close: {e}")))?;
        Ok::<(), crate::utils::errors::Error>(())
    }
    .await;
    if let Err(e) = result {
        warn!(
            error = %e,
            event = "shareinputdevices_session_close_best_effort_failed",
            "Best-effort Session.Close failed (session may already be gone)"
        );
    }
}

/// Build the InputCapture signal stream. The rule matches on
/// PORTAL_PATH — the portal emits Activated/Deactivated/Disabled/
/// ZonesChanged on the DESKTOP object with the session handle as the
/// first body element (inputcapturesession.cpp:94-100 connects them
/// on the InputCapture interface at portalPath(); panel 5c245d1a —
/// matching on the session path would deliver nothing).
/// `for_match_rule` issues `org.freedesktop.DBus.AddMatch` and queues
/// `RemoveMatch` on stream drop — no manual bookkeeping.
async fn session_signal_stream(conn: &Connection) -> Result<MessageStream> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(PORTAL_DESTINATION)
        .and_then(|b| b.interface(INPUT_CAPTURE_IFACE))
        .and_then(|b| b.path(PORTAL_PATH))
        .map(|b| b.build())
        .map_err(|e| internal(format!("signal MatchRule build: {e}")))?;
    MessageStream::for_match_rule(rule, conn, Some(16))
        .await
        .map_err(|e| internal(format!("signal stream subscribe: {e}")))
}

/// Spawn the signal-handling task. It runs until the
/// shutdown_rx fires OR the connection drops. Each signal is
/// dispatched in order: Activated → tx (consumer) + drain EI gate
/// if the receiver slot is populated; Deactivated/Disabled →
/// logged; ZonesChanged → re-GetZones + re-SetPointerBarriers
/// (filtered by zone_set id monotonicity — inputcapturesession.cpp:326).
/// The `stream` is created by the CALLER (before Enable) so the
/// AddMatch ordering against Enable is real.
///
/// **Activated ordering.** The handler does (1) decode +
/// `activated_tx.send` (sync, non-blocking) THEN (2) `.await`
/// `receiver.handle_activated(activation_id)`. Step 1 queues the
/// `shareinputdevices.request` packet event in the consumer's
/// mpsc; step 2 drains the EI gate and queues the
/// `kdeconnect.mousepad.request` packet events. The consumer
/// uses `tokio::select!` with `biased;` so the shareinputdevices
/// request is processed BEFORE the first mousepad packet on
/// every select iteration — the wire order on the receiver side
/// matches the cpp's `started(deltax, deltay)` → `for (event :
/// queuedEiEvents) handleEiEvent(event)` order (inputcapturesession
/// .cpp:296-300). The drain cannot land AFTER the first relayed
/// shareinputdevices.request because the consumer cannot make
/// progress until it observes the channel — and the channel
/// received both events before it was scheduled to run.
#[allow(clippy::too_many_arguments)]
fn spawn_signal_handler(
    mut stream: MessageStream,
    conn: Connection,
    session_handle: OwnedObjectPath,
    edge: Edge,
    current_zone_set: Arc<Mutex<u32>>,
    barrier_origin: Arc<Mutex<(i32, i32)>>,
    activated_tx: mpsc::UnboundedSender<ActivatedEvent>,
    ei_receiver_slot: Arc<Mutex<Option<Arc<EiReceiver>>>>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    tokio::spawn(async move {
        let iface: InterfaceName<'static> = INPUT_CAPTURE_IFACE.try_into().expect("iface parse");
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    debug!(event = "shareinputdevices_signal_handler_exit", "shutdown signalled");
                    return;
                }
                next = stream.next() => {
                    let Some(msg_result) = next else { return };
                    let msg = match msg_result {
                        Ok(m) => m,
                        Err(e) => {
                            warn!(error = %e, event = "shareinputdevices_signal_stream_error",
                                  "MessageStream error in signal handler");
                            return;
                        }
                    };
                    let header = msg.header();
                    if header.path().map(|p| p.as_str() != PORTAL_PATH).unwrap_or(true) {
                        continue;
                    }
                    if header.interface().map(|i| i != &iface).unwrap_or(true) {
                        continue;
                    }
                    // Session discrimination: all InputCapture signals
                    // share the desktop path, so the session handle is
                    // the first body element `(o, a{sv})` — act only on
                    // OUR session.
                    let (sig_session, _): (OwnedObjectPath, HashMap<String, OwnedValue>) =
                        match msg.body().deserialize() {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                    if sig_session != session_handle {
                        continue;
                    }
                    let member = match header.member() {
                        Some(m) => m.to_string(),
                        None => continue,
                    };
                    match member.as_str() {
                        "Activated" => {
                            // Step 1: decode + send ActivatedEvent to
                            // the consumer. Returns the activation_id
                            // so we can drive the EI gate drain in
                            // step 2 with the same value the consumer
                            // just saw.
                            if let Some(activation_id) = handle_activated(
                                &msg.body(),
                                &activated_tx,
                                &barrier_origin,
                            ) {
                                // Step 2: drain the EI gate. The slot
                                // is None while the M4 wiring is
                                // mid-construct (between PortalSession
                                // ::start and populate_ei_receiver);
                                // those events stay queued and replay
                                // on the next Activated once the
                                // receiver is in place. We clone the
                                // Arc out of the slot and release the
                                // std::sync lock BEFORE awaiting —
                                // guards are !Send and the receiver's
                                // pump future is !Send, but the
                                // already-running task here is on a
                                // multithread runtime; holding the
                                // std::sync::Mutex across an .await
                                // would block the runtime.
                                let receiver = {
                                    let guard = ei_receiver_slot
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner());
                                    guard.as_ref().cloned()
                                };
                                if let Some(r) = receiver {
                                    r.handle_activated(activation_id).await;
                                } else {
                                    debug!(
                                        activation_id,
                                        event = "shareinputdevices_activated_no_receiver",
                                        "Activated received before EI receiver attached; \
                                         gate drain deferred until receiver populates"
                                    );
                                }
                            }
                        }
                        "Deactivated" => {
                            debug!(event = "shareinputdevices_portal_deactivated",
                                   "Portal Deactivated received");
                        }
                        "Disabled" => {
                            warn!(event = "shareinputdevices_portal_disabled",
                                  "Portal Disabled received");
                        }
                        "ZonesChanged" => {
                            let body = msg.body();
                            let (_, opts): (OwnedObjectPath, HashMap<String, OwnedValue>) =
                                match body.deserialize() {
                                    Ok(v) => v,
                                    Err(e) => {
                                        warn!(error = %e,
                                              event = "shareinputdevices_zones_changed_decode",
                                              "ZonesChanged body decode failed");
                                        continue;
                                    }
                                };
                            let new_zone_set = opts
                                .get("zone_set")
                                .and_then(|v| u32::try_from(v.clone()).ok());
                            let current = *current_zone_set.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(new_zs) = new_zone_set {
                                if new_zs < current {
                                    debug!(
                                        new_zone_set = new_zs,
                                        current_zone_set = current,
                                        event = "shareinputdevices_zones_changed_stale",
                                        "Discarding stale ZonesChanged"
                                    );
                                    continue;
                                }
                            }
                            if let Err(e) = rearm_barriers(
                                &conn,
                                &session_handle,
                                edge,
                                &current_zone_set,
                                &barrier_origin,
                            ).await {
                                warn!(error = %e,
                                      event = "shareinputdevices_zones_changed_rearm_failed",
                                      "Re-GetZones + re-SetPointerBarriers failed");
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    });
}

fn handle_activated(
    body: &zbus::message::Body,
    activated_tx: &mpsc::UnboundedSender<ActivatedEvent>,
    barrier_origin: &Arc<Mutex<(i32, i32)>>,
) -> Option<u32> {
    let (_, opts): (OwnedObjectPath, HashMap<String, OwnedValue>) = match body.deserialize() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, event = "shareinputdevices_activated_decode",
                  "Activated body decode failed");
            return None;
        }
    };
    let activation_id = opts
        .get("activation_id")
        .and_then(|v| u32::try_from(v.clone()).ok())
        .unwrap_or(0);
    let barrier_id = opts
        .get("barrier_id")
        .and_then(|v| u32::try_from(v.clone()).ok())
        .unwrap_or(0);
    let cursor_position = opts.get("cursor_position").cloned();
    let (cursor_x, cursor_y) = match cursor_position.and_then(|v| <(f64, f64)>::try_from(v).ok()) {
        Some((x, y)) => (x, y),
        None => {
            warn!(
                event = "shareinputdevices_activated_no_cursor",
                "Activated signal missing cursor_position (dd)"
            );
            return None;
        }
    };
    let (bx, by) = *barrier_origin.lock().unwrap_or_else(|e| e.into_inner());
    let deltax = cursor_x - f64::from(bx);
    let deltay = cursor_y - f64::from(by);
    if let Err(e) = activated_tx.send(ActivatedEvent {
        activation_id,
        deltax,
        deltay,
        barrier_id,
    }) {
        warn!(error = %e, event = "shareinputdevices_activated_send_failed",
              "Activated receiver dropped");
    }
    Some(activation_id)
}

/// Re-fetch zones and re-arm barriers after ZonesChanged. Builds its
/// OWN Request-Response stream (panel 81ce9641 / 6bf0f9b7): the
/// signal handler's stream matches the InputCapture interface, but
/// Response signals arrive on the Request object path under
/// org.freedesktop.portal.Request — awaiting them on the signal
/// stream could only ever time out (and drained real session signals
/// while doing so). Same path-less rule shape as PortalSession::start.
async fn rearm_barriers(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    edge: Edge,
    current_zone_set: &Arc<Mutex<u32>>,
    barrier_origin: &Arc<Mutex<(i32, i32)>>,
) -> Result<()> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(PORTAL_DESTINATION)
        .and_then(|b| b.interface(REQUEST_IFACE))
        .and_then(|b| b.member("Response"))
        .map(|b| b.build())
        .map_err(|e| internal(format!("rearm MatchRule build: {e}")))?;
    let mut response_stream = MessageStream::for_match_rule(rule, conn, Some(16))
        .await
        .map_err(|e| internal(format!("rearm response stream: {e}")))?;
    let zones_token = unique_token();
    let get_opts = Options::new().insert_str("handle_token", &zones_token);
    let get_body = (session_handle.clone(), get_opts.into_body());
    let zones_req: OwnedObjectPath = call_input_capture(conn, "GetZones", &get_body).await?;
    let (code, results) = await_request_response(
        &mut response_stream,
        zones_req.as_str(),
        Duration::from_secs(5),
    )
    .await?;
    require_success(code, "GetZones(rearm)")?;
    let zones = decode_zones(&results)?;
    let barrier_id: u32 = 1;
    let barrier_rect = barrier::plan_barrier(&zones.zones, edge, barrier_id)
        .ok_or_else(|| internal("barrier::plan_barrier returned None on rearm"))?;
    let mut barrier_entry = HashMap::new();
    barrier_entry.insert("barrier_id".to_string(), Value::U32(barrier_id));
    barrier_entry.insert("position".to_string(), barrier_position(&barrier_rect));
    let barriers_token = unique_token();
    let set_opts = Options::new().insert_str("handle_token", &barriers_token);
    let set_body = (
        session_handle.clone(),
        set_opts.into_body(),
        vec![barrier_entry],
        zones.zone_set,
    );
    let barriers_req: OwnedObjectPath =
        call_input_capture(conn, "SetPointerBarriers", &set_body).await?;
    let (code, results) = await_request_response(
        &mut response_stream,
        barriers_req.as_str(),
        Duration::from_secs(5),
    )
    .await?;
    require_success(code, "SetPointerBarriers(rearm)")?;
    let failed: Vec<u32> = results
        .get("failed_barriers")
        .map(|v| Vec::<u32>::try_from(v.clone()).unwrap_or_default())
        .unwrap_or_default();
    if !failed.is_empty() {
        return Err(internal(format!(
            "SetPointerBarriers(rearm) failed: {failed:?}"
        )));
    }
    *current_zone_set.lock().unwrap_or_else(|e| e.into_inner()) = zones.zone_set;
    *barrier_origin.lock().unwrap_or_else(|e| e.into_inner()) = (barrier_rect.x1, barrier_rect.y1);
    info!(
        zone_set = zones.zone_set,
        event = "shareinputdevices_zones_changed_rearmed",
        "ZonesChanged: barriers re-armed"
    );
    Ok(())
}

/// Direct call_method() wrapper for the InputCapture interface. The
/// return type is `R` (caller-supplied). Errors include both
/// transport-level failures (no portal on the bus) and method-error
/// replies (portal reported failure).
async fn call_input_capture<B, R>(
    conn: &Connection,
    method: &'static str,
    body: &B,
) -> zbus::Result<R>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    R: serde::de::DeserializeOwned + zbus::zvariant::DynamicType + zbus::zvariant::Type,
{
    let msg = conn
        .call_method(
            Some(PORTAL_DESTINATION),
            PORTAL_PATH,
            Some(INPUT_CAPTURE_IFACE),
            method,
            body,
        )
        .await?;
    msg.body().deserialize::<R>()
}

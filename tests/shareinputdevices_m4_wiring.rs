//! Integration tests for the M4 wiring lane (Task #1042) — the
//! `ConnectToEIS` fd handoff + `handle_activated` hookup + disconnect
//! handling that connects the M2 portal half to the M3 EI half.
//!
//! **Composition.** The M4 lane stitches two earlier harnesses:
//!
//! - The M2 fake portal (extended with a `socketpair` fd option on
//!   `ConnectToEIS`), so the production `PortalSession::start`
//!   returns an `Arc<PortalSession>` whose `take_ei_fd()` hands
//!   back a real EIS peer — not `/dev/null`.
//! - A minimal fake EIS peer on a dedicated thread: completes the
//!   handshake so the receiver's `start()` returns, then keeps the
//!   read loop alive so the receiver's bind_capabilities makes it
//!   onto the EIS side.
//!
//! **What the tests pin:**
//!
//! 1. `take_ei_fd` returns the same fd the portal handed back from
//!    `ConnectToEIS`. (Verified by the socketpair handoff: the test
//!    owns the peer end; if production's fd is something else, the
//!    handshake times out.)
//! 2. The `Activated` D-Bus signal lands on the `activated_tx` the
//!    test owns — the signal handler routes correctly.
//! 3. The `Activated` signal also calls `EiReceiver::handle_activated`
//!    (via the populated slot). Observable as a backend state change
//!    when the EI peer drops — the `disconnect_rx` watch flips, the
//!    test consumer's backend flag goes false.
//!
//! **Outbound capture.** Each test reads a recording `mpsc` of
//! `OutboundPacket` (one entry per packet the unified consumer
//! would have sent). The recording channel substitutes for the
//! production `ConnectionManager.send_packet` so the test asserts
//! on the wire shape without the cryptographic / connection-state
//! weight of a real `ConnectionManager`.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reis::eis::{self};
use reis::enumflags2::BitFlags;
use reis::event::DeviceCapability;
use reis::handshake::EisHandshaker;
use reis::request::{Connection as EisConnection, Device as EisDevice, EisRequest};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use rust_connect::plugins::shareinputdevices::ei::{EiReceiver, WireBody};
use rust_connect::plugins::shareinputdevices::portal::{ActivatedEvent, PortalSession};
use rust_connect::plugins::shareinputdevices::Edge;

// ============ Test-private fake portal (mirrors shareinputdevices_portal_lifecycle) ============

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH_STR: &str = "/org/freedesktop/portal/desktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const INPUT_CAPTURE_IFACE: &str = "org.freedesktop.portal.InputCapture";

static BUS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct FakePortalState {
    pub version: u32,
    pub supported_caps: u32,
    pub session_handle: String,
    pub conn: Option<zbus::Connection>,
    pub zones: Vec<(u32, u32, i32, i32)>,
    pub request_id: Arc<AtomicU32>,
    pub connect_to_eis_socketpair: Option<OwnedFd>,
    /// If `Some`, the fake's `Enable` handler emits an `Activated`
    /// D-Bus signal with the listed `(activation_id, cursor_x,
    /// cursor_y)` BEFORE returning. Used by the M4 replay-on-
    /// populate test to deterministically reproduce the
    /// Enable-before-populate window: the portal's `Activated`
    /// reaches the signal handler while `populate_ei_receiver` has
    /// not yet been called.
    pub emit_activated_in_enable: Option<(u32, f64, f64)>,
}

struct DaemonGuard(Option<std::process::Child>);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_daemon(socket_path: &std::path::Path) -> std::process::Child {
    use std::process::{Command, Stdio};
    Command::new("dbus-daemon")
        .arg("--session")
        .arg(format!("--address=unix:path={}", socket_path.display()))
        .arg("--print-address")
        .arg("--nofork")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dbus-daemon failed")
}

async fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("dbus-daemon socket never appeared at {}", path.display());
}

struct FakePortal {
    state: Arc<Mutex<FakePortalState>>,
}

#[zbus::interface(name = "org.freedesktop.portal.InputCapture")]
impl FakePortal {
    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        self.state.lock().unwrap().version
    }

    #[zbus(property, name = "SupportedCapabilities")]
    async fn supported_capabilities(&self) -> u32 {
        self.state.lock().unwrap().supported_caps
    }

    async fn create_session(
        &self,
        _parent_window: String,
        options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        use std::collections::HashMap;
        let _capabilities = options
            .get("capabilities")
            .and_then(|v| u32::try_from(v.clone()).ok());
        let (session_handle, request_path, conn_for_signal) = {
            let guard = self.state.lock().unwrap();
            let session_handle = guard.session_handle.clone();
            let id = guard.request_id.fetch_add(1, Ordering::SeqCst);
            let request_path = format!("/org/freedesktop/portal/desktop/request/{id}");
            let conn_for_signal = guard.conn.as_ref().expect("conn").clone();
            (session_handle, request_path, conn_for_signal)
        };
        let mut results: HashMap<String, zbus::zvariant::OwnedValue> = HashMap::new();
        results.insert(
            "session_handle".to_string(),
            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::ObjectPath(
                zbus::zvariant::ObjectPath::from_string_unchecked(session_handle),
            ))
            .expect("session_handle OwnedValue"),
        );
        emit_response_signal(&conn_for_signal, &request_path, 0, results).await;
        Ok(zbus::zvariant::OwnedObjectPath::from(
            zbus::zvariant::ObjectPath::from_string_unchecked(request_path),
        ))
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        _session_handle: zbus::zvariant::OwnedObjectPath,
        _options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::Fd<'static>> {
        // Take the socketpair fd the test installed and hand it
        // back. Production's PortalSession::take_ei_fd will move
        // ownership into the EiReceiver; the OTHER end of the pair
        // stays in the harness for the fake EIS peer to drive.
        let owned = self
            .state
            .lock()
            .unwrap()
            .connect_to_eis_socketpair
            .take()
            .expect("ConnectToEIS called twice or no socketpair installed");
        Ok(zbus::zvariant::Fd::from(owned))
    }

    #[zbus(name = "GetZones")]
    async fn get_zones(
        &self,
        _session_handle: zbus::zvariant::OwnedObjectPath,
        _options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        use std::collections::HashMap;
        let (request_path, zones, conn_for_signal) = {
            let guard = self.state.lock().unwrap();
            let id = guard.request_id.fetch_add(1, Ordering::SeqCst);
            let request_path = format!("/org/freedesktop/portal/desktop/request/{id}");
            let zones = guard.zones.clone();
            let conn_for_signal = guard.conn.as_ref().expect("conn").clone();
            (request_path, zones, conn_for_signal)
        };
        let mut results: HashMap<String, zbus::zvariant::OwnedValue> = HashMap::new();
        results.insert(
            "zones".to_string(),
            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::Array(
                zbus::zvariant::Array::from(zones),
            ))
            .expect("zones OwnedValue"),
        );
        results.insert(
            "zone_set".to_string(),
            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::U32(0))
                .expect("zone_set OwnedValue"),
        );
        emit_response_signal(&conn_for_signal, &request_path, 0, results).await;
        Ok(zbus::zvariant::OwnedObjectPath::from(
            zbus::zvariant::ObjectPath::from_string_unchecked(request_path),
        ))
    }

    #[zbus(name = "SetPointerBarriers")]
    async fn set_pointer_barriers(
        &self,
        _session_handle: zbus::zvariant::OwnedObjectPath,
        _options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        _barriers: Vec<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>,
        _zone_set: u32,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        use std::collections::HashMap;
        let (request_path, conn_for_signal) = {
            let guard = self.state.lock().unwrap();
            let id = guard.request_id.fetch_add(1, Ordering::SeqCst);
            let request_path = format!("/org/freedesktop/portal/desktop/request/{id}");
            let conn_for_signal = guard.conn.as_ref().expect("conn").clone();
            (request_path, conn_for_signal)
        };
        let mut results: HashMap<String, zbus::zvariant::OwnedValue> = HashMap::new();
        results.insert(
            "failed_barriers".to_string(),
            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::Array(
                zbus::zvariant::Array::from(Vec::<u32>::new()),
            ))
            .expect("failed_barriers OwnedValue"),
        );
        emit_response_signal(&conn_for_signal, &request_path, 0, results).await;
        Ok(zbus::zvariant::OwnedObjectPath::from(
            zbus::zvariant::ObjectPath::from_string_unchecked(request_path),
        ))
    }

    async fn enable(
        &self,
        session_handle: zbus::zvariant::OwnedObjectPath,
        _options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        // Test seam: when the harness arms `emit_activated_in_enable`,
        // emit Activated on this conn BEFORE returning. The signal
        // handler's match rule is registered before Enable, so the
        // signal is delivered to the producer's connection while
        // `PortalSession::start` is still in Enable — the precise
        // window the M4 replay-on-populate fix targets.
        let (to_emit, conn_for_signal) = {
            let guard = self.state.lock().unwrap();
            (
                guard.emit_activated_in_enable,
                guard.conn.as_ref().expect("conn").clone(),
            )
        };
        if let Some((id, x, y)) = to_emit {
            emit_activated_signal(&conn_for_signal, session_handle.as_str(), id, x, y, 1).await;
        }
        Ok(())
    }
}

async fn emit_response_signal(
    conn: &zbus::Connection,
    request_path: &str,
    code: u32,
    results: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
) {
    use std::collections::HashMap;
    let body: (u32, HashMap<String, zbus::zvariant::OwnedValue>) = (code, results);
    conn.emit_signal(None::<&str>, request_path, REQUEST_IFACE, "Response", &body)
        .await
        .expect("emit Response signal");
}

async fn setup(state: Arc<Mutex<FakePortalState>>) -> Option<DaemonGuard> {
    use std::process::Command;
    if Command::new("dbus-daemon")
        .arg("--version")
        .output()
        .is_err()
    {
        // Loud-skip (panel M4 panel round 1 hygiene pass): the
        // previous silent-skip returned `None` and the caller
        // early-returned, marking the test as PASS — but no
        // coverage was actually exercised. A bare `eprintln` is
        // invisible in the test summary; a `panic!` makes the
        // absence a hard failure with the dependency name
        // surfaced. CI environments must provide dbus-daemon.
        panic!(
            "shareinputdevices integration tests require `dbus-daemon` on PATH \
             (panel M4 round 1 hygiene: silent-skip reported coverage that \
             was not exercised; install dbus-daemon to run these tests)"
        );
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("bus");
    std::env::set_var(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", socket_path.display()),
    );
    let daemon = spawn_daemon(&socket_path);
    wait_for_socket(&socket_path).await;

    let conn = zbus::Connection::session()
        .await
        .expect("fake portal: session connect failed");
    conn.object_server()
        .at(
            PORTAL_PATH_STR,
            FakePortal {
                state: state.clone(),
            },
        )
        .await
        .expect("fake portal: at() failed");
    conn.request_name(PORTAL_NAME)
        .await
        .expect("fake portal: request_name failed");
    state.lock().unwrap().conn = Some(conn.clone());

    Box::leak(Box::new(conn));
    Box::leak(Box::new(tmp));

    Some(DaemonGuard(Some(daemon)))
}

// ============ Fake EIS peer ============

/// Commands the test can issue to the fake EIS peer thread after the
/// handshake completes. The peer maintains thread-local state for the
/// `EisConnection`, `EisSeat`, and `EisDevice` (these are `!Send` —
/// they must stay on the eis-owning thread); each command is applied
/// to that local state, never copied across threads.
///
/// **Capability honesty (the r3 seat-widening note).** The receiver
/// binds `Keyboard | Pointer | Button | Scroll` (compile-pinned in
/// `EiReceiver::new`). For the bind to be accepted by the fake peer's
/// converter, the seat must advertise the SAME set — see reis
/// `request.rs:851` (`if !self.0.advertised_capabilities.contains(capability)`).
/// The M3 socketpair tests pin this with the comment block on
/// `seat_bind_reaches_the_eis_peer`. We pin the same here: callers
/// MUST pass `Keyboard | Pointer | Button | Scroll` to
/// `AddSeat` / `AddDevice` or the bind will be rejected and the
/// receiver's pump will hang on `DeviceAdded`.
#[derive(Debug, Clone)]
pub(crate) enum EisCommand {
    /// `Connection::add_seat(name, caps)` — emits `SeatAdded`.
    /// `caps` must be a superset of the device's capabilities, and
    /// must contain the receiver's bound set.
    AddSeat(BitFlags<DeviceCapability>),
    /// `Seat::add_device(name, Virtual, caps, |_| {})` — emits
    /// `DeviceAdded`. `caps` must be a subset of the seat's caps.
    AddDevice(BitFlags<DeviceCapability>),
    /// `Device::resumed()` — emits `DeviceResumed`.
    Resumed,
    /// `Device::start_emulating(seq)` — emits `DeviceStartEmulating`
    /// and arms the receiver's activation gate (sequence > activation_id).
    StartEmulating(u32),
    /// `Pointer::motion_relative(dx, dy)` + `Device::frame(0)` + flush.
    PointerMotion(f32, f32),
    /// `Button::button(btn, state)` + `Device::frame(0)` + flush.
    /// `is_press=true` → `Press`, `false` → `Released`.
    Button(u32, bool),
}

/// Test-side handle for the fake EIS peer. Cloning shares the same
/// channel; dropping every clone closes the peer's command stream
/// (the peer ignores `Disconnected` on `try_recv` and keeps draining).
#[derive(Clone)]
pub(crate) struct FakeEisEmitter {
    tx: tokio::sync::mpsc::Sender<EisCommand>,
}

impl FakeEisEmitter {
    pub fn add_seat_with_caps(&self, caps: BitFlags<DeviceCapability>) {
        self.tx
            .try_send(EisCommand::AddSeat(caps))
            .expect("fake eis emitter: AddSeat send (channel closed?)");
    }

    pub fn add_virtual_device_with_caps(&self, caps: BitFlags<DeviceCapability>) {
        self.tx
            .try_send(EisCommand::AddDevice(caps))
            .expect("fake eis emitter: AddDevice send");
    }

    pub fn resumed(&self) {
        self.tx
            .try_send(EisCommand::Resumed)
            .expect("fake eis emitter: Resumed send");
    }

    pub fn start_emulating(&self, sequence: u32) {
        self.tx
            .try_send(EisCommand::StartEmulating(sequence))
            .expect("fake eis emitter: StartEmulating send");
    }

    pub fn pointer_motion(&self, dx: f32, dy: f32) {
        self.tx
            .try_send(EisCommand::PointerMotion(dx, dy))
            .expect("fake eis emitter: PointerMotion send");
    }

    pub fn button(&self, btn: u32, is_press: bool) {
        self.tx
            .try_send(EisCommand::Button(btn, is_press))
            .expect("fake eis emitter: Button send");
    }
}

/// Spawn a thread that owns the peer end of the socketpair and
/// drives the EIS handshake, then keeps the read loop alive. The
/// `handshake_complete` oneshot fires once the connection is ready;
/// the `keep_alive` flag controls the post-handshake read loop; the
/// optional `cmd_rx` lets the test drive input events through the
/// peer (see `EisCommand` / `FakeEisEmitter`). The existing tests
/// pass `None`; the M4 relay test passes `Some(...)`.
fn spawn_fake_eis_peer(
    peer_stream: UnixStream,
    handshake_complete: oneshot::Sender<EisConnection>,
    keep_alive: Arc<AtomicBool>,
    cmd_rx: Option<tokio::sync::mpsc::Receiver<EisCommand>>,
) {
    std::thread::spawn(move || {
        eprintln!("[m4-test] fake eis peer thread starting");
        let eis_ctx = eis::Context::new(peer_stream).expect("eis Context::new");
        let handshaker = std::sync::Mutex::new(EisHandshaker::new(&eis_ctx, 1));
        let resp = loop {
            let _ = eis_ctx.read();
            let mut got = None;
            while let Some(result) = eis_ctx.pending_request() {
                let request = match result {
                    reis::PendingRequestResult::Request(r) => r,
                    _ => continue,
                };
                if let Some(r) = handshaker
                    .lock()
                    .unwrap()
                    .handle_request(request)
                    .expect("handshake handle_request")
                {
                    got = Some(r);
                }
            }
            if let Some(r) = got {
                eis_ctx.flush().expect("flush handshake");
                eprintln!("[m4-test] fake eis peer handshake complete");
                break r;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut converter = reis::request::EisRequestConverter::new(&eis_ctx, resp, 1);
        // `Connection` is `Arc`-backed (see reis `request.rs:67`),
        // so cloning is cheap. We need TWO clones: one ships out
        // via the handshake_complete oneshot for any test that
        // wants it; the other stays on this thread as the seed
        // for the command-driven seat/device state.
        let connection_for_thread = converter.handle().clone();
        let _ = handshake_complete.send(converter.handle().clone());

        // EIS-side handles — kept on this thread because reis ties
        // them to the eis::Context (which owns the socketpair fd).
        // Commands on `cmd_rx` mutate this state.
        let mut seat: Option<reis::request::Seat> = None;
        let mut device: Option<EisDevice> = None;

        // Post-handshake loop: drain the socket (so the receiver's
        // `bind_capabilities` reaches the EIS side and its own
        // motion/button writes are answered), AND drain pending
        // commands from the test. The two are interleaved so the
        // peer's read loop never starves the test's send queue.
        let mut cmd_rx = cmd_rx;
        while keep_alive.load(Ordering::SeqCst) {
            // Process up to a small batch of commands per loop tick
            // so a burst of events the test fires in quick
            // succession all reach the EIS side before the next
            // 5ms sleep.
            if let Some(rx) = cmd_rx.as_mut() {
                for _ in 0..16 {
                    match rx.try_recv() {
                        Ok(cmd) => {
                            apply_eis_command(&connection_for_thread, &mut seat, &mut device, cmd);
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            // Test dropped its emitter — keep
                            // draining until keep_alive flips.
                            cmd_rx = None;
                            break;
                        }
                    }
                }
            }

            // Drain: keep the receiver's bind on the EIS side and
            // handle any ACK bytes the receiver sends (rare on the
            // M4 path; the receiver is a pure consumer).
            let _ = eis_ctx.read();
            while let Some(result) = eis_ctx.pending_request() {
                let request = match result {
                    reis::PendingRequestResult::Request(r) => r,
                    _ => continue,
                };
                let _ = converter.handle_request(request);
            }
            while let Some(request) = converter.next_request() {
                // Bind requests are the only ones we expect on the
                // M4 path; drop the rest on the floor (we don't
                // echo them anywhere — the receiver's pump is
                // purely a consumer).
                if matches!(request, EisRequest::Bind(_)) {
                    // Drop the request; the receiver's bind already
                    // round-tripped and we don't need its contents.
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Drop the converter → closes our half of the socketpair
        // → receiver's pump sees EOF → disconnect arm fires.
        eprintln!("[m4-test] fake eis peer thread exiting");
        drop(converter);
    });
}

/// Apply a single `EisCommand` to the peer's local EIS state. Each
/// command mirrors the M3 socketpair emitter helpers verbatim —
/// `connection.add_seat(...)`, `seat.add_device(...)`, `device.resumed()`,
/// `device.start_emulating(seq)`, and the `motion+frame / button+frame
/// + flush` shape for event emitters.
fn apply_eis_command(
    connection: &EisConnection,
    seat: &mut Option<reis::request::Seat>,
    device: &mut Option<EisDevice>,
    cmd: EisCommand,
) {
    match cmd {
        EisCommand::AddSeat(caps) => {
            let s = connection.add_seat(Some("m4-test"), caps);
            // Flush so the receiver sees `SeatAdded` before the
            // fake peer's bind arrives. Without this, the receiver
            // processes the bind before the seat exists and the
            // EIS side would be missing the seat object.
            connection.flush().expect("fake peer: flush after AddSeat");
            *seat = Some(s);
        }
        EisCommand::AddDevice(caps) => {
            let s = seat.as_ref().expect("AddDevice before AddSeat").clone();
            let d = s.add_device(
                Some("m4-test-pointer"),
                reis::eis::device::DeviceType::Virtual,
                caps,
                |_device| {},
            );
            *device = Some(d);
        }
        EisCommand::Resumed => {
            let d = device.as_ref().expect("Resumed before AddDevice");
            d.resumed();
        }
        EisCommand::StartEmulating(seq) => {
            let d = device.as_ref().expect("StartEmulating before AddDevice");
            d.start_emulating(seq);
        }
        EisCommand::PointerMotion(dx, dy) => {
            let d = device.as_ref().expect("PointerMotion before AddDevice");
            let ptr: eis::Pointer = d.interface().expect("device has pointer interface");
            ptr.motion_relative(dx, dy);
            d.frame(0);
            connection.flush().expect("fake peer: flush PointerMotion");
        }
        EisCommand::Button(btn, is_press) => {
            let d = device.as_ref().expect("Button before AddDevice");
            let btn_iface: eis::Button = d.interface().expect("device has button interface");
            let state = if is_press {
                reis::eis::button::ButtonState::Press
            } else {
                reis::eis::button::ButtonState::Released
            };
            btn_iface.button(btn, state);
            d.frame(0);
            connection.flush().expect("fake peer: flush Button");
        }
    }
}

// ============ Test helpers ============

/// What the unified consumer would have sent. The test consumer
/// writes one of these per packet to its recording channel.
#[derive(Debug, Clone)]
struct OutboundPacket {
    packet_type: String,
    body: Value,
}

/// Mirror the production consumer's select! loop (mod.rs:
/// activate_portal_session) but route packets to a recording channel
/// instead of a `ConnectionManager`. The structural shape matches
/// production byte-for-byte: `biased;` select, mpsc::UnboundedReceiver
/// for both Activated + wire bodies, build Packet, send to recording.
async fn run_test_consumer(
    mut activated_rx: mpsc::UnboundedReceiver<ActivatedEvent>,
    mut wire_rx: mpsc::UnboundedReceiver<WireBody>,
    edge: Edge,
    tx: mpsc::UnboundedSender<OutboundPacket>,
) {
    let mut activated_closed = false;
    let mut wire_closed = false;
    loop {
        if activated_closed && wire_closed {
            return;
        }
        tokio::select! {
            biased;
            event = activated_rx.recv(), if !activated_closed => {
                match event {
                    Some(event) => {
                        let body = serde_json::json!({
                            "exitEdge": i32::from(edge),
                            "deltax": event.deltax,
                            "deltay": event.deltay,
                        });
                        let _ = tx.send(OutboundPacket {
                            packet_type: "kdeconnect.shareinputdevices.request".to_string(),
                            body,
                        });
                    }
                    None => {
                        activated_closed = true;
                    }
                }
            }
            body = wire_rx.recv(), if !wire_closed => {
                match body {
                    Some(wire_body) => {
                        let _ = tx.send(OutboundPacket {
                            packet_type: "kdeconnect.mousepad.request".to_string(),
                            body: wire_body.into_json(),
                        });
                    }
                    None => {
                        wire_closed = true;
                    }
                }
            }
        }
    }
}

/// Emit an Activated signal on the InputCapture interface that
/// matches the production D-Bus wire shape: `(o session_handle,
/// a{sv} options)` where options carry `activation_id u`,
/// `cursor_position (dd)`, `barrier_id u`.
async fn emit_activated_signal(
    conn: &zbus::Connection,
    session_handle: &str,
    activation_id: u32,
    cursor_x: f64,
    cursor_y: f64,
    barrier_id: u32,
) {
    use std::collections::HashMap;
    let mut opts: HashMap<String, zbus::zvariant::Value<'_>> = HashMap::new();
    opts.insert(
        "activation_id".to_string(),
        zbus::zvariant::Value::U32(activation_id),
    );
    opts.insert(
        "barrier_id".to_string(),
        zbus::zvariant::Value::U32(barrier_id),
    );
    // cursor_position is a D-Bus `(dd)` per InputCapture.xml:337-345
    // and inputcapturesession.cpp:278. zvariant's `Value::Structure`
    // with `Value::F64` fields produces a `(vv)` wire encoding (the
    // signature shows Variant, Variant — see zvariant 5.14
    // StructureBuilder), which the production decode at
    // portal.rs's `handle_activated` rejects via
    // `<(f64, f64)>::try_from`. The fix is to put the tuple in the
    // HashMap directly — zvariant serializes a `(f64, f64)` Rust
    // tuple as the spec's `(dd)` STRUCT.
    opts.insert("cursor_position".to_string(), (cursor_x, cursor_y).into());
    let body = (
        zbus::zvariant::ObjectPath::from_string_unchecked(session_handle.to_string()),
        opts,
    );
    conn.emit_signal(
        None::<&str>,
        PORTAL_PATH_STR,
        INPUT_CAPTURE_IFACE,
        "Activated",
        &body,
    )
    .await
    .expect("emit Activated signal");
}

struct M4Harness {
    outbound_rx: mpsc::UnboundedReceiver<OutboundPacket>,
    backend_available: Arc<AtomicBool>,
    keep_alive: Arc<AtomicBool>,
    session_handle: String,
    /// The fake portal's D-Bus connection — used to emit signals
    /// (Activated, etc.) with the well-known bus name
    /// `org.freedesktop.portal.Desktop` as sender. The match rule
    /// in `session_signal_stream` filters on that sender; emitting
    /// from a fresh connection uses the unique-name sender, which
    /// the rule does not match.
    fake_conn: zbus::Connection,
    /// Optional handle for driving EI events through the fake peer.
    /// `Some` only when `setup_m4_harness` was called with
    /// `with_emitter = true` (the M4 relay test); `None` for the
    /// existing signal-routing / disconnect tests.
    emitter: Option<FakeEisEmitter>,
    /// Held to keep the receiver + portal session alive for the test's
    /// duration. Dropping it tears the wiring down.
    _resources: M4Resources,
}

struct M4Resources {
    _session: Arc<PortalSession>,
    _receiver: Arc<EiReceiver>,
    _daemon: DaemonGuard,
}

async fn setup_m4_harness(session_handle: &str, with_emitter: bool) -> Option<M4Harness> {
    let (client_stream, peer_stream) = UnixStream::pair().expect("UnixStream::pair");
    let connect_to_eis_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(client_stream.into_raw_fd()) };
    // NOTE: the fd number changes across the D-Bus boundary
    // (zbus serialises an `Fd` via SCM_RIGHTS — the kernel hands the
    // receiver a *new* fd whose number matches no handle on the
    // sender). The wiring is verified by the EIS handshake
    // completing below: if production's `take_ei_fd` returned a
    // fd NOT connected to the fake peer, the receiver's HELLO
    // bytes would never reach the peer's read loop and the
    // handshake oneshot would time out.

    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: session_handle.to_string(),
        zones: vec![(1920, 1080, 0, 0)],
        connect_to_eis_socketpair: Some(connect_to_eis_fd),
        ..Default::default()
    }));

    let _daemon = match setup(state.clone()).await {
        Some(d) => d,
        None => return None,
    };

    let fake_conn = state
        .lock()
        .unwrap()
        .conn
        .clone()
        .expect("setup stored conn");

    let conn = zbus::Connection::session().await.unwrap();
    let (activated_tx, activated_rx) = mpsc::unbounded_channel::<ActivatedEvent>();
    // `take_ei_fd` requires `&mut PortalSession`. We unwrap the
    // Arc only after the M4 wiring (take_ei_fd + populate_ei_receiver)
    // completes; for the test's lifetime the harness holds the Arc.
    let mut session = tokio::time::timeout(
        Duration::from_secs(5),
        PortalSession::start(conn, Edge::Left, activated_tx),
    )
    .await
    .expect("PortalSession::start timed out")
    .expect("PortalSession::start must succeed");

    let ei_fd = session.take_ei_fd();
    let _ = ei_fd.as_raw_fd(); // pin the local; the fd travels to EiReceiver::new below
    let receiver = EiReceiver::new(ei_fd, "shareinputdevices-m4-test")
        .expect("EiReceiver::new must succeed against the socketpair");

    let (handshake_tx, handshake_rx) = oneshot::channel::<EisConnection>();
    let keep_alive = Arc::new(AtomicBool::new(true));
    // Optionally wire an emitter channel into the fake peer. The
    // existing signal-routing and disconnect tests pass
    // `with_emitter = false` and get `None`; the M4 relay test
    // passes `true` and gets a `FakeEisEmitter` handle for driving
    // events through the gate.
    let emitter = if with_emitter {
        let (tx, rx) = mpsc::channel::<EisCommand>(32);
        spawn_fake_eis_peer(peer_stream, handshake_tx, keep_alive.clone(), Some(rx));
        Some(FakeEisEmitter { tx })
    } else {
        spawn_fake_eis_peer(peer_stream, handshake_tx, keep_alive.clone(), None);
        None
    };

    session.populate_ei_receiver(Arc::clone(&receiver)).await;

    // Drive the receiver pump on a dedicated thread BEFORE
    // awaiting the handshake — the pump and the EIS peer make
    // progress concurrently. The handshake completes when both
    // sides exchange HELLO + finish.
    let (wire_rx_tx, wire_rx_rx) =
        tokio::sync::oneshot::channel::<mpsc::UnboundedReceiver<WireBody>>();
    let (disconnect_rx_tx, disconnect_rx_rx) =
        tokio::sync::oneshot::channel::<tokio::sync::watch::Receiver<bool>>();
    let pump_receiver = Arc::clone(&receiver);
    let _pump_handle = std::thread::Builder::new()
        .name("m4-test-ei-pump".to_string())
        .spawn(move || {
            eprintln!("[m4-test] ei pump thread starting");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime build");
            rt.block_on(async move {
                eprintln!("[m4-test] calling receiver.start()");
                let (wire_rx, disconnect_rx, drive) =
                    pump_receiver.start().await.expect("receiver start");
                eprintln!("[m4-test] receiver.start() returned");
                let _ = wire_rx_tx.send(wire_rx);
                let _ = disconnect_rx_tx.send(disconnect_rx);
                drive.await;
            });
        })
        .expect("pump thread spawn");

    let _eis_conn = tokio::time::timeout(Duration::from_secs(5), handshake_rx)
        .await
        .expect("EIS handshake timed out — fd wiring is broken")
        .expect("EIS handshake send failed");

    let wire_rx = tokio::time::timeout(Duration::from_secs(5), wire_rx_rx)
        .await
        .expect("wire_rx delivery timed out")
        .expect("wire_rx channel closed");
    let disconnect_rx = tokio::time::timeout(Duration::from_secs(5), disconnect_rx_rx)
        .await
        .expect("disconnect_rx delivery timed out")
        .expect("disconnect_rx channel closed");

    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<OutboundPacket>();
    tokio::spawn(run_test_consumer(
        activated_rx,
        wire_rx,
        Edge::Left,
        outbound_tx,
    ));

    // Disconnect watcher mirrors the production wiring. The
    // backend_available flag starts true; flips false when the EI
    // pump's disconnect arm fires.
    let backend_available = Arc::new(AtomicBool::new(true));
    let backend_avail_watcher = backend_available.clone();
    let mut drx = disconnect_rx;
    tokio::spawn(async move {
        if drx.changed().await.is_ok() {
            backend_avail_watcher.store(false, Ordering::SeqCst);
        }
    });

    Some(M4Harness {
        outbound_rx,
        backend_available,
        keep_alive,
        session_handle: session_handle.to_string(),
        fake_conn,
        emitter,
        _resources: M4Resources {
            _session: Arc::new(session),
            _receiver: receiver,
            _daemon,
        },
    })
}

// ============ TESTS ============

/// M4 wiring test 1: the `Activated` D-Bus signal lands on the
/// `activated_rx` the test's consumer owns. This proves the
/// PortalSession's signal handler routes Activated to the consumer
/// (the shareinputdevices.request emission path). With the
/// receiver slot populated and the EI queue empty, Activated
/// triggers a no-op drain — the recording channel sees only the
/// one shareinputdevices.request.
///
/// **What this pins:**
/// - `PortalSession::start` end-to-end against the M2 fake portal.
/// - `take_ei_fd` returns the same fd ConnectToEIS handed back.
/// - The fake EIS peer completes the handshake — proves the fd
///   wiring is real (any other fd would not see the receiver's
///   HELLO bytes).
/// - The Activated D-Bus signal reaches the signal handler task,
///   which decodes cursor_position, computes deltax/deltay against
///   the barrier origin, and pushes the ActivatedEvent to
///   `activated_tx` — the consumer reads it and emits the
///   `kdeconnect.shareinputdevices.request` packet.
/// - `populate_ei_receiver` populated the slot before the signal
///   landed; the signal handler's `handle_activated` call ran (a
///   no-op for an empty queue) and did not error.
#[tokio::test(flavor = "multi_thread")]
async fn m4_activated_signal_routes_to_consumer_via_session() {
    let _bus_lock = BUS.lock().await;
    let Some(mut harness) =
        setup_m4_harness("/org/freedesktop/portal/desktop/session/m4test1", false).await
    else {
        return;
    };

    // The barrier on a 1920x1080 zone with Edge::Left is the line
    // x=0, y from 0 to 1079. Its p1 (top-left) is (0, 0), so
    // deltax/deltay = cursor_position verbatim. Emit Activated with
    // cursor_position = (50, 100), activation_id = 42, barrier_id = 1.
    let handle = harness.session_handle.clone();
    eprintln!("[m4-test] emitting Activated signal via fake portal conn");
    emit_activated_signal(&harness.fake_conn, &handle, 42, 50.0, 100.0, 1).await;
    eprintln!("[m4-test] Activated signal emitted");

    // Expect the shareinputdevices.request packet.
    let outbound = tokio::time::timeout(Duration::from_secs(2), harness.outbound_rx.recv())
        .await
        .expect("did not see the shareinputdevices.request within 2s")
        .expect("outbound channel closed");
    assert_eq!(
        outbound.packet_type, "kdeconnect.shareinputdevices.request",
        "first packet after Activated must be the activation announcement"
    );
    assert_eq!(
        outbound.body,
        serde_json::json!({
            "exitEdge": 2, // Edge::Left
            "deltax": 50.0,
            "deltay": 100.0,
        }),
        "shareinputdevices.request body must match cursor_position minus barrier.p1"
    );

    // No further packets — the EI queue was empty, so the drain
    // produced nothing. Poll briefly to confirm no spurious
    // mousepad.request sneaks in.
    let spurious =
        tokio::time::timeout(Duration::from_millis(200), harness.outbound_rx.recv()).await;
    assert!(
        spurious.is_err() || spurious.is_ok_and(|v| v.is_none()),
        "no further packets expected after the empty-queue drain"
    );
}

/// M4 wiring test 2: the disconnect watcher flips the backend
/// flag when the EI peer drops. Mirrors the cpp's
/// inputcapturesession.cpp:372-374 — the disconnect is logged and
/// observed, but the session is NOT closed (the destruction path
/// closes it explicitly via Session.Close; the Disabled signal is
/// the session-side teardown trigger).
///
/// **What this pins:**
/// - The EI pump's terminal-disconnect path sends `true` on
///   `disconnect_tx`, observable on the watch receiver.
/// - The `tokio::spawn`'d watcher task reacts and flips
///   `backend_available` to false — the production wiring's
///   behaviour, observed from outside the wiring.
#[tokio::test(flavor = "multi_thread")]
async fn m4_ei_peer_disconnect_flips_backend_available() {
    let _bus_lock = BUS.lock().await;
    let Some(harness) =
        setup_m4_harness("/org/freedesktop/portal/desktop/session/m4test3", false).await
    else {
        return;
    };

    assert!(
        harness.backend_available.load(Ordering::SeqCst),
        "backend_available must start true after a successful start()"
    );

    // Signal the fake EIS peer to stop its read loop. Dropping the
    // converter closes the EIS-side socketpair end, which makes the
    // receiver's read return EOF, which makes `disconnect_tx.send(true)`
    // fire, which the watcher observes.
    harness.keep_alive.store(false, Ordering::SeqCst);

    // Wait for the flip with a generous timeout. The peer thread
    // polls every 5ms; the converter drops; the receiver sees EOF;
    // the pump's disconnect arm runs `let _ = disconnect_tx.send(true)`;
    // the watcher's `.changed()` resolves; backend_available flips.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !harness.backend_available.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("backend_available did not flip to false within 3s of EI peer shutdown");
}

/// M4 wiring test 3 (the brief's load-bearing case): input flowing
/// through the fake EIS peer, through the activation gate, drained
/// by the `Activated` signal, and relayed as
/// `kdeconnect.mousepad.request` packets by the unified consumer's
/// `body = wire_rx.recv()` arm. The deliveries `m4_activated_signal
/// _routes_to_consumer_via_session` makes (one shareinputdevices
/// .request on an empty queue) leave the EI queue empty — that arm
/// of the consumer has ZERO coverage without this test.
///
/// **Scenario.** The fake portal's session runs at (0, 0)..(1920,
/// 1080), barrier on `Edge::Left` with p1 (0, 0). The fake peer
/// advertises a seat with the receiver's bound caps (`Keyboard |
/// Pointer | Button | Scroll`), then adds a virtual pointer device,
/// resumes it, and `start_emulating(7)` arms the gate. A pointer
/// motion + a button press + a button release fire BEFORE the
/// D-Bus `Activated` signal — the cpp's `m_currentEisSequence >
/// m_currentActivationId` rule queues every one of them. We assert
/// NOTHING reaches the recording consumer yet (the wire_rx arm
/// must stay silent while the gate is armed). Then `Activated`
/// fires with `activation_id = 7` — the gate drains in arrival
/// order, the cpp-mirror `started(deltax, deltay)` packet ships
/// first via the `biased;` select, then each queued body reaches
/// the consumer's wire arm in arrival order.
///
/// **What this pins:**
///
/// - The EI peer → activation gate → `handle_activated` → wire_rx
///   pipeline delivers each queued `WireBody` exactly once and in
///   arrival order.
/// - The unified consumer's `biased; tokio::select!` produces the
///   `kdeconnect.shareinputdevices.request` announcement BEFORE the
///   first `kdeconnect.mousepad.request` on every select iteration —
///   the cpp's `started(…)` first / queued-events-after ordering.
/// - The drain's body shape matches M3's `plan_motion` / `plan_button`
///   verbatims — `dx`/`dy` present and matching what the fake peer
///   sent, `{singlehold: true}` for BTN_LEFT press,
///   `{singlerelease: true}` for BTN_LEFT release.
/// - End-to-end delivery from a non-test live EIS source (the fake
///   peer's reis high-level helpers) into the consumer's recording
///   channel.
#[tokio::test(flavor = "multi_thread")]
async fn m4_input_relays_through_gate_and_consumer() {
    let _bus_lock = BUS.lock().await;
    let Some(mut harness) =
        setup_m4_harness("/org/freedesktop/portal/desktop/session/m4test_relay", true).await
    else {
        return;
    };
    let emitter = harness
        .emitter
        .take()
        .expect("emitter must be wired when with_emitter = true");

    // The receiver binds `Keyboard | Pointer | Button | Scroll`
    // (compile-pinned in `EiReceiver::new`). The seat must
    // advertise exactly that set — reis's `add_device` checks
    // against `advertised_capabilities`, and a mismatch would
    // reject the bind and stall `DeviceAdded`. The M3
    // socketpair test `seat_bind_reaches_the_eis_peer` pins this
    // contract (see its `connection.add_seat(... caps)` call).
    let caps = DeviceCapability::Keyboard
        | DeviceCapability::Pointer
        | DeviceCapability::Button
        | DeviceCapability::Scroll;
    emitter.add_seat_with_caps(caps);
    emitter.add_virtual_device_with_caps(caps);
    emitter.resumed();

    // `start_emulating(7)` triggers DeviceStartEmulating on the
    // receiver, which sets `eis_sequence = 7`. With
    // `activation_id = 0`, the gate becomes armed
    // (`should_queue()` returns true). Every subsequent input
    // event is queued in arrival order.
    emitter.start_emulating(7);

    // Let the SeatAdded → bind → DeviceAdded → DeviceResumed →
    // DeviceStartEmulating chain reach the receiver before we
    // queue motion/button events. The peer thread polls at 5ms;
    // each reis high-level op auto-flushes, so this is mostly
    // round-trip + processing time on the receiver's pump thread.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send pointer motion + BTN_LEFT press + BTN_LEFT release
    // BEFORE any Activated signal. The gate armed above means
    // every one of these queues in arrival order.
    //
    // `plan_motion` (mod.rs:170-172) emits `{dx, dy}` — dx/dy
    // pass through verbatim. The fake peer's reis
    // `Pointer::motion_relative` takes f32, our planner takes f64;
    // 5.0_f32 → 5.0_f64 exactly representable.
    //
    // `plan_button` (mod.rs:203-217): BTN_LEFT press → `{singlehold:
    // true}`, BTN_LEFT release → `{singlerelease: true}`.
    // BTN_LEFT = 0x110 per Linux input-event-codes (the same
    // constant the M3 button_press_release_round_trip test uses).
    emitter.pointer_motion(5.0, 7.0);
    emitter.button(0x110, true); // BTN_LEFT press
    emitter.button(0x110, false); // BTN_LEFT release

    // Wait for the events to be delivered into the gate's queue.
    // The peer thread polls every 5ms; the receiver's pump
    // processes events on the dedicated pump thread's
    // current-thread runtime. 200ms is plenty for three motion-
    // /button-class writes to round-trip and queue.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Oracle 1: gate armed, nothing reached the consumer yet.
    // The wire_rx arm of the unified consumer must stay silent
    // until Activated drains the gate. (The activated_rx arm is
    // also silent — no Activated signal has been emitted yet.)
    let premature =
        tokio::time::timeout(Duration::from_millis(150), harness.outbound_rx.recv()).await;
    assert!(
        premature.is_err(),
        "gate must hold events: an outbound packet reached the consumer \
         while the activation gate was armed. Got {:?}",
        premature.ok().flatten().map(|p| (p.packet_type, p.body))
    );

    // Emit Activated with `activation_id = 42` — deliberately NOT the
    // EIS sequence (7): production always passes the matching id, but
    // the drain does not depend on it (`handle_activated` opens the
    // gate and replays the queue in arrival order whatever id it is
    // given), so the mismatch here is incidental and pins that the
    // consumer reads the queued WIRE BODIES, not the activation id.
    // The barrier's p1 is (0, 0) (1920×1080 zone with Edge::Left), so
    // deltax/deltay = cursor_position verbatim. cursor = (50, 100),
    // barrier_id = 1.
    let handle = harness.session_handle.clone();
    eprintln!("[m4-test-relay] emitting Activated signal via fake portal conn");
    emit_activated_signal(&harness.fake_conn, &handle, 42, 50.0, 100.0, 1).await;
    eprintln!("[m4-test-relay] Activated signal emitted");

    // Oracle 2a: FIRST packet is the shareinputdevices.request
    // announcement. The `biased;` select in `run_test_consumer`
    // (which mirrors `activate_portal_session`'s production
    // consumer — see mod.rs:766-768) processes the
    // activated_rx arm first, so the cpp-mirror `started(...)`
    // packet arrives BEFORE any wire-rx packet on every select
    // iteration.
    let first = tokio::time::timeout(Duration::from_secs(2), harness.outbound_rx.recv())
        .await
        .expect("first outbound packet did not arrive within 2s")
        .expect("outbound channel closed");
    assert_eq!(
        first.packet_type, "kdeconnect.shareinputdevices.request",
        "first packet after Activated must be the activation announcement; got body={}",
        first.body
    );
    assert_eq!(
        first.body,
        serde_json::json!({
            "exitEdge": 2, // Edge::Left
            "deltax": 50.0,
            "deltay": 100.0,
        }),
        "shareinputdevices.request body must match cursor_position minus barrier.p1"
    );

    // Oracle 2b-i: motion body. `plan_motion(5.0, 7.0)` produces
    // `{"dx": 5.0, "dy": 7.0}`. The dx/dy values MUST match what
    // the fake peer sent through the reis high-level emitter
    // (drift would indicate the float coercion dropped bits).
    let motion = tokio::time::timeout(Duration::from_secs(2), harness.outbound_rx.recv())
        .await
        .expect("motion body did not arrive within 2s")
        .expect("outbound channel closed");
    assert_eq!(
        motion.packet_type, "kdeconnect.mousepad.request",
        "second packet must be a mousepad.request; got {}",
        motion.packet_type
    );
    assert_eq!(
        motion.body,
        serde_json::json!({ "dx": 5.0, "dy": 7.0 }),
        "motion body must match `plan_motion(5.0, 7.0)` exactly; got {}",
        motion.body
    );

    // Oracle 2b-ii: button press body. `plan_button(Left, Press)`
    // is `{singlehold: true}` (mod.rs:207). This pins that the
    // drain-side `plan_button(...)` ran (not skipped via the
    // `is_null()` guard at handle_activated:777).
    let press = tokio::time::timeout(Duration::from_secs(2), harness.outbound_rx.recv())
        .await
        .expect("button press body did not arrive within 2s")
        .expect("outbound channel closed");
    assert_eq!(
        press.packet_type, "kdeconnect.mousepad.request",
        "third packet must be a mousepad.request; got {}",
        press.packet_type
    );
    assert_eq!(
        press.body,
        serde_json::json!({ "singlehold": true }),
        "BTN_LEFT press must produce `singlehold: true` (mod.rs:207); got {}",
        press.body
    );

    // Oracle 2b-iii: button release body. `plan_button(Left,
    // Release)` is `{singlerelease: true}` (mod.rs:208). This
    // pins the queued-event drain order — release arrived LAST in
    // the fake peer, must surface LAST on the wire.
    let release = tokio::time::timeout(Duration::from_secs(2), harness.outbound_rx.recv())
        .await
        .expect("button release body did not arrive within 2s")
        .expect("outbound channel closed");
    assert_eq!(
        release.packet_type, "kdeconnect.mousepad.request",
        "fourth packet must be a mousepad.request; got {}",
        release.packet_type
    );
    assert_eq!(
        release.body,
        serde_json::json!({ "singlerelease": true }),
        "BTN_LEFT release must produce `singlerelease: true` (mod.rs:208); got {}",
        release.body
    );

    // Oracle 3: nothing further. The drain emptied the queue
    // exactly once; spurious packets would mean either the gate
    // re-armed unexpectedly or the consumer kept emitting after
    // its sources went silent.
    let spurious =
        tokio::time::timeout(Duration::from_millis(200), harness.outbound_rx.recv()).await;
    assert!(
        spurious.is_err() || spurious.as_ref().is_ok_and(|v| v.is_none()),
        "no further packets expected after the four drained bodies; got {:?}",
        spurious
            .as_ref()
            .ok()
            .and_then(|o| o.as_ref())
            .map(|p| (p.packet_type.clone(), p.body.clone()))
    );
}

/// M4 wiring test 4: D-Bus `Activated` arriving BEFORE the EI
/// receiver is populated must still drain the activation gate
/// when the populate call lands. The bug shape (pre-fix): the
/// signal handler ran in the empty slot, found no receiver, logged
/// "drain deferred", and dropped the activation_id on the floor —
/// the gate.activation_id stayed at 0 and queued EI input was
/// never replayed for that cycle. The fix shape: the signal
/// handler stores the deferred id on `PortalSession`; the
/// `populate_ei_receiver` call replays `handle_activated(id)` on
/// the receiver immediately after the Arc lands.
///
/// **How this test is deterministic.** The fake portal's
/// `Enable` handler is wired (via `FakePortalState::
/// emit_activated_in_enable`) to emit an `Activated` signal
/// BEFORE returning. With the match rule registered before
/// `Enable` (panel 66ae8992), the signal reaches the producer's
/// own connection while `PortalSession::start` is still inside
/// `Enable`. The signal handler is spawned AFTER `Enable`
/// returns but during `start()`, so by the time `start()` yields
/// `Self`, the signal handler may or may not have read the
/// signal — we deterministically drive the determinism barrier
/// by polling `activated_rx` for the corresponding `Activated`
/// event (step 1 of the signal handler — the sync `send`) BEFORE
/// we call `populate_ei_receiver`. At that moment the slot was
/// empty, so the handler's step 2 hit the deferred branch.
///
/// **What this pins:**
/// - `populate_ei_receiver` is async; awaiting it lands the
///   deferred id onto the receiver's gate.
/// - `EiReceiver::gate_activation_id_for_test()` reports the
///   replayed id — i.e. the gate was drained despite the slot
///   being empty at the moment `Activated` arrived.
/// - The `activated_tx` channel still receives the consumer-side
///   event regardless — that path is independent of the slot.
#[tokio::test(flavor = "multi_thread")]
async fn m4_activated_before_populate_is_replayed_on_populate() {
    use rust_connect::plugins::shareinputdevices::ei::EiReceiver;
    use rust_connect::plugins::shareinputdevices::portal::{ActivatedEvent, PortalSession};

    let _bus_lock = BUS.lock().await;

    // Install the socketpair fd BEFORE the fake is set up.
    let (client_stream, peer_stream) = UnixStream::pair().expect("UnixStream::pair");
    let connect_to_eis_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(client_stream.into_raw_fd()) };

    // The session runs at (0, 0)..(1920, 1080) with Edge::Left →
    // barrier p1 (0, 0). cursor_position (50, 100) yields
    // deltax = 50, deltay = 100 verbatim.
    let session_handle = "/org/freedesktop/portal/desktop/session/m4_replay".to_string();
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: session_handle.clone(),
        zones: vec![(1920, 1080, 0, 0)],
        connect_to_eis_socketpair: Some(connect_to_eis_fd),
        emit_activated_in_enable: Some((42, 50.0, 100.0)),
        ..Default::default()
    }));

    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };

    // The other half of the socketpair goes unused for this test —
    // `EiReceiver::new` only `set_nonblocking`s the fd and stores
    // it; no handshake or pump I/O runs here. Closing the peer
    // end immediately is safe. (A real production wire keeps
    // this end alive for the EIS peer's lifetime; the rest of the
    // wiring is exercised by `m4_input_relays_through_gate_and_
    // consumer`.)
    drop(peer_stream);

    // Build the PortalSession. The fake's Enable emits Activated
    // BEFORE returning; the signal handler task is spawned inside
    // `start()` and will read the signal asynchronously. We then
    // POLL `activated_rx` to deterministically wait for step 1
    // (the sync `activated_tx.send`) to land — at that moment the
    // slot was empty, so the handler's step 2 hit the deferred
    // branch and (post-fix) the id is stored on `PortalSession`.
    let (activated_tx, mut activated_rx) = tokio::sync::mpsc::unbounded_channel::<ActivatedEvent>();
    let conn = zbus::Connection::session().await.unwrap();
    let mut session = tokio::time::timeout(
        Duration::from_secs(5),
        PortalSession::start(conn, Edge::Left, activated_tx),
    )
    .await
    .expect("PortalSession::start timed out")
    .expect("PortalSession::start must succeed");

    let activated_event = tokio::time::timeout(Duration::from_secs(2), activated_rx.recv())
        .await
        .expect("signal handler did not deliver ActivatedEvent within 2s")
        .expect("activated_rx closed before producing an event");
    assert_eq!(
        activated_event.activation_id, 42,
        "consumer-side Activated event must carry the id the fake emitted in Enable"
    );

    // Build the EI receiver from the fd ConnectToEIS handed back.
    // `EiReceiver::new` is sync and does NOT start the pump — the
    // pump would normally start on the dedicated drive thread, but
    // this test does not care about pump events (the gate is
    // empty); we only need a valid receiver to populate.
    let ei_fd = session.take_ei_fd();
    let receiver = EiReceiver::new(ei_fd, "shareinputdevices-m4-replay-test")
        .expect("EiReceiver::new must succeed against the socketpair");

    // Pre-fix: this just stored the receiver in the slot. The
    // deferred id was lost; gate.activation_id stays 0.
    // Post-fix: the populate replays handle_activated(42) on the
    // receiver; gate.activation_id becomes 42.
    session.populate_ei_receiver(Arc::clone(&receiver)).await;

    assert_eq!(
        receiver.gate_activation_id_for_test().await,
        42,
        "populate_ei_receiver must replay the deferred Activated id onto the receiver's gate"
    );
}

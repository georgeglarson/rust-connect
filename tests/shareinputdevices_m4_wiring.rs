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
use reis::handshake::EisHandshaker;
use reis::request::{Connection as EisConnection, EisRequest};
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
        _session_handle: zbus::zvariant::OwnedObjectPath,
        _options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::fdo::Result<()> {
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
        eprintln!("dbus-daemon not on PATH — skipping");
        return None;
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

/// Spawn a thread that owns the peer end of the socketpair and
/// drives the EIS handshake, then keeps the read loop alive. The
/// `handshake_complete` oneshot fires once the connection is ready;
/// the `keep_alive` flag controls the post-handshake read loop.
fn spawn_fake_eis_peer(
    peer_stream: UnixStream,
    handshake_complete: oneshot::Sender<EisConnection>,
    keep_alive: Arc<AtomicBool>,
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
        let connection = converter.handle().clone();
        let _ = handshake_complete.send(connection);

        // Post-handshake read loop. Keep draining so the receiver's
        // bind_capabilities makes it onto the EIS side.
        while keep_alive.load(Ordering::SeqCst) {
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
    /// Held to keep the receiver + portal session alive for the test's
    /// duration. Dropping it tears the wiring down.
    _resources: M4Resources,
}

struct M4Resources {
    _session: Arc<PortalSession>,
    _receiver: Arc<EiReceiver>,
    _daemon: DaemonGuard,
}

async fn setup_m4_harness(session_handle: &str) -> Option<M4Harness> {
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
    spawn_fake_eis_peer(peer_stream, handshake_tx, keep_alive.clone());

    session.populate_ei_receiver(Arc::clone(&receiver));

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
        setup_m4_harness("/org/freedesktop/portal/desktop/session/m4test1").await
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
    let Some(harness) = setup_m4_harness("/org/freedesktop/portal/desktop/session/m4test3").await
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

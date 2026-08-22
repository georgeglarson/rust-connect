//! Integration tests for the peer-gated activation lane (Task
//! #1042 fix lane B, panel M4 round 1) — the capability gate drives
//! activation/deactivation in response to peer connect/disconnect
//! events emitted by the device-event broadcaster.
//!
//! **What this lane is.** Pre-fix, `enable_session_backend()`
//! probed and stashed the portal connection, then ran the full v1
//! session sequence on the spot. That trapped the cursor on a
//! peerless desktop — no consumer to drain the wire stream.
//! The fix splits the boot path from the activation path:
//! `enable_session_backend()` probes + stashes only; the
//! capability gate (subscribed to the device-event broadcaster)
//! drives `activate_portal_session()` on the FIRST capable peer
//! connect and `deactivate_portal_session()` on the LAST capable
//! peer disconnect.
//!
//! **The seam this lane uses.** No new registry callback was
//! needed: the per-peer capability map
//! (`ConnectionManager::peer_capabilities`) and the per-device
//! lifecycle broadcaster were both already present.
//! `record_peer_capabilities` runs BEFORE the `Connected`
//! transition broadcasts (`inbound.rs:175`, `outbound.rs:321`), so
//! by the time the gate sees a `StateChanged{Connected}` event,
//! the peer's caps are live in the map.
//!
//! **Test shape.** A private `dbus-daemon` hosts the same
//! `FakePortal` the boot-attach harness uses; the plugin's
//! `ConnectionManager` is real (built from a temp
//! `CertificateManager`) and uses the test-only
//! `mark_fake_connected_for_test` to declare peers "connected"
//! without standing up a real TLS handshake. The device-event
//! broadcaster is a real `EventBroadcaster` and is driven by the
//! test; the gate's spawned task receives the events through the
//! real subscription path. The fan-out filter test does NOT
//! require a portal — it asserts on
//! `capable_consumer_ids()` directly, which is the single
//! invariant the wire consumer reads on every packet.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reis::eis::{self};
use reis::handshake::EisHandshaker;

use rust_connect::device::types::{DeviceEvent, DeviceState};
use rust_connect::device::EventBroadcaster;
use rust_connect::plugins::shareinputdevices::ShareInputDevicesPlugin;
use rust_connect::plugins::Plugin;
use rust_connect::protocol::CertificateManager;
use rust_connect::protocol::ConnectionManager;

use zbus::zvariant::OwnedValue;

// ============ Private-bus plumbing (mirrors shareinputdevices_m4_wiring) ============

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH_STR: &str = "/org/freedesktop/portal/desktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

static BUS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct DaemonGuard(Option<Child>);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_daemon(socket_path: &std::path::Path) -> Child {
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

// ============ Fake portal with call ledger ============

#[derive(Debug, Clone)]
#[allow(dead_code)]
#[allow(clippy::large_enum_variant)]
enum Call {
    CreateSession {
        capabilities: Option<u32>,
    },
    ConnectToEIS {
        session_handle: String,
    },
    GetZones {
        session_handle: String,
    },
    SetPointerBarriers {
        barrier_id: Option<u32>,
        position: Option<Vec<i32>>,
        zone_set: u32,
    },
    Enable {
        session_handle: String,
    },
}

#[derive(Default)]
struct FakePortalState {
    pub version: u32,
    pub supported_caps: u32,
    pub calls: Vec<Call>,
    pub request_id: Arc<AtomicU32>,
    pub session_handle: String,
    pub conn: Option<zbus::Connection>,
    pub zones: Vec<(u32, u32, i32, i32)>,
    /// Socketpair fds handed back from ConnectToEIS. The fake
    /// portal pops one per ConnectToEIS call; tests that drive
    /// multiple activations push one fd per activation upfront
    /// (a single socketpair can only carry one EIS handshake —
    /// the receiver takes ownership of its end and the kernel
    /// shuts the stream when either side drops). The
    /// disconnect/reconnect test installs two so the second
    /// activation's `EiReceiver::new` doesn't end up reading
    /// from /dev/null.
    pub connect_to_eis_socketpairs: std::collections::VecDeque<OwnedFd>,
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
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        let capabilities = options
            .get("capabilities")
            .and_then(|v| u32::try_from(v.clone()).ok());
        let (session_handle, request_path, conn_for_signal) = {
            let mut guard = self.state.lock().unwrap();
            let session_handle = guard.session_handle.clone();
            guard.calls.push(Call::CreateSession { capabilities });
            let id = guard.request_id.fetch_add(1, Ordering::SeqCst);
            let request_path = format!("/org/freedesktop/portal/desktop/request/{id}");
            let conn_for_signal = guard.conn.as_ref().expect("conn").clone();
            (session_handle, request_path, conn_for_signal)
        };

        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert(
            "session_handle".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::ObjectPath(
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
        session_handle: zbus::zvariant::OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::Fd<'static>> {
        let sh_str = session_handle.to_string();
        let socketpair_handoff = {
            let mut guard = self.state.lock().unwrap();
            guard.calls.push(Call::ConnectToEIS {
                session_handle: sh_str,
            });
            guard.connect_to_eis_socketpairs.pop_front()
        };
        if let Some(owned) = socketpair_handoff {
            Ok(zbus::zvariant::Fd::from(owned))
        } else {
            let f = std::fs::File::open("/dev/null").expect("/dev/null");
            let owned = std::os::fd::OwnedFd::from(f);
            Ok(zbus::zvariant::Fd::from(owned))
        }
    }

    #[zbus(name = "GetZones")]
    async fn get_zones(
        &self,
        session_handle: zbus::zvariant::OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        let sh_str = session_handle.to_string();
        let (request_path, conn_for_signal) = {
            let mut guard = self.state.lock().unwrap();
            guard.calls.push(Call::GetZones {
                session_handle: sh_str,
            });
            let id = guard.request_id.fetch_add(1, Ordering::SeqCst);
            let request_path = format!("/org/freedesktop/portal/desktop/request/{id}");
            let conn_for_signal = guard.conn.as_ref().expect("conn").clone();
            (request_path, conn_for_signal)
        };

        let zones = self.state.lock().unwrap().zones.clone();
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert(
            "zones".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::Array(zbus::zvariant::Array::from(
                zones,
            )))
            .expect("zones OwnedValue"),
        );
        results.insert(
            "zone_set".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::U32(0)).expect("zone_set OwnedValue"),
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
        _options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        let (barrier_id, position) = barriers
            .first()
            .map(|entry| {
                let id = entry
                    .get("barrier_id")
                    .and_then(|v| u32::try_from(v.clone()).ok());
                let pos = entry.get("position").and_then(|v| match &**v {
                    zbus::zvariant::Value::Array(arr) => {
                        let mut nums = Vec::new();
                        for item in arr.iter() {
                            match item {
                                zbus::zvariant::Value::I32(n) => nums.push(*n),
                                _ => return None,
                            }
                        }
                        (nums.len() == 4).then_some(nums)
                    }
                    _ => None,
                });
                (id, pos)
            })
            .unwrap_or((None, None));
        let (request_path, conn_for_signal) = {
            let mut guard = self.state.lock().unwrap();
            guard.calls.push(Call::SetPointerBarriers {
                barrier_id,
                position,
                zone_set,
            });
            let id = guard.request_id.fetch_add(1, Ordering::SeqCst);
            let request_path = format!("/org/freedesktop/portal/desktop/request/{id}");
            let conn_for_signal = guard.conn.as_ref().expect("conn").clone();
            (request_path, conn_for_signal)
        };
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert(
            "failed_barriers".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::Array(zbus::zvariant::Array::from(
                Vec::<u32>::new(),
            )))
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
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        self.state.lock().unwrap().calls.push(Call::Enable {
            session_handle: session_handle.to_string(),
        });
        Ok(())
    }
}

async fn emit_response_signal(
    conn: &zbus::Connection,
    request_path: &str,
    code: u32,
    results: HashMap<String, OwnedValue>,
) {
    let body: (u32, HashMap<String, OwnedValue>) = (code, results);
    conn.emit_signal(None::<&str>, request_path, REQUEST_IFACE, "Response", &body)
        .await
        .expect("emit Response signal");
}

async fn setup(state: Arc<Mutex<FakePortalState>>) -> Option<DaemonGuard> {
    if Command::new("dbus-daemon")
        .arg("--version")
        .output()
        .is_err()
    {
        panic!(
            "shareinputdevices integration tests require `dbus-daemon` on PATH \
             (install dbus-daemon to run these tests)"
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

// ============ Minimal fake EIS peer ============

/// Spawn a thread that completes the EIS handshake against the
/// socketpair's peer end, then parks with the EIS context alive.
/// The receiver's `start()` finishes its handshake once HELLO +
/// FINISH round-trip; the peer's role is to be ready on the
/// other side of that exchange. After the handshake the peer
/// keeps the EIS context alive so the receiver's pump does NOT
/// see EOF (which would fire the disconnect arm and flip
/// `backend_available=false` while the test still asserts on
/// it). The thread returns after `keep_alive` flips false, or
/// silently holds until the JoinHandle is dropped — whichever
/// the test prefers.
fn spawn_minimal_eis_peer(
    peer_stream: UnixStream,
    keep_alive: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let eis_ctx = eis::Context::new(peer_stream).expect("eis Context::new");
        let handshaker = std::sync::Mutex::new(EisHandshaker::new(&eis_ctx, 1));
        loop {
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
            if let Some(_r) = got {
                eis_ctx.flush().expect("flush handshake");
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Hold the EIS context alive across the test's
        // assertions; the keep_alive flag lets the test
        // deterministically drop this thread when it wants to
        // observe the disconnect path.
        while keep_alive.load(Ordering::SeqCst) {
            let _ = eis_ctx.read();
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(eis_ctx);
    })
}

// ============ Harness ============

/// What a peer-gated activation test needs on hand. The portal
/// half comes from the M4 harness (FakePortal on a private
/// dbus-daemon); the capability-gate half is driven directly by
/// the test through `mark_fake_connected_for_test` +
/// `record_peer_capabilities` + broadcaster broadcasts.
struct Harness {
    plugin: ShareInputDevicesPlugin,
    cm: Arc<ConnectionManager>,
    broadcaster: Arc<EventBroadcaster>,
    fake_state: Arc<Mutex<FakePortalState>>,
    /// Kept alive for the harness lifetime. Dropping it kills the
    /// private dbus-daemon and unhooks the FakePortal's bus
    /// name; without it, every `zbus::Connection::session()` call
    /// after the daemon exits gets `Connection refused`.
    _daemon: DaemonGuard,
    /// Held to keep the minimal EIS peer alive across the
    /// test's `backend_available()` assertion — without it, the
    /// receiver's pump sees EOF and the disconnect watcher flips
    /// `backend_available=false`. The flag lets the
    /// disconnect-and-reconnect test deterministically retire
    /// the peer for its disconnect phase.
    #[allow(dead_code)]
    eis_keep_alive: Arc<AtomicBool>,
    _eis_peer: Option<std::thread::JoinHandle<()>>,
}

async fn build_harness(session_handle: &str, socketpair_count: usize) -> Harness {
    // Install `socketpair_count` socketpairs; the fake portal
    // hands one back per ConnectToEIS call. Each pair has its
    // own peer thread because once `EiReceiver::new` takes its
    // end of the pair, the receiver OWNS the fd — sharing a
    // socketpair across two activations is impossible (the
    // kernel sees the first receiver reading from its end and
    // the second would read /dev/null after the fake's
    // `take()` ran out). One shared keep_alive flag is fine
    // because all peer threads retire together when the test
    // sets it false (only the disconnect/reconnect test uses
    // that path; the single-activation tests just keep it
    // alive for the run).
    let mut connect_to_eis_fds: std::collections::VecDeque<OwnedFd> =
        std::collections::VecDeque::new();
    let mut peer_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let shared_keep_alive = Arc::new(AtomicBool::new(true));

    for _ in 0..socketpair_count {
        let (client_stream, peer_stream) = UnixStream::pair().expect("UnixStream::pair");
        let join = spawn_minimal_eis_peer(peer_stream, shared_keep_alive.clone());
        peer_threads.push(join);
        connect_to_eis_fds.push_back(unsafe { OwnedFd::from_raw_fd(client_stream.into_raw_fd()) });
    }

    let fake_state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: session_handle.to_string(),
        zones: vec![(1920, 1080, 0, 0)],
        connect_to_eis_socketpairs: connect_to_eis_fds,
        ..Default::default()
    }));

    let daemon_guard = setup(fake_state.clone()).await.expect("setup");

    let cert_dir = tempfile::tempdir().expect("cert tempdir");
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.path().to_path_buf()));
    let cm = Arc::new(ConnectionManager::new(cert_manager).expect("ConnectionManager::new"));

    let broadcaster = Arc::new(EventBroadcaster::new(64, "device"));

    let plugin = ShareInputDevicesPlugin::new()
        .with_connection_manager(cm.clone())
        .with_event_broadcaster(broadcaster.clone());

    // Single-peer-thread case: keep the harness API the same
    // shape (eis_keep_alive + single join handle). Tests that
    // need more than one peer thread can ignore the unused
    // extras — they sit alive in their keep_alive loop and the
    // OS reclaims them when the test process exits.
    let (eis_keep_alive, eis_peer) = match peer_threads.into_iter().next() {
        Some(jh) => (shared_keep_alive, Some(jh)),
        None => (Arc::new(AtomicBool::new(false)), None),
    };

    Harness {
        plugin,
        cm,
        broadcaster,
        fake_state,
        _daemon: daemon_guard,
        eis_keep_alive,
        _eis_peer: eis_peer,
    }
}

/// Mark `device_id` as a fake-connected capable peer: record its
/// incoming caps + mark it as live in the shadow set. Used by the
/// gate's `capable_consumer_ids` and `is_connected` checks.
async fn make_capable_peer(cm: &ConnectionManager, device_id: &str) {
    cm.record_peer_capabilities(
        &device_id.to_string(),
        &[
            "kdeconnect.shareinputdevices.request".to_string(),
            "kdeconnect.mousepad.request".to_string(),
        ],
        &["kdeconnect.ping".to_string()],
    )
    .await;
    cm.mark_fake_connected_for_test(device_id);
}

async fn drop_capable_peer(cm: &ConnectionManager, device_id: &str) {
    cm.unmark_fake_connected_for_test(device_id);
}

/// Wait up to `deadline` for the fake portal's call ledger to
/// match the predicate. Used to bridge the test's race against
/// the gate's tokio task.
async fn wait_for_calls<F: Fn(&[Call]) -> bool>(
    fake_state: &Arc<Mutex<FakePortalState>>,
    pred: F,
    timeout: Duration,
) -> Vec<Call> {
    let deadline = Instant::now() + timeout;
    loop {
        let calls = fake_state.lock().unwrap().calls.clone();
        if pred(&calls) {
            return calls;
        }
        if Instant::now() >= deadline {
            panic!(
                "v1 sequence predicate did not satisfy within {:?}; calls so far: {:?}",
                timeout, calls
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn call_kinds(calls: &[Call]) -> Vec<&'static str> {
    calls
        .iter()
        .map(|c| match c {
            Call::CreateSession { .. } => "CreateSession",
            Call::ConnectToEIS { .. } => "ConnectToEIS",
            Call::GetZones { .. } => "GetZones",
            Call::SetPointerBarriers { .. } => "SetPointerBarriers",
            Call::Enable { .. } => "Enable",
        })
        .collect()
}

fn broadcast_connected(broadcaster: &EventBroadcaster, device_id: &str) {
    broadcaster.broadcast(DeviceEvent::StateChanged {
        device_id: device_id.to_string(),
        old_state: DeviceState::Paired,
        new_state: DeviceState::Connected,
    });
}

fn broadcast_disconnected(broadcaster: &EventBroadcaster, device_id: &str) {
    broadcaster.broadcast(DeviceEvent::StateChanged {
        device_id: device_id.to_string(),
        old_state: DeviceState::Connected,
        new_state: DeviceState::Disconnected,
    });
}

// ============ TESTS ============

/// **Capable peer connect drives the full v1 sequence.** The
/// boot path (`enable_session_backend`) only probes + stashes —
/// it does NOT activate. The activation gate waits for a
/// `StateChanged{Connected}` event whose device advertises
/// `kdeconnect.shareinputdevices.request` (or
/// `kdeconnect.mousepad.request`); on that event, the gate calls
/// `activate_portal_session` which drives CreateSession →
/// ConnectToEIS → GetZones → SetPointerBarriers → Enable.
///
/// This pins the lane's headline behavior: capability-gated
/// activation, not boot-time activation.
#[tokio::test(flavor = "multi_thread")]
async fn capable_peer_connect_drives_v1_sequence() {
    let _bus_lock = BUS.lock().await;

    let h = build_harness("/org/freedesktop/portal/desktop/session/peer_gate_on", 1).await;

    // Boot path. Probes + stashes + spawns the gate; the fake
    // portal sees NO CreateSession yet.
    h.plugin.enable_session_backend().await;
    // Sanity: the boot path alone does not drive v1.
    let boot_calls = h.fake_state.lock().unwrap().calls.clone();
    assert!(
        boot_calls.is_empty(),
        "boot path must not drive v1; saw calls: {:?}",
        boot_calls
    );
    assert!(
        !h.plugin.portal_backend_available(),
        "backend_available must stay false after the boot path alone"
    );

    // The capable peer arrives. Record its caps + mark it as
    // fake-connected, then broadcast the StateChanged. The
    // gate's subscription reads the event, evaluates, and
    // activates.
    let device_id = "peer-capable";
    make_capable_peer(&h.cm, device_id).await;
    broadcast_connected(&h.broadcaster, device_id);

    // Wait for the v1 sequence to land on the fake's ledger.
    let calls = wait_for_calls(
        &h.fake_state,
        |calls| calls.iter().any(|c| matches!(c, Call::Enable { .. })),
        Duration::from_secs(10),
    )
    .await;

    let kinds = call_kinds(&calls);
    assert_eq!(
        kinds,
        vec![
            "CreateSession",
            "ConnectToEIS",
            "GetZones",
            "SetPointerBarriers",
            "Enable"
        ],
        "first capable peer connect must drive the full v1 sequence; saw {:?}",
        kinds
    );

    // Wait for `backend_available` to flip — `do_activate`
    // populates it AFTER `PortalSession::start` returns (the
    // EI pump must deliver its oneshot receivers first). The
    // v1 calls land on the fake's ledger synchronously inside
    // `PortalSession::start`, so the sequence check above is
    // tight; this poll bridges the gap to the EI-pump
    // readiness check.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if h.plugin.portal_backend_available() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Capability honesty after activation.
    assert!(
        h.plugin.portal_backend_available(),
        "backend_available must flip true after activation"
    );
    assert_eq!(
        h.plugin.outgoing_capabilities(),
        vec!["kdeconnect.shareinputdevices.request".to_string()],
        "outgoing capability must advertise after activation"
    );
}

/// **Disconnect closes the session + flips backend_available
/// false; reconnect re-activates.** The peer-gated contract:
/// the session is opened lazily on the first capable peer and
/// closed when the last capable peer leaves. The plugin must
/// re-arm and re-activate cleanly when a new capable peer
/// arrives.
#[tokio::test(flavor = "multi_thread")]
async fn last_capable_peer_disconnect_closes_and_reconnect_reactivates() {
    let _bus_lock = BUS.lock().await;

    let h = build_harness("/org/freedesktop/portal/desktop/session/peer_gate_off", 2).await;
    h.plugin.enable_session_backend().await;

    // First capable peer arrives and activates.
    let first = "peer-first";
    make_capable_peer(&h.cm, first).await;
    broadcast_connected(&h.broadcaster, first);
    wait_for_calls(
        &h.fake_state,
        |calls| calls.iter().any(|c| matches!(c, Call::Enable { .. })),
        Duration::from_secs(10),
    )
    .await;
    assert!(h.plugin.portal_backend_available());

    // Last capable peer leaves: unmark + broadcast Disconnected.
    // The gate's subscription sees the non-Connected transition,
    // re-snapshots the consumer set (empty), and deactivates.
    drop_capable_peer(&h.cm, first).await;
    broadcast_disconnected(&h.broadcaster, first);

    // The deactivate path drops the session Arc + flips
    // backend_available=false. Poll the plugin's flag with a
    // generous bound; the gate's tokio task may need a tick to
    // reach the deactivate arm.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !h.plugin.portal_backend_available() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !h.plugin.portal_backend_available(),
        "backend_available must flip false after the last capable peer disconnects"
    );

    // The session slot must be empty after deactivation. The
    // plugin's `portal_session` is private, but the gate's
    // re-activation path only fires if no session is live; we
    // prove that by connecting a fresh capable peer and
    // observing a SECOND v1 sequence on the ledger.
    let initial_call_count = h.fake_state.lock().unwrap().calls.len();
    let second = "peer-second";
    make_capable_peer(&h.cm, second).await;
    broadcast_connected(&h.broadcaster, second);

    // Wait until the fake sees a SECOND CreateSession call —
    // that is the unambiguous signal of re-activation.
    wait_for_calls(
        &h.fake_state,
        |calls| {
            calls
                .iter()
                .filter(|c| matches!(c, Call::CreateSession { .. }))
                .count()
                >= 2
        },
        Duration::from_secs(10),
    )
    .await;

    let post_disconnect_calls = h.fake_state.lock().unwrap().calls.clone();
    assert!(
        post_disconnect_calls.len() > initial_call_count,
        "second capable peer must drive a fresh v1 sequence; \
         ledger before={}, after={}",
        initial_call_count,
        post_disconnect_calls.len(),
    );

    // Wait for the second activation to land end-to-end. The v1
    // calls themselves run inside `PortalSession::start` (so the
    // ledger updates synchronously), but `backend_available.store(true)`
    // runs only AFTER the EI pump's oneshot receivers deliver
    // (the slot check + pump thread spawn + oneshot await). Poll
    // the flag with a generous bound; the disconnect→reconnect
    // path also requires `do_deactivate` to have run for the
    // slot to be empty before the second `do_activate` can succeed.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if h.plugin.portal_backend_available() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        h.plugin.portal_backend_available(),
        "backend_available must flip true again on reconnect"
    );
}

/// **Fan-out filter.** `capable_consumer_ids` returns the
/// intersection of `is_connected` and "advertises at least one
/// of CONSUMER_INCOMING_CAPS as an incoming capability". A peer
/// that's connected but does NOT advertise the cap is filtered
/// out. This is the single invariant the unified wire consumer
/// reads on every packet (mod.rs:1283-1286) — testing it directly
/// pins the filter behavior the brief mandates.
///
/// `mark_fake_connected_for_test` and `unmark_fake_connected_for_test`
/// are test-only accessors gated on cfg(test) / feature =
/// "test-helpers"; this test is the canonical proof that they
/// drive the gate's view of "connected".
#[tokio::test(flavor = "multi_thread")]
async fn fan_out_filter_only_targets_capable_peers() {
    use rust_connect::plugins::shareinputdevices::CONSUMER_INCOMING_CAPS;

    let cert_dir = tempfile::tempdir().expect("cert tempdir");
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.path().to_path_buf()));
    let cm = Arc::new(ConnectionManager::new(cert_manager).expect("ConnectionManager::new"));

    // Two fake peers. "capable-a" advertises the consumer cap;
    // "passive-b" is connected but offers NO consumer caps.
    let capable = "capable-a";
    let passive = "passive-b";

    cm.record_peer_capabilities(
        &capable.to_string(),
        &[
            "kdeconnect.shareinputdevices.request".to_string(),
            "kdeconnect.mousepad.request".to_string(),
        ],
        &["kdeconnect.ping".to_string()],
    )
    .await;
    cm.record_peer_capabilities(
        &passive.to_string(),
        &["kdeconnect.battery".to_string()], // NOT a consumer cap
        &["kdeconnect.ping".to_string()],
    )
    .await;
    cm.mark_fake_connected_for_test(capable);
    cm.mark_fake_connected_for_test(passive);

    // The fan-out filter's contract: only the capable peer.
    let mut fanout = cm.capable_consumer_ids(CONSUMER_INCOMING_CAPS).await;
    fanout.sort();
    assert_eq!(
        fanout,
        vec![capable.to_string()],
        "capable_consumer_ids must filter out non-capable peers; saw {:?}",
        fanout
    );

    // The opposite: a peer with NO connection (but with caps in
    // the map — the lingering-entry shape) is filtered out too.
    cm.unmark_fake_connected_for_test(passive);
    cm.unmark_fake_connected_for_test(capable);
    cm.record_peer_capabilities(
        &capable.to_string(),
        &[
            "kdeconnect.shareinputdevices.request".to_string(),
            "kdeconnect.mousepad.request".to_string(),
        ],
        &["kdeconnect.ping".to_string()],
    )
    .await;
    let fanout_disconnected = cm.capable_consumer_ids(CONSUMER_INCOMING_CAPS).await;
    assert!(
        fanout_disconnected.is_empty(),
        "a peer with caps but no connection must not be a consumer candidate; saw {:?}",
        fanout_disconnected
    );

    // Re-mark the capable peer; filter must include it again,
    // demonstrating `mark_fake_connected_for_test` is a live
    // addition not a snapshot.
    cm.mark_fake_connected_for_test(capable);
    let fanout_reconnected = cm.capable_consumer_ids(CONSUMER_INCOMING_CAPS).await;
    assert_eq!(
        fanout_reconnected,
        vec![capable.to_string()],
        "re-marking the capable peer must restore it to the fan-out set"
    );

    // And `is_connected` agrees: capable is live, passive is
    // not (and never comes back unless re-marked).
    assert!(cm.is_connected(&capable.to_string()).await);
    assert!(!cm.is_connected(&passive.to_string()).await);
}

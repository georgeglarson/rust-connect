//! Integration test for the M4-wire boot-time activation hookup
//! (Task #1042) — proves that calling `enable_session_backend()`
//! against a passed probe drives the full v1 session sequence
//! (CreateSession → ConnectToEIS → GetZones → SetPointerBarriers →
//! Enable) WITHOUT a manual `activate_portal_session()` call.
//!
//! **The contract this pins.** Before this lane,
//! `enable_session_backend` probed the portal, stashed the
//! connection, and STOPPED — the producer stayed INERT. The M3 EI
//! transport attaching was supposed to be the trigger. That trigger
//! never came, so the plugin's capability never advertised. This
//! test pins the lane's fix: the boot path now drives the full v1
//! sequence in one shot, mirroring the cpp upstream where the
//! plugin's `enable()` slot starts the InputCapture session.
//!
//! **The seam it uses.** `enable_session_backend` opens
//! `zbus::Connection::session()` itself (mirroring
//! screensaver_inhibit / mpris / pausemusic), and zbus respects
//! the process-level `DBUS_SESSION_BUS_ADDRESS` env var. The test
//! spins up a private dbus-daemon on a unix socket, sets the env
//! var to point at it, and serves a `FakePortal` on
//! `org.freedesktop.portal.Desktop`. With that env var in place,
//! the production code path is end-to-end: the probe, the v1
//! sequence, and the EI receiver wiring, all against the fake.
//!
//! **The fake EIS peer.** `activate_portal_session` constructs an
//! `EiReceiver` from the fd ConnectToEIS returned, then spawns a
//! dedicated thread that calls `receiver.start()` — which does the
//! EIS handshake on the socketpair fd. Without a peer the handshake
//! hangs; the test installs a unix socketpair and a minimal fake
//! peer thread that completes the handshake and exits, letting the
//! drive future see EOF and fire the disconnect arm. We don't
//! drive input events — the test's only interest is the v1 call
//! sequence on the fake's call ledger.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reis::eis::{self};
use reis::handshake::EisHandshaker;
use zbus::zvariant::OwnedValue;

use rust_connect::plugins::shareinputdevices::ShareInputDevicesPlugin;
use rust_connect::plugins::Plugin;

// ============ Private-bus plumbing (mirrors shareinputdevices_portal_lifecycle) ============

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH_STR: &str = "/org/freedesktop/portal/desktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

/// Process-wide serializing lock for the private dbus-daemon.
/// `DBUS_SESSION_BUS_ADDRESS` is process-level, so concurrent tests
/// would race the env var and each other's daemons. Acquired at the
/// top of every test, held until end-of-test. Tokio mutex because
/// the guard must be `Send` across the multi-thread runtime.
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

// ============ Fake portal with call ledger + socketpair handoff ============

/// Recorded call. The fake pushes one of these onto `state.calls`
/// per method invocation; the test asserts on the exact sequence
/// (CreateSession → ConnectToEIS → GetZones → SetPointerBarriers →
/// Enable).
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
    /// Connection the fake uses to emit Response signals inline.
    pub conn: Option<zbus::Connection>,
    /// The zones GetZones returns, as `(width, height, x, y)`. Empty
    /// would drive the abort path; one zone drives the full sequence.
    pub zones: Vec<(u32, u32, i32, i32)>,
    /// If set, the fake hands THIS fd back from ConnectToEIS
    /// instead of /dev/null. The boot-attach test installs one end
    /// of a UnixStream::pair() here so the production
    /// `activate_portal_session` constructs an `EiReceiver` against
    /// a real socket, not /dev/null. `take()`'d on the call → at
    /// most one ConnectToEIS per test.
    pub connect_to_eis_socketpair: Option<OwnedFd>,
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
            guard.connect_to_eis_socketpair.take()
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

    // Test-only leak to keep the connection + tempdir alive for
    // the daemon's lifetime.
    Box::leak(Box::new(conn));
    Box::leak(Box::new(tmp));

    Some(DaemonGuard(Some(daemon)))
}

// ============ Minimal fake EIS peer (handshake-only, no event traffic) ============

/// Spawn a thread that completes the EIS handshake against the
/// socketpair's peer end, then keeps the EIS context alive until
/// the optional `drop_tx` oneshot fires. **The peer MUST stay
/// alive across the test's `portal_backend_available()` assertion**
/// — pre-Fix-3, the previous version dropped the eis_ctx at the
/// end of the handshake loop, which closed the socketpair end and
/// caused the receiver's pump to fire the disconnect arm; the
/// disconnect watcher was already running by then, so
/// `backend_available` flipped to `false` between
/// `enable_session_backend()` returning and the test's poll. The
/// Fix-3 ordering narrows the race, but does not eliminate it —
/// the peer is the synchronization point. The peer thread owns
/// the EIS state for its lifetime (reis's `eis::Context` is
/// `!Send`).
fn spawn_minimal_eis_peer(
    peer_stream: UnixStream,
    drop_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let eis_ctx = eis::Context::new(peer_stream).expect("eis Context::new");
        let handshaker = std::sync::Mutex::new(EisHandshaker::new(&eis_ctx, 1));
        // Drive the handshake loop until the peer's `EisConnection`
        // handle is established (HELLO + FINISH exchanged).
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
        // Hold the EIS context alive until the test signals. We
        // never bind a seat/device because the test doesn't care
        // about input events — it only cares that the v1 portal
        // sequence ran to completion and that the capability
        // advertisement sticks during the assertion window.
        if let Some(tx) = drop_tx {
            let _ = tx.send(());
        }
        // Park until the channel closes (test drops its rx) — at
        // that point the socketpair end closes, the receiver's
        // pump sees EOF, and the disconnect arm fires. Tests that
        // don't care about the disconnect just let the function
        // return and the JoinHandle drop their interest.
        std::thread::sleep(Duration::from_secs(60));
        drop(eis_ctx);
    })
}

// ============ TESTS ============

/// The lane's load-bearing test. One call to
/// `enable_session_backend()` against a passed probe drives the
/// FULL v1 session sequence (CreateSession → ConnectToEIS →
/// GetZones → SetPointerBarriers → Enable) on the fake's call
/// ledger — with NO manual `activate_portal_session()` call. The
/// plugin's `portal_backend_available()` flips true and the
/// `portal_session` slot is populated.
///
/// **What this pins:**
/// - The brief's contract: boot-time activation after a passed
///   probe, mirroring the cpp upstream's plugin enable path.
/// - The order of v1 calls, with `ConnectToEIS` strictly before
///   `Enable` (spec InputCapture.xml:359-360).
/// - The capability-honesty contract: once the sequence completes,
///   `is_backend_available()` returns true and
///   `outgoing_capabilities()` advertises
///   `kdeconnect.shareinputdevices.request`.
#[tokio::test(flavor = "multi_thread")]
async fn enable_session_backend_drives_full_v1_sequence() {
    let _bus_lock = BUS.lock().await;

    // Install the socketpair fd BEFORE the fake is set up: the
    // fake's state is what production's ConnectToEIS reads from.
    let (client_stream, peer_stream) = UnixStream::pair().expect("UnixStream::pair");
    let connect_to_eis_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(client_stream.into_raw_fd()) };

    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: "/org/freedesktop/portal/desktop/session/bootattach".to_string(),
        zones: vec![(1920, 1080, 0, 0)],
        connect_to_eis_socketpair: Some(connect_to_eis_fd),
        ..Default::default()
    }));

    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };

    // The minimal EIS peer completes the handshake so the
    // receiver's `start()` returns, then KEEPS ITS END ALIVE
    // until this scope ends — Fix 3 narrows the boot-path
    // ordering race between `backend_available.store(true)` and
    // the disconnect watcher, but does not eliminate it. Keeping
    // the peer alive across the assertion removes the EOF path
    // that the watcher would otherwise observe. Passing `None`
    // means "stay alive until the JoinHandle drops"; tests that
    // care about the disconnect path drop their own oneshot.
    let _peer_thread = spawn_minimal_eis_peer(peer_stream, None);

    // The boot path. `enable_session_backend` is what bootstrap.rs
    // calls; with this lane's hookup it now drives the v1 sequence
    // in one shot.
    let plugin = ShareInputDevicesPlugin::new();
    plugin.enable_session_backend().await;

    // The v1 sequence is driven synchronously inside
    // `PortalSession::start` (zbus call_method + Response await), so
    // by the time `enable_session_backend` returns, the ledger has
    // the full sequence. A short poll guards against any
    // unlikely delay between the EI thread's `start()` returning
    // and the disconnect watcher firing.
    let deadline = Instant::now() + Duration::from_secs(2);
    let calls = loop {
        let calls = state.lock().unwrap().calls.clone();
        if calls.iter().any(|c| matches!(c, Call::Enable { .. })) {
            break calls;
        }
        if Instant::now() >= deadline {
            panic!(
                "v1 sequence did not reach Enable within 2s; calls so far: {:?}",
                state.lock().unwrap().calls
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let kinds: Vec<&'static str> = calls
        .iter()
        .map(|c| match c {
            Call::CreateSession { .. } => "CreateSession",
            Call::ConnectToEIS { .. } => "ConnectToEIS",
            Call::GetZones { .. } => "GetZones",
            Call::SetPointerBarriers { .. } => "SetPointerBarriers",
            Call::Enable { .. } => "Enable",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "CreateSession",
            "ConnectToEIS",
            "GetZones",
            "SetPointerBarriers",
            "Enable"
        ],
        "enable_session_backend must drive the full v1 sequence in one shot"
    );

    // Capability honesty: after a successful start the plugin
    // advertises its outgoing capability (the contract
    // outgoing_capabilities() asserts on — see mod.rs:919-925).
    assert!(
        plugin.portal_backend_available(),
        "portal_backend_available must be true after enable_session_backend completes the v1 sequence"
    );
    assert_eq!(
        plugin.outgoing_capabilities(),
        vec!["kdeconnect.shareinputdevices.request".to_string()],
        "outgoing capability must advertise after enable_session_backend completes the v1 sequence"
    );
}

/// Fix 2 (P2) — bound the boot-path awaits with a timeout. The
/// production code awaits `wire_rx_rx` / `disconnect_rx_rx` while
/// the dedicated pump thread completes the EIS handshake; a
/// portal that returns a valid-but-silent EIS fd (a peer that
/// completes the D-Bus handshake but never speaks EI) parks
/// `enable_session_backend` forever — that sits inline on the
/// daemon boot path (`bootstrap → create_state → Daemon::new`),
/// so the entire daemon never reaches `run()`, listeners never
/// bind, and the watchdog never starts. The fix wraps both
/// awaits in `tokio::time::timeout(BOOT_PATH_PUMP_DELIVERY_TIMEOUT)`
/// and degrades to the documented warn-and-stay-inert shape on
/// expiry; the pump thread keeps running to its own end.
///
/// **Test shape (per the brief).** A fake EIS peer that completes
/// the portal handshake but never speaks EI: the fake portal's
/// ConnectToEIS hands back the socketpair fd; the peer thread
/// holds the other end alive (so the socket doesn't EOF) but
/// never sends a HELLO, so `receiver.start()` blocks inside
/// `handshake_tokio`. `enable_session_backend` MUST return
/// within the boot-path timeout (NOT hang), and the backend MUST
/// stay un-advertised.
#[tokio::test(flavor = "multi_thread")]
async fn enable_session_backend_times_out_on_silent_eis_peer() {
    let _bus_lock = BUS.lock().await;

    let (client_stream, peer_stream) = UnixStream::pair().expect("UnixStream::pair");
    let connect_to_eis_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(client_stream.into_raw_fd()) };

    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: "/org/freedesktop/portal/desktop/session/silenteis".to_string(),
        zones: vec![(1920, 1080, 0, 0)],
        connect_to_eis_socketpair: Some(connect_to_eis_fd),
        ..Default::default()
    }));

    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };

    // The silent peer holds its end of the socketpair alive (so
    // the kernel doesn't deliver EOF to the receiver) but never
    // reads, writes, or runs an `eis::Context`. The receiver's
    // `handshake_tokio` blocks waiting for a peer's HELLO that
    // never arrives — exactly the silent-EIS shape the brief
    // targets. The thread's only purpose is to keep `peer_stream`
    // alive across the test; when the thread exits (test end +
    // `peer_stream` drop in this scope) the socketpair closes
    // and the pump thread's EOF arm would fire — irrelevant to
    // this test, but it's why the thread is alive for the
    // `enable_session_backend` call's duration.
    let _silent_peer = std::thread::spawn(move || {
        let _hold_open = peer_stream;
        std::thread::sleep(Duration::from_secs(30));
    });

    let plugin = ShareInputDevicesPlugin::new();

    // The boot path. With the fix, the await on `wire_rx_rx`
    // times out after `BOOT_PATH_PUMP_DELIVERY_TIMEOUT` (5s) and
    // the function returns inert. Without the fix, this would
    // hang forever — and the outer `tokio::time::timeout` below
    // would fire instead. We give the test a generous outer
    // bound (15s — 3x the production timeout) so the test fails
    // loudly if either the fix is missing or the timeout
    // constant drifts to a much larger value.
    let outer = Duration::from_secs(15);
    let inner_start = Instant::now();
    let result = tokio::time::timeout(outer, plugin.enable_session_backend()).await;
    let inner_elapsed = inner_start.elapsed();

    assert!(
        result.is_ok(),
        "enable_session_backend must return within the boot-path timeout \
         (got: {:?}, hung at outer {:?})",
        inner_elapsed,
        outer,
    );
    // Also assert it didn't burn the full outer window — that
    // would mean the production timeout isn't firing (either the
    // fix is missing or the constant is misconfigured).
    assert!(
        inner_elapsed < outer,
        "enable_session_backend returned at the outer timeout bound ({:?}); \
         the production boot-path timeout did not fire",
        inner_elapsed,
    );
    assert!(
        inner_elapsed >= Duration::from_secs(4),
        "enable_session_backend returned too fast ({:?}); did the boot-path \
         timeout actually engage, or did the test fire before the pump could block?",
        inner_elapsed,
    );

    // Capability honesty: a silent-EIS portal leaves the backend
    // un-advertised. The plugin's outgoing_capabilities() must
    // be empty.
    assert!(
        !plugin.portal_backend_available(),
        "backend_available must stay false when the EIS peer is silent"
    );
    assert!(
        plugin.outgoing_capabilities().is_empty(),
        "outgoing_capabilities must be empty when the EIS peer is silent"
    );
}

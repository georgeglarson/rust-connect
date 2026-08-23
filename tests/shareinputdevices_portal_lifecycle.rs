//! Integration tests for the ShareInputDevices producer's portal
//! half (Task #1042 M2). Exercises the v1 sequence against a fake
//! portal on a private D-Bus and pins the exact call order, barrier
//! coordinates, and probe-failure path.
//!
//! Test surface mirrors `tests/mpris_bus_recovery.rs` (same private
//! dbus-daemon pattern). The private dbus-daemon is process-singleton
//! via `set_var("DBUS_SESSION_BUS_ADDRESS", ...)`; with
//! `#[tokio::test(flavor = "multi_thread")]` each test runs on its
//! own runtime and would race the env var. We serialize them via a
//! process-wide `tokio::sync::Mutex` (`BUS`) acquired at the start
//! of every test and held until the end. The guard is `Send` (it's
//! a tokio mutex), so it survives the multi-thread runtime.
//!
//! Tests:
//! - `probe_passes_when_supported_caps_have_keyboard_pointer_v1`
//! - `probe_fails_when_caps_missing_keyboard`
//! - `probe_fails_when_version_below_one`
//! - `v1_session_records_call_sequence_in_spec_order`
//!
//! The v1 test uses a `FakePortal` that emits the Response signal
//! inline from each interface method (mirroring real xdp behaviour:
//! `request.emit_response()` runs synchronously after the method
//! returns its Request path). The test's `Connection` is shared via
//! `Arc<Mutex<Option<...>>>`; the fake picks it up at emission time.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::zvariant::OwnedValue;

/// Process-wide serializing lock for the private dbus-daemon.
/// `DBUS_SESSION_BUS_ADDRESS` is process-level, so concurrent tests
/// would race the env var and each other's daemons. Acquired at the
/// top of every test, held until end-of-test. Tokio mutex because
/// the guard must be `Send` across the multi-thread runtime.
static BUS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH_STR: &str = "/org/freedesktop/portal/desktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

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

/// Recorded call. The fake populates these and tests assert on the
/// exact sequence — the full five-call v1 lifecycle when the fake
/// returns a real zone, the three-call prefix on the empty-zones
/// abort path. `SessionClose` lands when the test drives an explicit
/// `close()` against the served session object.
#[derive(Debug, Clone)]
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
    /// `cursor_position` decoded STRICTLY as `(dd)` — `None` when the
    /// value arrived as anything else (notably the `(vv)` a
    /// StructureBuilder produces). A real portal decodes this field
    /// as a `(dd)` and rejects other shapes, so the strict decode is
    /// the oracle, not a convenience.
    Release {
        session_handle: String,
        cursor_position: Option<(f64, f64)>,
    },
    SessionClose,
}

#[derive(Default)]
struct FakePortalState {
    pub version: u32,
    pub supported_caps: u32,
    pub calls: Vec<Call>,
    pub request_id: Arc<AtomicU32>,
    /// The session_handle the fake hands out for CreateSession —
    /// shared across the test so SetPointerBarriers/Enable see the
    /// same value the production code carries forward.
    pub session_handle: String,
    /// Connection the fake uses to emit Response signals inline.
    /// `None` until `setup()` runs.
    pub conn: Option<zbus::Connection>,
    /// The zones GetZones returns, as `(width, height, x, y)` tuples
    /// (the portal's `a(uuii)` wire order). Empty = the abort path
    /// ("no pointer barriers can be set"); one real zone drives the
    /// sequence through SetPointerBarriers + Enable.
    pub zones: Vec<(u32, u32, i32, i32)>,
    /// If set, the fake hands THIS fd back from ConnectToEIS
    /// instead of /dev/null. The M4 wiring test installs one end of
    /// a UnixStream::pair() here so the production code's
    /// `take_ei_fd()` receives a real EIS peer, not a dead handle.
    /// `take()`'d on the call → at most one ConnectToEIS per test.
    pub connect_to_eis_socketpair: Option<std::os::fd::OwnedFd>,
}

/// The fake portal. Interface methods record the call, return a
/// placeholder Request path, then SYNCHRONOUSLY emit the Response
/// signal on that path — mirroring how xdp-portal behaves in real
/// life (the response is sent inline, not via a separate request
/// object worker).
struct FakePortal {
    state: Arc<Mutex<FakePortalState>>,
}

#[zbus::interface(name = "org.freedesktop.portal.InputCapture")]
impl FakePortal {
    // `#[zbus(property)]` on a function would auto-pascal_case the
    // name (the macro's default for read-only properties). The real
    // portal exposes these properties under
    // `version` (lowercase) and `SupportedCapabilities`
    // (PascalCase) — `version` doesn't match what pascal_case would
    // produce. We pin both names explicitly with `#[zbus(name = "...")]`
    // so the fake's introspection matches the real portal and the
    // probe's hardcoded property strings resolve.
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
        // Take everything we need out of the lock, then drop the
        // guard before the await. std::sync::MutexGuard is !Send,
        // so holding it across .await breaks the multi-thread test
        // runtime.
        let (session_handle, request_path, conn_for_signal) = {
            let mut guard = self.state.lock().unwrap();
            let session_handle = guard.session_handle.clone();
            guard.calls.push(Call::CreateSession { capabilities });
            let id = guard.request_id.fetch_add(1, Ordering::SeqCst);
            let request_path = format!("/org/freedesktop/portal/desktop/request/{id}");
            let conn_for_signal = guard.conn.as_ref().expect("conn").clone();
            (session_handle, request_path, conn_for_signal)
        };

        // Emit Response inline. results = { session_handle: o }
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
        // If the test installed a socketpair fd (M4 wiring test),
        // hand the client end to the production code; the peer end
        // stays in the test's state for the fake EIS harness to
        // play. Otherwise fall back to /dev/null — the test never
        // reads the fd and we just need the right wire shape.
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

        // Reply with the zones configured on the fake's state +
        // zone_set=0. Empty zones = the production abort path ("no
        // pointer barriers can be set", spec InputCapture.xml:138-139);
        // a real zone drives the sequence through SetPointerBarriers
        // and Enable.
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

    /// `Release(session_handle: o, options: a{sv})` — no Request
    /// path, no Response (spec InputCapture.xml:337-345).
    ///
    /// `cursor_position` is decoded with the SAME strict `(dd)`
    /// conversion the production inbound path uses on Activated
    /// (portal.rs:1436) and that a real portal applies. A value
    /// carrying variant-wrapped fields — `(vv)`, which
    /// `StructureBuilder::add_field(Value::F64(..))` produces —
    /// fails this conversion and records `None`, exactly as the
    /// real portal would reject it. This is the same class of trap
    /// the barrier `position` field hit (`(vvvv)` instead of `ai`).
    #[zbus(name = "Release")]
    async fn release(
        &self,
        session_handle: zbus::zvariant::OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let cursor_position = options
            .get("cursor_position")
            .and_then(|v| <(f64, f64)>::try_from(v.clone()).ok());
        self.state.lock().unwrap().calls.push(Call::Release {
            session_handle: session_handle.as_str().to_string(),
            cursor_position,
        });
        Ok(())
    }

    #[zbus(name = "SetPointerBarriers")]
    async fn set_pointer_barriers(
        &self,
        session_handle: zbus::zvariant::OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        // Record the call incl. the first barrier's id + position so
        // the test can pin the wire encoding (the `aa{sv}` barrier
        // entry is hand-built in portal.rs). Position must arrive as
        // `ai` — QList<int> in upstream (inputcapturesession.cpp:230).
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
            let _ = session_handle;
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

/// Minimal session object at the session_handle path. Production's
/// `PortalSession::close()` calls `org.freedesktop.portal.Session.
/// Close` there (portal.rs close()); without this object the call
/// fails with UnknownObject. Only the full-sequence test serves it —
/// the abort test deliberately leaves it out so the guard's
/// best-effort Close has nothing to land on.
struct FakeSession {
    state: Arc<Mutex<FakePortalState>>,
}

#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl FakeSession {
    async fn close(&self) -> zbus::fdo::Result<()> {
        self.state.lock().unwrap().calls.push(Call::SessionClose);
        Ok(())
    }
}

/// Emit a Response signal on the given request path. The signal's
/// body is `(u response, a{sv} results)` per the portal spec.
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

/// Spawn the fake portal. Returns the daemon guard. The state Arc
/// stays alive for the lifetime of the daemon — held via a
/// test-only leak (see end of fn).
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
    // the daemon's lifetime. End of test = DaemonGuard kills the
    // daemon = all connections die.
    Box::leak(Box::new(conn));
    Box::leak(Box::new(tmp));

    Some(DaemonGuard(Some(daemon)))
}

// ============ TESTS ============

#[tokio::test(flavor = "multi_thread")]
async fn probe_passes_when_supported_caps_have_keyboard_pointer_v1() {
    let _bus_lock = BUS.lock().await;
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3, // keyboard (1) | pointer (2)
        ..Default::default()
    }));
    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };
    let conn = zbus::Connection::session().await.unwrap();
    let result =
        rust_connect::plugins::shareinputdevices::portal::probe_portal_available(&conn).await;
    assert!(
        result,
        "probe must pass when InputCapture v1 reports caps=3 (keyboard+pointer)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_fails_when_caps_missing_keyboard() {
    let _bus_lock = BUS.lock().await;
    // caps = 2 → pointer only, no keyboard.
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 2,
        ..Default::default()
    }));
    let Some(_daemon) = setup(state).await else {
        return;
    };
    let conn = zbus::Connection::session().await.unwrap();
    let result =
        rust_connect::plugins::shareinputdevices::portal::probe_portal_available(&conn).await;
    assert!(
        !result,
        "probe must FAIL when SupportedCapabilities is missing keyboard (1) bit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_fails_when_version_below_one() {
    let _bus_lock = BUS.lock().await;
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 0,
        supported_caps: 3,
        ..Default::default()
    }));
    let Some(_daemon) = setup(state).await else {
        return;
    };
    let conn = zbus::Connection::session().await.unwrap();
    let result =
        rust_connect::plugins::shareinputdevices::portal::probe_portal_available(&conn).await;
    assert!(!result, "probe must FAIL when version is < 1");
}

/// Drives the FULL v1 sequence against a fake that returns one real
/// zone (1920x1080 at origin) and emits Response signals inline.
/// This is the test the raw-zbus dependency choice is justified with
/// (portal.rs module doc): it pins the exact five-call order
/// CreateSession → ConnectToEIS → GetZones → SetPointerBarriers →
/// Enable (ConnectToEIS strictly before Enable, spec), the
/// capabilities=3 body, the session_handle carried through every
/// call, and the hand-built `aa{sv}` barrier entry's wire contents —
/// position as an `ai` array (QList<int> in upstream,
/// inputcapturesession.cpp:230), with the Left-edge barrier on a
/// single zone being the vertical line x=0 from y=0 to the INCLUSIVE
/// bottom y=1079 (the QRect quirk barrier.rs replicates).
#[tokio::test(flavor = "multi_thread")]
async fn v1_session_records_call_sequence_in_spec_order() {
    let _bus_lock = BUS.lock().await;
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: "/org/freedesktop/portal/desktop/session/test1".to_string(),
        // (width, height, x, y) — the portal's a(uuii) wire order.
        zones: vec![(1920, 1080, 0, 0)],
        ..Default::default()
    }));
    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };

    // Serve the session object the explicit close() at the end of
    // this test calls Session.Close on.
    let fake_conn = state
        .lock()
        .unwrap()
        .conn
        .clone()
        .expect("setup() stores the fake's connection");
    let session_path = zbus::zvariant::ObjectPath::from_string_unchecked(
        state.lock().unwrap().session_handle.clone(),
    );
    assert!(
        fake_conn
            .object_server()
            .at(
                session_path,
                FakeSession {
                    state: state.clone()
                }
            )
            .await
            .expect("session object registration must not error"),
        "fake session object must register at the session_handle path"
    );

    let conn = zbus::Connection::session().await.unwrap();
    let (activated_tx, _activated_rx) = tokio::sync::mpsc::unbounded_channel::<
        rust_connect::plugins::shareinputdevices::portal::ActivatedEvent,
    >();

    let session = tokio::time::timeout(
        Duration::from_secs(5),
        rust_connect::plugins::shareinputdevices::portal::PortalSession::start(
            conn,
            rust_connect::plugins::shareinputdevices::Edge::Left,
            activated_tx,
        ),
    )
    .await
    .expect("PortalSession::start must complete well under the timeout")
    .expect("PortalSession::start must succeed with one real zone");

    let calls = state.lock().unwrap().calls.clone();
    let kinds: Vec<&'static str> = calls
        .iter()
        .map(|c| match c {
            Call::CreateSession { .. } => "CreateSession",
            Call::ConnectToEIS { .. } => "ConnectToEIS",
            Call::GetZones { .. } => "GetZones",
            Call::SetPointerBarriers { .. } => "SetPointerBarriers",
            Call::Enable { .. } => "Enable",
            Call::Release { .. } => "Release",
            Call::SessionClose => "SessionClose",
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
        "full v1 sequence must be CreateSession -> ConnectToEIS -> GetZones -> \
         SetPointerBarriers -> Enable, with ConnectToEIS strictly before Enable (spec)"
    );

    // Pin CreateSession body: capabilities = 3 (keyboard | pointer).
    if let Call::CreateSession { capabilities } = &calls[0] {
        assert_eq!(
            *capabilities,
            Some(3),
            "CreateSession must carry capabilities:3 (keyboard|pointer)"
        );
    }

    // Pin ConnectToEIS + GetZones: both must carry the same
    // session_handle the fake returned in CreateSession's response.
    let expected_handle = "/org/freedesktop/portal/desktop/session/test1".to_string();
    if let Call::ConnectToEIS { session_handle } = &calls[1] {
        assert_eq!(
            session_handle, &expected_handle,
            "ConnectToEIS must carry the session_handle returned by CreateSession"
        );
    }
    if let Call::GetZones { session_handle } = &calls[2] {
        assert_eq!(
            session_handle, &expected_handle,
            "GetZones must carry the session_handle"
        );
    }

    // Pin the hand-built barrier entry: Left edge of the only zone —
    // vertical line at x=0, y from 0 to the inclusive bottom 1079 —
    // and the zone_set passthrough (the 0 GetZones returned).
    if let Call::SetPointerBarriers {
        barrier_id,
        position,
        zone_set,
    } = &calls[3]
    {
        assert_eq!(*barrier_id, Some(1), "barrier_id must be 1");
        assert_eq!(
            position.as_deref(),
            Some(&[0, 0, 0, 1079][..]),
            "Left-edge barrier on one 1920x1080 zone must be (0,0)-(0,1079) \
             (inclusive bottom — the QRect quirk)"
        );
        assert_eq!(
            *zone_set, 0,
            "SetPointerBarriers must carry the zone_set GetZones returned"
        );
    }

    // Enable carries the session handle; the session is live now —
    // close it explicitly so Drop's best-effort Close stays the
    // backstop, not the path under test.
    if let Call::Enable { session_handle } = &calls[4] {
        assert_eq!(session_handle, &expected_handle);
    }
    session.close().await.expect("explicit close must succeed");
    let calls = state.lock().unwrap().calls.clone();
    assert!(
        matches!(calls.last(), Some(Call::SessionClose)),
        "explicit close must land Session.Close on the session object; got {calls:?}"
    );
}

/// The empty-zones abort path: GetZones returning no zones means
/// "no pointer barriers can be set" (spec InputCapture.xml:138-139),
/// so production must error out after exactly three calls — and the
/// SessionCloseGuard must keep the aborted session from leaking
/// (best-effort Close; the fake serves no session object, so only
/// the call sequence is asserted, not the Close itself).
#[tokio::test(flavor = "multi_thread")]
async fn v1_session_aborts_on_empty_zones() {
    let _bus_lock = BUS.lock().await;
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: "/org/freedesktop/portal/desktop/session/test1".to_string(),
        zones: Vec::new(),
        ..Default::default()
    }));
    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };

    let conn = zbus::Connection::session().await.unwrap();
    let (activated_tx, _activated_rx) = tokio::sync::mpsc::unbounded_channel::<
        rust_connect::plugins::shareinputdevices::portal::ActivatedEvent,
    >();

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        rust_connect::plugins::shareinputdevices::portal::PortalSession::start(
            conn,
            rust_connect::plugins::shareinputdevices::Edge::Left,
            activated_tx,
        ),
    )
    .await
    .expect("empty-zones abort must complete well under the timeout");
    assert!(
        result.is_err(),
        "PortalSession::start must FAIL when GetZones returns no zones"
    );

    let calls = state.lock().unwrap().calls.clone();
    let kinds: Vec<&'static str> = calls
        .iter()
        .map(|c| match c {
            Call::CreateSession { .. } => "CreateSession",
            Call::ConnectToEIS { .. } => "ConnectToEIS",
            Call::GetZones { .. } => "GetZones",
            Call::SetPointerBarriers { .. } => "SetPointerBarriers",
            Call::Enable { .. } => "Enable",
            Call::Release { .. } => "Release",
            Call::SessionClose => "SessionClose",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["CreateSession", "ConnectToEIS", "GetZones"],
        "empty zones must abort after GetZones — SetPointerBarriers/Enable unreachable"
    );
}

/// The Release wire shape: `cursor_position` must go out as a D-Bus
/// `(dd)`, and must carry `barrier.p1() + release_delta` (absolute),
/// not the peer's relative delta.
///
/// Both halves have real failure modes. The shape half:
/// `StructureBuilder::add_field(Value::F64(..))` wraps each field in
/// a variant, putting `(vv)` on the wire where the spec says `(dd)`
/// (InputCapture.xml:337-345, cpp :278 `QPointF`) — the identical
/// trap the barrier `position` field hit as `(vvvv)` instead of `ai`.
/// A real portal decodes `cursor_position` as `(dd)` and rejects the
/// variant-wrapped form, so every Release would fail against a live
/// compositor while every existing test stayed green: nothing
/// decoded a Release body until this test.
///
/// The arithmetic half: the zone here sits at (100, 200), so the
/// Left-edge barrier origin is (100, 200) and a (50, 100) delta must
/// surface as (150.0, 300.0). Sending the delta raw would release
/// the cursor at (50, 100) — wrong on any barrier away from the
/// origin (cpp :275-279).
#[tokio::test(flavor = "multi_thread")]
async fn release_sends_cursor_position_as_dd_offset_from_barrier_origin() {
    let _bus_lock = BUS.lock().await;
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: "/org/freedesktop/portal/desktop/session/release1".to_string(),
        // Deliberately NOT at the origin: (width, height, x, y).
        zones: vec![(1920, 1080, 100, 200)],
        ..Default::default()
    }));
    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };

    let fake_conn = state
        .lock()
        .unwrap()
        .conn
        .clone()
        .expect("setup() stores the fake's connection");
    let session_path = zbus::zvariant::ObjectPath::from_string_unchecked(
        state.lock().unwrap().session_handle.clone(),
    );
    assert!(
        fake_conn
            .object_server()
            .at(
                session_path,
                FakeSession {
                    state: state.clone()
                }
            )
            .await
            .expect("session object registration must not error"),
        "fake session object must register at the session_handle path"
    );

    let conn = zbus::Connection::session().await.unwrap();
    let (activated_tx, _activated_rx) = tokio::sync::mpsc::unbounded_channel::<
        rust_connect::plugins::shareinputdevices::portal::ActivatedEvent,
    >();

    let session = tokio::time::timeout(
        Duration::from_secs(5),
        rust_connect::plugins::shareinputdevices::portal::PortalSession::start(
            conn,
            rust_connect::plugins::shareinputdevices::Edge::Left,
            activated_tx,
        ),
    )
    .await
    .expect("PortalSession::start must complete well under the timeout")
    .expect("PortalSession::start must succeed with one real zone");

    session
        .release(50, 100)
        .await
        .expect("release must succeed against the fake portal");

    let calls = state.lock().unwrap().calls.clone();
    let release = calls
        .iter()
        .find_map(|c| match c {
            Call::Release {
                session_handle,
                cursor_position,
            } => Some((session_handle.clone(), *cursor_position)),
            _ => None,
        })
        .expect("release() must land a Release call on the portal interface");

    assert_eq!(
        release.0, "/org/freedesktop/portal/desktop/session/release1",
        "Release must carry the session_handle CreateSession returned"
    );
    assert_eq!(
        release.1,
        Some((150.0, 300.0)),
        "cursor_position must decode as a (dd) carrying barrier.p1() + delta \
         = (100+50, 200+100); None means the value did not arrive as (dd) at \
         all — the (vv) StructureBuilder trap a real portal rejects"
    );

    session.close().await.expect("explicit close must succeed");
}

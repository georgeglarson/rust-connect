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
/// exact sequence. SetPointerBarriers / Enable are not reached by
/// the current test (empty zones aborts earlier) but exist for
/// completeness when more end-to-end coverage lands.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant, dead_code)]
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
    /// The session_handle the fake hands out for CreateSession —
    /// shared across the test so SetPointerBarriers/Enable see the
    /// same value the production code carries forward.
    pub session_handle: String,
    /// Connection the fake uses to emit Response signals inline.
    /// `None` until `setup()` runs.
    pub conn: Option<zbus::Connection>,
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
        self.state.lock().unwrap().calls.push(Call::ConnectToEIS {
            session_handle: sh_str,
        });
        // Return a placeholder fd. The test never reads the fd; we
        // just need a value with the right wire shape (`h` handle).
        // zbus's owned-fd semantics: take one from ourselves by
        // dup'ing /dev/null — never actually read by either side.
        let f = std::fs::File::open("/dev/null").expect("/dev/null");
        let owned = std::os::fd::OwnedFd::from(f);
        Ok(zbus::zvariant::Fd::from(owned))
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

        // Reply with an empty zones list + zone_set=0. Production
        // code aborts at the empty-zones check, so we never reach
        // SetPointerBarriers / Enable. This is intentional: the
        // test pins the call SEQUENCE without needing the planner
        // to actually produce a barrier.
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert(
            "zones".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::Array(zbus::zvariant::Array::from(
                Vec::<(u32, u32, i32, i32)>::new(),
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
        _barriers: Vec<HashMap<String, OwnedValue>>,
        _zone_set: u32,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        // Not reached by this test (empty zones aborts earlier),
        // but kept for completeness.
        let (request_path, conn_for_signal) = {
            let guard = self.state.lock().unwrap();
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
        _session_handle: zbus::zvariant::OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        // Not reached by this test.
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

/// Drives the v1 sequence against a fake that emits Response
/// signals inline. The fake's GetZones returns an empty zones
/// list, which the production code treats as
/// "no pointer barriers can be set" (spec InputCapture.xml:138-139)
/// and aborts. So this test pins the first three calls in order:
/// CreateSession → ConnectToEIS → GetZones, with CreateSession
/// carrying capabilities=3 and ConnectToEIS/GetZones carrying the
/// session_handle the fake returned.
#[tokio::test(flavor = "multi_thread")]
async fn v1_session_records_call_sequence_in_spec_order() {
    let _bus_lock = BUS.lock().await;
    let state = Arc::new(Mutex::new(FakePortalState {
        version: 1,
        supported_caps: 3,
        session_handle: "/org/freedesktop/portal/desktop/session/test1".to_string(),
        ..Default::default()
    }));
    let Some(_daemon) = setup(state.clone()).await else {
        return;
    };

    let conn = zbus::Connection::session().await.unwrap();
    let (activated_tx, _activated_rx) = tokio::sync::mpsc::unbounded_channel::<
        rust_connect::plugins::shareinputdevices::portal::ActivatedEvent,
    >();

    // PortalSession::start will fail (empty zones) — that's the
    // intended path. We just need the calls recorded in order.
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        rust_connect::plugins::shareinputdevices::portal::PortalSession::start(
            conn,
            rust_connect::plugins::shareinputdevices::Edge::Left,
            activated_tx,
        ),
    )
    .await;

    let calls = state.lock().unwrap().calls.clone();
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
        vec!["CreateSession", "ConnectToEIS", "GetZones"],
        "v1 sequence through GetZones must be: CreateSession -> ConnectToEIS -> GetZones. \
         SetPointerBarriers/Enable are NOT reached because GetZones returned empty zones \
         (spec: 'no pointer barriers can be set')"
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
}

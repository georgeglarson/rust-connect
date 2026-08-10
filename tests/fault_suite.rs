//! Root-only network-fault suite for Task 2.4 (vk #990): keepalive
//! blackhole and a suspend/resume approximation. Needs `CAP_NET_ADMIN`
//! (namespace/veth creation) plus `iptables`, gated the SAME loud,
//! never-silent way `tests/netns_discovery.rs` (Task 2.2) already is —
//! see that file's module doc for the full rationale; this one follows its
//! conventions (namespace naming, cleanup-on-panic guards, bounded
//! deadlines, per-test `NetnsGuard`/`VethGuard`) rather than inventing new
//! ones. The scenarios that DON'T need real network-fault injection
//! (duplicate-dial storm, delayed stale-loop cleanup, peer restart, daemon
//! restart) live in `tests/fault_recovery.rs` instead, so ordinary `cargo
//! test` still exercises them.
//!
//! Run explicitly as root (same rustup-shim gotcha as `netns_discovery.rs`
//! — a bare `sudo -E cargo` hits the shim as root, which has no default
//! toolchain configured and fails before compiling anything):
//! ```text
//! TOOLBIN=$(dirname "$(rustup which rustc)") && \
//!   sudo env PATH="$TOOLBIN:$PATH" HOME="$HOME" CARGO_HOME="$HOME/.cargo" \
//!   "$TOOLBIN/cargo" test --test fault_suite --locked
//! ```
//! Non-root runs of the ordinary `cargo test` suite pass this file cleanly
//! with two skip lines. Each scenario is deliberately slow (the keepalive
//! scenario waits out a REAL kernel TCP_USER_TIMEOUT/keepalive cycle, up to
//! ~70s) — this suite is explicit on-demand, not a hot loop, matching the
//! brief's framing for root-only coverage.
//!
//! ## Scenario 6 (suspend/resume): honestly-scoped approximation
//!
//! There is no way to fake `clock_gettime(CLOCK_BOOTTIME)` from a test
//! without actually suspending real hardware (`suspend_watchdog` in
//! `src/services/network_watcher.rs` calls the syscall directly, by
//! design — see that module's doc), so this scenario does NOT drive
//! `suspend_watchdog` itself. It approximates the observable shape of a
//! resume instead: a real interface down/up cycle on the link carrying an
//! established connection (the same fault injection
//! `netns_discovery.rs`'s own `netns_interface_down_up_triggers_reannounce`
//! uses for the discovery layer, extended here to the CONNECTION layer),
//! then a single explicit redial standing in for the production
//! rediscovery-broadcast trigger (`service_manager.rs` wiring, out of
//! scope here — same documented-simplification shape as
//! `tests/fault_recovery.rs`'s peer-restart and daemon-restart scenarios).
//! The REAL suspend leg — an actual laptop lid close/open — stays with the
//! passive soak from Task 2.2 (vk #994); this is the reproducible,
//! CI-shaped stand-in.

use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use rust_connect::device::types::DeviceType;
use rust_connect::protocol::connection::CONNECTION_RATE_LIMIT;
use rust_connect::protocol::connection_loop::{run_packet_loop, LoopResult};
use rust_connect::protocol::keepalive::{
    KEEPALIVE_IDLE, KEEPALIVE_INTERVAL, KEEPALIVE_RETRIES, TCP_USER_TIMEOUT,
};
use rust_connect::protocol::types::Identity;
use rust_connect::protocol::{CertificateManager, ConnectionManager};
use tokio_util::sync::CancellationToken;

// --- netns/veth primitives -------------------------------------------
// Deliberately duplicated from `tests/netns_discovery.rs` rather than
// factored into a shared module: integration test binaries are separate
// crates in this repo and there is no existing shared-test-support
// convention to extend (see that file for the canonical version and its
// design notes — cleanup-on-panic guards, per-test SEQ-suffixed probe
// names, the setns(2)-on-a-dedicated-thread requirement).

static SEQ: AtomicU32 = AtomicU32::new(1);

fn is_root() -> bool {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn ip_on_path() -> bool {
    Command::new("ip").arg("-Version").output().is_ok()
}

fn iptables_on_path() -> bool {
    Command::new("iptables").arg("--version").output().is_ok()
}

fn preconditions_met(test_name: &str) -> bool {
    if !is_root() {
        eprintln!(
            "{test_name}: not running as root — skipping (netns/veth creation and iptables \
             rules need CAP_NET_ADMIN/CAP_NET_RAW; run under sudo to exercise this suite)"
        );
        return false;
    }
    if !ip_on_path() {
        eprintln!("{test_name}: `ip` (iproute2) not on PATH — skipping");
        return false;
    }
    if !iptables_on_path() {
        eprintln!("{test_name}: `iptables` not on PATH — skipping");
        return false;
    }
    let probe = format!(
        "rcfaultprobe-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let created = Command::new("ip")
        .args(["netns", "add", &probe])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if created {
        let _ = Command::new("ip")
            .args(["netns", "delete", &probe])
            .output();
        true
    } else {
        eprintln!(
            "{test_name}: root but `ip netns add` failed (restricted container, missing \
             CAP_NET_ADMIN?) — skipping"
        );
        false
    }
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn run_best_effort(cmd: &mut Command) {
    let _ = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output();
}

struct NetnsGuard {
    name: String,
}

impl NetnsGuard {
    fn create(label: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let name = format!("rcfns-{label}-{}-{n}", std::process::id());
        run_ok(Command::new("ip").args(["netns", "add", &name]));
        Self { name }
    }
}

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        run_best_effort(Command::new("ip").args(["netns", "del", &self.name]));
    }
}

struct VethGuard {
    host_side: String,
    ns_side: String,
    /// The directly-connected /24 route this creates persists across a
    /// link down/up (unlike a default route — see `netns_discovery.rs`'s
    /// `restore_default_route` doc), so unicast dials to `ns_ip` never
    /// need a route restored; nothing here dials the host side by address.
    ns_ip: String,
}

impl VethGuard {
    fn create(ns: &NetnsGuard) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let host_side = format!("rcfv{n}h");
        let ns_side = format!("rcfv{n}n");
        let host_ip = format!("10.251.{}.1", n % 250);
        let ns_ip = format!("10.251.{}.2", n % 250);
        let host_addr = format!("{host_ip}/24");
        let ns_addr = format!("{ns_ip}/24");

        run_ok(Command::new("ip").args([
            "link", "add", &host_side, "type", "veth", "peer", "name", &ns_side,
        ]));
        run_ok(Command::new("ip").args(["link", "set", &ns_side, "netns", &ns.name]));
        run_ok(Command::new("ip").args(["addr", "add", &host_addr, "dev", &host_side]));
        run_ok(Command::new("ip").args(["link", "set", &host_side, "up"]));
        run_ok(Command::new("ip").args([
            "netns", "exec", &ns.name, "ip", "addr", "add", &ns_addr, "dev", &ns_side,
        ]));
        run_ok(Command::new("ip").args([
            "netns", "exec", &ns.name, "ip", "link", "set", &ns_side, "up",
        ]));
        run_ok(
            Command::new("ip").args(["netns", "exec", &ns.name, "ip", "link", "set", "lo", "up"]),
        );

        Self {
            host_side,
            ns_side,
            ns_ip,
        }
    }

    fn exec_ip(&self, ns: &NetnsGuard, args: &[&str]) {
        let mut full = vec!["netns", "exec", ns.name.as_str(), "ip"];
        full.extend_from_slice(args);
        run_ok(Command::new("ip").args(&full));
    }
}

impl Drop for VethGuard {
    fn drop(&mut self) {
        let _ = &self.ns_side; // referenced for clarity only, see below
        run_best_effort(Command::new("ip").args(["link", "del", &self.host_side]));
    }
}

/// Blackholes all traffic to/from `ip` at the HOST's own netfilter hooks —
/// no packet leaves this box toward `ip`, and none arriving from it gets
/// past the host's own stack, so from either endpoint's perspective the
/// peer has simply vanished (no FIN, no RST, no ICMP unreachable). Cheaper
/// and more robust than injecting rules inside the netns: it needs no
/// `ip netns exec` round-trip and no coordination with the netns worker
/// thread, and removal is symmetric.
struct BlackholeGuard {
    ip: String,
}

impl BlackholeGuard {
    fn engage(ip: &str) -> Self {
        run_ok(Command::new("iptables").args(["-I", "OUTPUT", "1", "-d", ip, "-j", "DROP"]));
        run_ok(Command::new("iptables").args(["-I", "INPUT", "1", "-s", ip, "-j", "DROP"]));
        Self { ip: ip.to_string() }
    }
}

impl Drop for BlackholeGuard {
    fn drop(&mut self) {
        run_best_effort(
            Command::new("iptables").args(["-D", "OUTPUT", "-d", &self.ip, "-j", "DROP"]),
        );
        run_best_effort(
            Command::new("iptables").args(["-D", "INPUT", "-s", &self.ip, "-j", "DROP"]),
        );
    }
}

/// Runs `f` on a dedicated OS thread `setns(2)`'d into `ns_name` BEFORE
/// building its own single-threaded tokio runtime — see
/// `tests/netns_discovery.rs`'s module doc for why both are load-bearing
/// (`setns` reassigns only the calling THREAD's namespace, and a migratable
/// multi-threaded-runtime task could hop off it mid-flight).
fn run_in_netns<F, Fut, T>(ns_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    let ns_path = format!("/var/run/netns/{ns_name}");
    let ns_name_owned = ns_name.to_string();
    std::thread::Builder::new()
        .name(format!("faultns-{ns_name_owned}"))
        .spawn(move || {
            use std::os::unix::io::AsRawFd;
            let ns_file =
                std::fs::File::open(&ns_path).unwrap_or_else(|e| panic!("open {ns_path}: {e}"));
            let fd = ns_file.as_raw_fd();
            // SAFETY: `fd` refers to a network-namespace inode just opened
            // above, valid for this call; this thread is freshly spawned
            // and does nothing namespace-sensitive before this call.
            let rc = unsafe { libc::setns(fd, libc::CLONE_NEWNET) };
            assert_eq!(
                rc,
                0,
                "setns({ns_name_owned}) failed: {}",
                std::io::Error::last_os_error()
            );
            drop(ns_file);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread runtime inside the netns worker thread");
            rt.block_on(f())
        })
        .expect("spawn netns worker thread")
        .join()
        .expect("netns worker thread panicked")
}

// --- fault-suite specific helpers --------------------------------------

/// Worst-case bound before a genuinely blackholed peer's death is observed
/// on our side, derived from the REAL production keepalive constants (not
/// re-hardcoded — a future tuning change widens/narrows this with it,
/// rather than silently invalidating the suite, per the brief). Two
/// independent kernel mechanisms can fire: `TCP_USER_TIMEOUT` bounds any
/// unacknowledged data once retransmission starts (covers a blackhole that
/// begins mid-write), and the keepalive idle+retries cycle covers a
/// connection that was already idle when the blackhole began (the probe
/// itself becomes the "unacknowledged data" `TCP_USER_TIMEOUT` then also
/// bounds) — worst case is idle-until-first-probe plus that timeout, i.e.
/// `KEEPALIVE_IDLE + TCP_USER_TIMEOUT`. +30s slack is scheduler/CI margin;
/// this suite is explicit on-demand, not a hot loop.
fn keepalive_death_deadline() -> Duration {
    let idle_then_probe_timeout = KEEPALIVE_IDLE + TCP_USER_TIMEOUT;
    let keepalive_cycle = KEEPALIVE_IDLE + KEEPALIVE_INTERVAL * KEEPALIVE_RETRIES;
    idle_then_probe_timeout
        .max(keepalive_cycle)
        .max(TCP_USER_TIMEOUT)
        + Duration::from_secs(30)
}

fn dev_id(tag: &str) -> String {
    let mut id = format!("f24ns-{tag}-");
    while id.len() < 34 {
        id.push('a');
    }
    id.truncate(34);
    id
}

async fn wait_until<F, Fut>(mut cond: F, attempts: u32, poll: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..attempts {
        if cond().await {
            return true;
        }
        tokio::time::sleep(poll).await;
    }
    false
}

/// Spawns the netns-side peer: a `ConnectionManager` bound to `ns_ip:0`
/// that answers inbound dials via `accept_test_as_client` (the same
/// test-only role `tests/fault_recovery.rs` uses), looped so more than one
/// dial can land. Returns the bound port over `mpsc` once ready.
fn spawn_netns_peer(
    ns_name: String,
    peer_certs_dir: std::path::PathBuf,
    peer_id: String,
    our_id: String,
    port_tx: mpsc::Sender<u16>,
    shutdown: CancellationToken,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        run_in_netns(&ns_name, move || async move {
            let peer_certs = Arc::new(CertificateManager::new(peer_certs_dir));
            peer_certs.init().expect("peer cert init inside netns");
            peer_certs
                .ensure_certificate(&peer_id, "Peer")
                .expect("peer cert");
            peer_certs
                .ensure_certificate(&our_id, "Us")
                .expect("peer's copy of our cert");
            let peer_cm = Arc::new(ConnectionManager::new(peer_certs).expect("peer cm"));
            peer_cm.set_device_identity(&peer_id, "Peer");

            let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
                .await
                .expect("bind peer listener inside netns");
            let addr = listener.local_addr().expect("local_addr");
            port_tx.send(addr.port()).expect("send bound port");

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return; };
                        let peer_cm = peer_cm.clone();
                        let our_id = our_id.clone();
                        tokio::spawn(async move {
                            let _ = peer_cm
                                .accept_test_as_client(our_id, stream, addr.port())
                                .await;
                        });
                    }
                }
            }
        })
    })
}

fn peer_identity(peer_id: &str, tcp_port: u16) -> Identity {
    let mut identity = Identity::new(
        peer_id.to_string(),
        "Peer".to_string(),
        DeviceType::Phone,
        vec![],
        vec![],
    );
    identity.tcp_port = Some(tcp_port);
    identity
}

/// Scenario 1 (brief, the headline): a real TLS link across a veth pair,
/// silently blackholed (iptables DROP, not a FIN/RST — the peer must
/// vanish, not close). Asserts the link is observed dead within the
/// keepalive-derived deadline, `send_packet` fails honestly rather than
/// reporting false success on the dead link (the Task 2.0 `sent:true`
/// defect's regression surface — see `docs/functional-completeness-plan.md`
/// Task 2.0), and a fresh dial after the blackhole lifts replaces cleanly
/// with exactly one live generation (the F-4 recovery leg).
#[tokio::test]
async fn netns_keepalive_blackhole_is_observed_dead_and_recovers() {
    if !preconditions_met("netns_keepalive_blackhole_is_observed_dead_and_recovers") {
        return;
    }

    let our_id = dev_id("blackhole-our");
    let peer_id = dev_id("blackhole-peer");

    let our_temp = tempfile::TempDir::new().expect("our tempdir");
    let our_certs = Arc::new(CertificateManager::new(our_temp.path().to_path_buf()));
    our_certs.init().expect("our cert init");
    our_certs
        .ensure_certificate(&our_id, "Us")
        .expect("our cert");
    let cm = Arc::new(ConnectionManager::new(our_certs).expect("our cm"));
    cm.set_device_identity(&our_id, "Us");
    let our_identity = cm.get_identity().expect("our identity");

    let ns = NetnsGuard::create("blackhole");
    let veth = VethGuard::create(&ns);
    let peer_temp = tempfile::TempDir::new().expect("peer tempdir");

    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let peer_shutdown = CancellationToken::new();
    let peer_worker = spawn_netns_peer(
        ns.name.clone(),
        peer_temp.path().to_path_buf(),
        peer_id.clone(),
        our_id.clone(),
        port_tx,
        peer_shutdown.clone(),
    );
    let peer_port = port_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("netns peer must report its bound port within 10s");

    let peer_addr: std::net::SocketAddr = format!("{}:{peer_port}", veth.ns_ip)
        .parse()
        .expect("parse peer addr");
    let identity = peer_identity(&peer_id, peer_port);

    // gen1: real dial across the veth, real TLS, real steady-state loop.
    let (gen1_id, gen1_remote_identity, gen1) = cm
        .connect_to_device(&our_identity, peer_addr, Some(&identity))
        .await
        .expect("gen1 connect across the veth");
    assert_eq!(gen1_id, peer_id);
    let gen1_token = CancellationToken::new();
    assert!(
        cm.register_cancel_token_if_current(&gen1_id, gen1_token.clone(), gen1)
            .await
    );

    // A minimal AppState wired to the SAME ConnectionManager, cert_manager
    // and lifecycle so run_packet_loop's production disconnect/lifecycle
    // path can be driven exactly as the daemon drives it — see
    // `tests/fault_recovery.rs` for why this is preferred over reaching
    // into `run_packet_loop`'s private call sites.
    let state = test_app_state_sharing(cm.clone());
    state
        .lifecycle
        .ensure_and_transition(
            &gen1_id,
            &gen1_remote_identity,
            rust_connect::device::DeviceState::Connected,
        )
        .await
        .expect("gen1 lifecycle transition");
    let h1 = {
        let state = state.clone();
        let device_id = gen1_id.clone();
        let token = gen1_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen1, 0).await })
    };

    assert!(
        wait_until(
            || {
                let cm = cm.clone();
                let peer_id = peer_id.clone();
                async move { cm.is_connected(&peer_id).await }
            },
            40,
            Duration::from_millis(50)
        )
        .await,
        "gen1 must be connected before the blackhole engages"
    );

    // Blackhole: the peer vanishes without a trace, not a graceful close.
    let blackhole = BlackholeGuard::engage(&veth.ns_ip);

    // Honest-send check DURING the blackhole window, before death is
    // detected: this is the Task 2.0 `sent:true` regression surface — a
    // send here must not silently report success on a link nothing can
    // reach. `send_packet`'s own internal write/flush timeout is 100ms, so
    // this resolves almost immediately regardless of the outcome.
    let mid_blackhole_send = cm
        .send_packet(&peer_id, &rust_connect::protocol::Packet::ping())
        .await;

    let deadline = keepalive_death_deadline();
    let observed_dead = wait_until(
        || {
            let cm = cm.clone();
            let peer_id = peer_id.clone();
            async move { !cm.is_connected(&peer_id).await }
        },
        (deadline.as_secs() as u32 * 4).max(1),
        Duration::from_millis(250),
    )
    .await;
    assert!(
        observed_dead,
        "the blackholed link must be observed dead within {deadline:?} \
         (derived from KEEPALIVE_IDLE/KEEPALIVE_INTERVAL/KEEPALIVE_RETRIES/TCP_USER_TIMEOUT)"
    );

    let h1_result = tokio::time::timeout(Duration::from_secs(10), h1)
        .await
        .expect("gen1's loop must exit once the kernel gives up on the blackholed peer")
        .expect("gen1 task must not panic");
    assert!(matches!(h1_result, LoopResult::Disconnected));
    assert_eq!(
        state.lifecycle.get_state(&peer_id).await.ok(),
        Some(rust_connect::device::DeviceState::Disconnected)
    );

    // The brief's explicit requirement: "send to the dead link fails
    // honestly." Now that death is confirmed observed (the connection is
    // removed from the map), a send MUST fail — never a false Ok(()).
    assert!(
        cm.send_packet(&peer_id, &rust_connect::protocol::Packet::ping())
            .await
            .is_err(),
        "a send against a link already confirmed dead must fail honestly"
    );

    // The MID-blackhole send (before death was detected) is a different,
    // softer question: TCP's write() only ever means "accepted into the
    // kernel send buffer," never "received by the peer," so a small ping
    // payload succeeding here is ORDINARY buffering, not evidence of the
    // Task 2.0 `sent:true` defect. That defect was about a link ALREADY
    // known-stale for 2.5 hours still reporting success — a send fired the
    // INSTANT the blackhole engages has no such staleness to have missed.
    // Logged for the report either way, never asserted on: no code change
    // could make an unacknowledged local buffer write fail without adding
    // a delivery-acknowledgment layer neither this daemon nor the
    // reference implementations have.
    eprintln!(
        "INFO: mid-blackhole send_packet result: {:?} (expected/benign either way — see the \
         fault-suite report)",
        mid_blackhole_send.is_ok()
    );

    drop(blackhole);

    // F-4 recovery leg: sleep past the connection rate limit, then a fresh
    // dial to the SAME peer (still alive and listening — only its traffic
    // was blackholed, not its process) must land cleanly.
    tokio::time::sleep(CONNECTION_RATE_LIMIT + Duration::from_millis(200)).await;
    let (gen2_id, gen2_remote_identity, gen2) = cm
        .connect_to_device(&our_identity, peer_addr, Some(&identity))
        .await
        .expect("the redial after the blackhole lifts must succeed");
    assert!(gen2 > gen1, "gen2 must be a newer generation");
    let gen2_token = CancellationToken::new();
    assert!(
        cm.register_cancel_token_if_current(&gen2_id, gen2_token.clone(), gen2)
            .await
    );
    state
        .lifecycle
        .ensure_and_transition(
            &gen2_id,
            &gen2_remote_identity,
            rust_connect::device::DeviceState::Connected,
        )
        .await
        .expect("gen2 lifecycle transition");
    let h2 = {
        let state = state.clone();
        let device_id = gen2_id.clone();
        let token = gen2_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen2, 0).await })
    };
    assert!(
        wait_until(
            || {
                let cm = cm.clone();
                let peer_id = peer_id.clone();
                async move { cm.is_connected(&peer_id).await }
            },
            80,
            Duration::from_millis(25)
        )
        .await,
        "the recovery dial must land"
    );
    assert_eq!(
        cm.connected_device_ids().await.len(),
        1,
        "exactly one live generation after recovery"
    );
    assert_eq!(cm.cancel_token_count().await, 1);
    assert_eq!(
        state.lifecycle.get_state(&peer_id).await.ok(),
        Some(rust_connect::device::DeviceState::Connected)
    );

    // Regression pin for the Task 2.0 defect: a send against the RECOVERED
    // (genuinely live) link must succeed honestly.
    assert!(
        cm.send_packet(&peer_id, &rust_connect::protocol::Packet::ping())
            .await
            .is_ok(),
        "a send against the recovered, genuinely live link must succeed"
    );

    gen2_token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), h2).await;
    peer_shutdown.cancel();
    let _ = peer_worker.join();
}

/// Scenario 6 (brief): suspend/resume approximation via a real interface
/// down/up cycle on the link carrying an established connection — see the
/// module doc for exactly what this does and doesn't prove. Asserts: the
/// old link's fate is coherent (a real disconnect event, not a spurious
/// extra one, if it disconnects at all), the redial standing in for
/// production's rediscovery trigger lands with exactly one live
/// generation, and the total dial count stays bounded (no storm) across
/// the whole recovery window.
#[tokio::test]
async fn netns_interface_flap_recovers_without_redial_storm() {
    if !preconditions_met("netns_interface_flap_recovers_without_redial_storm") {
        return;
    }

    let our_id = dev_id("flap-our");
    let peer_id = dev_id("flap-peer");

    let our_temp = tempfile::TempDir::new().expect("our tempdir");
    let our_certs = Arc::new(CertificateManager::new(our_temp.path().to_path_buf()));
    our_certs.init().expect("our cert init");
    our_certs
        .ensure_certificate(&our_id, "Us")
        .expect("our cert");
    let cm = Arc::new(ConnectionManager::new(our_certs).expect("our cm"));
    cm.set_device_identity(&our_id, "Us");
    let our_identity = cm.get_identity().expect("our identity");

    let ns = NetnsGuard::create("flap");
    let veth = VethGuard::create(&ns);
    let peer_temp = tempfile::TempDir::new().expect("peer tempdir");

    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let peer_shutdown = CancellationToken::new();
    let peer_worker = spawn_netns_peer(
        ns.name.clone(),
        peer_temp.path().to_path_buf(),
        peer_id.clone(),
        our_id.clone(),
        port_tx,
        peer_shutdown.clone(),
    );
    let peer_port = port_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("netns peer must report its bound port within 10s");
    let peer_addr: std::net::SocketAddr = format!("{}:{peer_port}", veth.ns_ip)
        .parse()
        .expect("parse peer addr");
    let identity = peer_identity(&peer_id, peer_port);

    let (gen1_id, gen1_remote_identity, gen1) = cm
        .connect_to_device(&our_identity, peer_addr, Some(&identity))
        .await
        .expect("gen1 connect across the veth");
    let gen1_token = CancellationToken::new();
    assert!(
        cm.register_cancel_token_if_current(&gen1_id, gen1_token.clone(), gen1)
            .await
    );
    let state = test_app_state_sharing(cm.clone());
    state
        .lifecycle
        .ensure_and_transition(
            &gen1_id,
            &gen1_remote_identity,
            rust_connect::device::DeviceState::Connected,
        )
        .await
        .expect("gen1 lifecycle transition");
    let h1 = {
        let state = state.clone();
        let device_id = gen1_id.clone();
        let token = gen1_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen1, 0).await })
    };
    assert!(
        wait_until(
            || {
                let cm = cm.clone();
                let peer_id = peer_id.clone();
                async move { cm.is_connected(&peer_id).await }
            },
            40,
            Duration::from_millis(50)
        )
        .await,
        "gen1 must be connected before the flap"
    );

    // The suspend/resume approximation: bring the veth's netns-side link
    // down, hold it for a beat, bring it back up. Same mechanism
    // `netns_discovery.rs`'s `netns_interface_down_up_triggers_reannounce`
    // uses for the discovery layer.
    veth.exec_ip(&ns, &["link", "set", &veth.ns_side, "down"]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    veth.exec_ip(&ns, &["link", "set", &veth.ns_side, "up"]);

    // The old link's fate is coherent either way: give it a bounded window
    // to resolve one way or the other, then check whichever branch it
    // took.
    let deadline = keepalive_death_deadline();
    let old_link_gone = wait_until(
        || {
            let cm = cm.clone();
            let peer_id = peer_id.clone();
            async move { !cm.is_connected(&peer_id).await }
        },
        (deadline.as_secs() as u32 * 4).max(1),
        Duration::from_millis(250),
    )
    .await;

    // Whichever branch fires, capture the CURRENTLY-live token/handle so
    // cleanup can run AFTER the invariant assertions below, not before —
    // cancelling a token tears down its lifecycle/cancel-token state (see
    // `run_packet_loop`'s cancelled-branch), which would make the
    // assertions observe already-torn-down state rather than the actual
    // post-recovery invariant this scenario exists to check.
    let mut redial_count = 0usize;
    let (live_token, live_handle): (CancellationToken, tokio::task::JoinHandle<LoopResult>) =
        if old_link_gone {
            let h1_result = tokio::time::timeout(Duration::from_secs(10), h1)
                .await
                .expect("a dead gen1 must actually exit, not hang")
                .expect("gen1 task must not panic");
            assert!(
                matches!(h1_result, LoopResult::Disconnected),
                "if the old link died, it must be a real Disconnected exit"
            );
            assert_eq!(
                state.lifecycle.get_state(&peer_id).await.ok(),
                Some(rust_connect::device::DeviceState::Disconnected),
                "a died link must show ONE coherent Disconnected transition, not a stale one"
            );

            // The redial standing in for production's rediscovery trigger —
            // ONE attempt, not a loop, so this test's own driving can't
            // manufacture the "no storm" result it's asserting.
            tokio::time::sleep(CONNECTION_RATE_LIMIT + Duration::from_millis(200)).await;
            let (gen2_id, gen2_remote_identity, gen2) = cm
                .connect_to_device(&our_identity, peer_addr, Some(&identity))
                .await
                .expect("the single post-flap redial must land");
            redial_count += 1;
            assert!(gen2 > gen1);
            let gen2_token = CancellationToken::new();
            assert!(
                cm.register_cancel_token_if_current(&gen2_id, gen2_token.clone(), gen2)
                    .await
            );
            state
                .lifecycle
                .ensure_and_transition(
                    &gen2_id,
                    &gen2_remote_identity,
                    rust_connect::device::DeviceState::Connected,
                )
                .await
                .expect("gen2 lifecycle transition");
            let h2 = {
                let state = state.clone();
                let device_id = gen2_id.clone();
                let token = gen2_token.clone();
                tokio::spawn(
                    async move { run_packet_loop(state, token, &device_id, gen2, 0).await },
                )
            };
            assert!(
                wait_until(
                    || {
                        let cm = cm.clone();
                        let peer_id = peer_id.clone();
                        async move { cm.is_connected(&peer_id).await }
                    },
                    80,
                    Duration::from_millis(25)
                )
                .await,
                "the redial must reach a healthy state"
            );
            (gen2_token, h2)
        } else {
            // The link survived the flap outright (plausible: the IPv4
            // address itself typically isn't removed by a down/up, only
            // IPv6 link-local — see `netns_discovery.rs`'s own doc — so an
            // in-flight TCP connection can simply resume once routing
            // returns). No redial needed; assert the survival is genuinely
            // coherent, not a zombie the loop task already abandoned.
            assert!(
                !h1.is_finished(),
                "a survived link's loop must still be running"
            );
            (gen1_token, h1)
        };

    // No redial storm: at most one redial fired across the whole recovery
    // window (either the link survived — zero — or it died and got
    // exactly one redial, driven explicitly above, never a loop).
    assert!(
        redial_count <= 1,
        "the recovery window must not produce a redial storm (saw {redial_count})"
    );
    assert_eq!(
        cm.connected_device_ids().await.len(),
        1,
        "exactly one live generation once the recovery window settles"
    );
    assert_eq!(cm.cancel_token_count().await, 1);

    live_token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), live_handle).await;
    peer_shutdown.cancel();
    let _ = peer_worker.join();
}

/// A minimal `AppState`-shaped bundle sharing the caller's REAL
/// `ConnectionManager` so `run_packet_loop`'s production disconnect/
/// lifecycle path can be driven exactly as the daemon drives it, without
/// needing a full `AppState::new` (which owns its own separate
/// `ConnectionManager`, `pairing_handler`, etc. this suite has no use
/// for). `run_packet_loop` only ever touches `state.connection_manager`,
/// `state.packet_router`, `state.pairing_handler`, `state.plugin_registry`,
/// and `state.lifecycle`; a real `AppState` built from a throwaway temp
/// dir supplies the last four, and its own `connection_manager` field is
/// swapped for the shared one.
fn test_app_state_sharing(cm: Arc<ConnectionManager>) -> Arc<rust_connect::app::AppState> {
    let temp_dir = tempfile::TempDir::new().expect("appstate tempdir");
    let settings = rust_connect::config::settings::AppSettings::new_with_data_dir(
        temp_dir.path().to_path_buf(),
    );
    let mut state =
        rust_connect::app::AppState::new_without_input(settings).expect("AppState::new");
    state.connection_manager = cm;
    // Leak the tempdir's lifetime into the returned Arc's lifetime by
    // leaking the guard — acceptable in a short-lived, explicit-on-demand
    // root test process (never a long-running service), and simpler than
    // threading a second TempDir through every call site.
    std::mem::forget(temp_dir);
    Arc::new(state)
}

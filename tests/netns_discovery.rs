//! Network-namespace integration suite (Task 2.2 piece 4, vk #994)
//!
//! There are no netns tests anywhere else in this repo — this file
//! establishes the convention. Root-only (needs `CAP_NET_ADMIN` to
//! create namespaces and veth pairs), explicit on-demand, and gated
//! the SAME visible-skip way the Xvfb (`tests/clipboard_x11.rs`) and
//! dbus (`tests/mpris_bus_recovery.rs`) real-backend suites already
//! are: print the reason to stderr and return normally (a PASS, not a
//! silent CI no-op) when the precondition isn't met. Per the task
//! plan's own Prerequisites section, root-only coverage must never be
//! silent CI coverage — a developer or CI run without root sees an
//! explicit skip line, not a suite that quietly ran zero assertions.
//!
//! Run explicitly as root (integrator-verified form — a bare
//! `sudo -E cargo` hits the rustup SHIM as root, which has no default
//! toolchain configured and fails before compiling anything):
//! ```text
//! TOOLBIN=$(dirname "$(rustup which rustc)") && \
//!   sudo env PATH="$TOOLBIN:$PATH" HOME="$HOME" CARGO_HOME="$HOME/.cargo" \
//!   "$TOOLBIN/cargo" test --test netns_discovery --locked
//! ```
//! Non-root runs of the ordinary `cargo test` suite pass this file
//! cleanly with three skip lines.
//!
//! ## Why real netns + veth, not mocked interfaces
//!
//! `network_watcher`'s address-change detection (Task 2.2 piece 1) is a
//! thin wrapper over real netlink sockets (`if-watch`); the only way to
//! prove it observes a REAL interface transition is to give it one.
//! Each test here creates an isolated `ip netns` namespace with a veth
//! pair, runs the actual production code (`network_watcher`,
//! `DiscoveryService`, `services::broadcast_fallback`) inside that
//! namespace via `setns(2)`, mutates the namespace's network state from
//! OUTSIDE via `ip netns exec` (which performs its own `setns` before
//! exec'ing — the standard, well-documented way to run a command in a
//! namespace, rather than relying on fork-inherits-caller-thread-
//! namespace semantics), and observes the resulting behavior over a
//! real UDP socket bound inside that same namespace.
//!
//! `setns(2)` reassigns only the CALLING THREAD's network namespace,
//! not the whole process (man setns(2)) — so each test's "namespace
//! worker" is a dedicated `std::thread`, never a tokio worker thread
//! that might migrate under a multi-threaded runtime. The worker builds
//! its own `current_thread` tokio runtime after the `setns` call, so
//! every future it drives (the code under test AND the observing
//! socket) stays pinned to that one already-namespaced OS thread for
//! its whole lifetime.

use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use rust_connect::device::types::DeviceType;
use rust_connect::protocol::types::Identity;
use rust_connect::protocol::DiscoveryService;
use rust_connect::services::{broadcast_fallback, network_watcher};
use tokio_util::sync::CancellationToken;

/// Every veth/address in this file is namespaced by this counter so
/// concurrently-running tests (Rust runs `#[test]` fns on a thread pool
/// by default) never collide on a host-side interface name or IPv4
/// range, even though each test also gets its own `ip netns`.
static SEQ: AtomicU32 = AtomicU32::new(1);

/// True when running as euid 0. Namespace creation needs
/// `CAP_NET_ADMIN`; this file takes the same practical shortcut the
/// brief's "root-only" framing does rather than attempting a
/// capability-only path.
fn is_root() -> bool {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn ip_on_path() -> bool {
    Command::new("ip").arg("-Version").output().is_ok()
}

/// Shared precondition gate for every test in this file. Returns
/// `true` when the test should proceed; otherwise prints the skip
/// reason to stderr (visible, per the module doc) and returns `false`.
fn preconditions_met(test_name: &str) -> bool {
    if !is_root() {
        eprintln!(
            "{test_name}: not running as root — skipping (netns/veth creation needs \
             CAP_NET_ADMIN; run under sudo to exercise this suite)"
        );
        return false;
    }
    if !ip_on_path() {
        eprintln!("{test_name}: `ip` (iproute2) not on PATH — skipping");
        return false;
    }
    // euid 0 does not guarantee namespace privileges (a restricted-root
    // container can lack CAP_NET_ADMIN / CAP_SYS_ADMIN) — probe the real
    // operation rather than trusting the uid, so such environments get
    // the documented visible skip instead of a panic (PR #13 review).
    // SEQ-suffixed: the three tests run concurrently and all call this
    // gate — a shared probe name collides ("File exists") and silently
    // skips a valid test (PR #13 review, on the previous fixup).
    let probe = format!(
        "rcprobe-{}-{}",
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
            "{test_name}: root but `ip netns add` failed (restricted container, \
             missing CAP_NET_ADMIN?) — skipping"
        );
        false
    }
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = cmd.stdin(Stdio::null()).output().unwrap_or_else(|e| {
        panic!("spawn {cmd:?}: {e}");
    });
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Best-effort — used only during cleanup, where a namespace or link
/// already gone (a prior partial failure, a concurrent run) must not
/// mask the real test failure with a panic from `Drop`.
fn run_best_effort(cmd: &mut Command) {
    let _ = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output();
}

/// Owns an `ip netns` namespace; deletes it on drop.
struct NetnsGuard {
    name: String,
}

impl NetnsGuard {
    fn create(label: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let name = format!("rcns-{label}-{}-{n}", std::process::id());
        run_ok(Command::new("ip").args(["netns", "add", &name]));
        Self { name }
    }
}

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        run_best_effort(Command::new("ip").args(["netns", "del", &self.name]));
    }
}

/// Owns a veth pair with the namespace-side end moved into `ns`,
/// addressed and brought up on both ends (plus loopback inside the
/// namespace). Deleting the host-side end also removes the peer.
struct VethGuard {
    host_side: String,
    /// Name of the peer as seen FROM INSIDE the namespace — this is
    /// what `ip netns exec <ns> ip link set <ns_side> down/up` and
    /// similar mutations target.
    ns_side: String,
    /// The host-side address, kept for re-adding the default route
    /// after a down/up cycle — see `restore_default_route`'s doc.
    host_ip: String,
}

impl VethGuard {
    fn create(ns: &NetnsGuard) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        // IFNAMSIZ is 16 bytes including the NUL — stay well under it.
        let host_side = format!("rcv{n}h");
        let ns_side = format!("rcv{n}n");
        let host_ip = format!("10.250.{}.1", n % 250);
        let host_addr = format!("{host_ip}/24");
        let ns_addr = format!("10.250.{}.2/24", n % 250);

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
        // A 255.255.255.255 send needs a route to exist at all — Linux's
        // route lookup for the limited-broadcast address doesn't fall
        // back to "any interface" without one (verified empirically
        // this session: `ip route show` inside a bare directly-
        // connected-only namespace has no matching route, and a plain
        // UDP sendto(255.255.255.255) there fails ENETUNREACH). The
        // directly-connected /24 route `ip addr add` creates isn't
        // enough on its own; a default route resolves it.
        run_ok(Command::new("ip").args([
            "netns", "exec", &ns.name, "ip", "route", "add", "default", "via", &host_ip,
        ]));

        Self {
            host_side,
            ns_side,
            host_ip,
        }
    }

    /// Runs `ip netns exec <ns> ip <args...>` — the standard way to
    /// mutate namespace state from outside it. Panics on failure (used
    /// for the trigger step itself, which the test's correctness
    /// depends on actually taking effect).
    fn exec_ip(&self, ns: &NetnsGuard, args: &[&str]) {
        let mut full = vec!["netns", "exec", ns.name.as_str(), "ip"];
        full.extend_from_slice(args);
        run_ok(Command::new("ip").args(&full));
    }

    /// Re-adds the default route after a link down/up cycle. Verified
    /// empirically this session: bringing the link down removes the
    /// manually-added default route (its nexthop becomes unreachable
    /// and the kernel drops it), and it does NOT come back on its own
    /// when the link returns — only the directly-connected /24 route
    /// does. A real DHCP client restores the default route as part of
    /// reconnecting too, so this is faithful to what "the network came
    /// back" actually means, not a shortcut around the test.
    fn restore_default_route(&self, ns: &NetnsGuard) {
        self.exec_ip(ns, &["route", "add", "default", "via", &self.host_ip]);
    }
}

impl Drop for VethGuard {
    fn drop(&mut self) {
        // Deleting the host-side end tears down the whole pair,
        // including the namespace-side peer — no separate cleanup for
        // ns_side needed even if the namespace itself outlives this.
        let _ = &self.ns_side; // referenced for clarity only; see above
        run_best_effort(Command::new("ip").args(["link", "del", &self.host_side]));
    }
}

/// Runs `f` on a dedicated OS thread that has `setns(2)`'d into network
/// namespace `ns_name` BEFORE building its own single-threaded tokio
/// runtime — see the module doc for why both of those are load-bearing.
/// Blocks the calling thread until `f`'s future (and everything spawned
/// on that runtime) completes, returning its output.
fn run_in_netns<F, Fut, T>(ns_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    let ns_path = format!("/var/run/netns/{ns_name}");
    let ns_name_owned = ns_name.to_string();
    std::thread::Builder::new()
        .name(format!("netns-{ns_name_owned}"))
        .spawn(move || {
            use std::os::unix::io::AsRawFd;
            let ns_file =
                std::fs::File::open(&ns_path).unwrap_or_else(|e| panic!("open {ns_path}: {e}"));
            let fd = ns_file.as_raw_fd();
            // SAFETY: `fd` is a valid, open file descriptor referring to
            // a network-namespace inode (opened just above) for the
            // duration of this call. `setns` with `CLONE_NEWNET`
            // reassigns only the CALLING THREAD's network namespace
            // (man setns(2)) — this thread is freshly spawned and does
            // nothing namespace-sensitive before this call.
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

/// The identity's `tcp_port` is metadata carried IN the packet (what a
/// peer would dial), unrelated to which UDP port this test's
/// `DiscoveryService` actually binds/broadcasts on — but
/// `DiscoveryService::listen`'s own guard (mirroring android
/// `LanLinkProvider.java:236-240`) drops any identity whose `tcpPort`
/// falls outside kdeconnect's 1716-1764 range, so it must be a real
/// in-range value, not the arbitrary discovery port this test picks
/// per scenario to keep sockets apart.
fn test_identity(name: &str) -> Identity {
    let mut identity = Identity::new(
        Identity::generate_device_id(),
        name.to_string(),
        DeviceType::Desktop,
        vec![],
        vec![],
    );
    identity.tcp_port = Some(rust_connect::protocol::types::MIN_PORT);
    identity
}

/// Scenario 1 (brief): interface down/up -> re-announce observed.
///
/// The mechanism, verified empirically this session (see
/// `plans/task-2.2-report.md`): bringing a veth end down REMOVES its
/// IPv6 link-local address entirely; bringing it back up re-adds it.
/// `if-watch` subscribes to IPv6 address groups too (`network_watcher`
/// module doc), so this address churn — not any direct RTMGRP_LINK
/// subscription, which `if-watch` doesn't have — is the actual
/// mechanism behind "interface up/down surfaces as address changes on
/// Linux in practice." This test is the live proof of that claim, not
/// an assumption.
#[test]
fn netns_interface_down_up_triggers_reannounce() {
    if !preconditions_met("netns_interface_down_up_triggers_reannounce") {
        return;
    }

    let ns = NetnsGuard::create("linkflap");
    let veth = VethGuard::create(&ns);

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let ns_name = ns.name.clone();

    let worker = std::thread::spawn(move || {
        run_in_netns(&ns_name, move || async move {
            let port = 17161u16;
            let sender_identity = test_identity("netns-flap-sender");
            let listener_identity = test_identity("netns-flap-listener");

            let sender = DiscoveryService::new(sender_identity, port)
                .await
                .expect("bind sender DiscoveryService inside the netns");
            let listener = DiscoveryService::new(listener_identity, port)
                .await
                .expect("bind listener DiscoveryService inside the netns (SO_REUSEADDR)");

            // Startup announce + drain, matching production's
            // announce-on-start (piece 3) — keeps the flap-triggered
            // broadcast unambiguous from the startup one.
            sender
                .broadcast()
                .await
                .expect("startup broadcast must succeed");
            tokio::time::timeout(Duration::from_secs(3), listener.listen())
                .await
                .expect("startup broadcast must be observed")
                .expect("startup broadcast must parse");

            // Wire the SAME reaction piece 2 wires in service_manager
            // (minus mDNS, out of scope for this scenario): a debounced
            // NetworkChanged event re-broadcasts once.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let shutdown = CancellationToken::new();
            tokio::spawn(network_watcher::watch_network_changes(tx, shutdown.clone()));
            let reactor_sender = sender;
            tokio::spawn(async move {
                while rx.recv().await.is_some() {
                    let _ = reactor_sender.broadcast().await;
                }
            });

            // Let the spawned watcher actually reach its netlink
            // subscription before declaring ready: tokio::spawn only
            // QUEUES the task, and a flap that lands before IfWatcher::new
            // subscribes is silently unobservable (PR #13 review). There
            // is no ready-handshake in the production API (production
            // starts the watcher at daemon boot, ages before any change),
            // so a settle window stands in: subscription is one
            // synchronous netlink socket setup, sub-millisecond in
            // practice — 300ms is orders of magnitude of margin.
            tokio::time::sleep(Duration::from_millis(300)).await;
            ready_tx.send(()).expect("send ready signal");

            // Bounded wait for the re-announce. DEBOUNCE_WINDOW (750ms)
            // plus real netlink delivery latency plus the down/up pair
            // itself easily fits in 15s; this is generous, not tight,
            // since the suite is explicit on-demand, not a hot loop.
            let result = tokio::time::timeout(Duration::from_secs(15), listener.listen()).await;

            shutdown.cancel();
            result
        })
    });

    // Wait for the worker's setup to finish, then flap the link from
    // outside the netns'd thread via `ip netns exec` (see the module
    // doc for why this, not a Command spawned from inside the worker).
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("netns worker must signal ready within 10s");

    veth.exec_ip(&ns, &["link", "set", &veth.ns_side, "down"]);
    std::thread::sleep(Duration::from_millis(300));
    veth.exec_ip(&ns, &["link", "set", &veth.ns_side, "up"]);
    veth.restore_default_route(&ns);

    let result = worker.join().expect("netns worker thread panicked");
    let (identity, _addr) = result
        .expect("re-announce must be observed within the bounded wait")
        .expect("the observed broadcast must parse as a valid identity");
    assert_eq!(
        identity.device_name, "netns-flap-sender",
        "the re-announce must carry OUR identity, not a stray packet"
    );
}

/// Scenario 2 (brief): address change -> re-announce observed.
///
/// Distinct from scenario 1: this adds a genuinely NEW address on top
/// of the existing one, an unambiguous, directly-observed
/// `RTM_NEWADDR` — no reliance on the link-down/up side effect.
#[test]
fn netns_address_change_triggers_reannounce() {
    if !preconditions_met("netns_address_change_triggers_reannounce") {
        return;
    }

    let ns = NetnsGuard::create("addrchg");
    let veth = VethGuard::create(&ns);

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let ns_name = ns.name.clone();

    let worker = std::thread::spawn(move || {
        run_in_netns(&ns_name, move || async move {
            let port = 17162u16;
            let sender_identity = test_identity("netns-addr-sender");
            let listener_identity = test_identity("netns-addr-listener");

            let sender = DiscoveryService::new(sender_identity, port)
                .await
                .expect("bind sender DiscoveryService inside the netns");
            let listener = DiscoveryService::new(listener_identity, port)
                .await
                .expect("bind listener DiscoveryService inside the netns (SO_REUSEADDR)");

            sender
                .broadcast()
                .await
                .expect("startup broadcast must succeed");
            tokio::time::timeout(Duration::from_secs(3), listener.listen())
                .await
                .expect("startup broadcast must be observed")
                .expect("startup broadcast must parse");

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let shutdown = CancellationToken::new();
            tokio::spawn(network_watcher::watch_network_changes(tx, shutdown.clone()));
            let reactor_sender = sender;
            tokio::spawn(async move {
                while rx.recv().await.is_some() {
                    let _ = reactor_sender.broadcast().await;
                }
            });

            // Same settle-before-ready as the flap test above (PR #13
            // review): let the watcher reach its netlink subscription
            // before the main thread mutates addresses.
            tokio::time::sleep(Duration::from_millis(300)).await;
            ready_tx.send(()).expect("send ready signal");

            let result = tokio::time::timeout(Duration::from_secs(15), listener.listen()).await;
            shutdown.cancel();
            result
        })
    });

    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("netns worker must signal ready within 10s");

    // A genuinely new address, not a re-add of the existing one.
    let new_addr_n = SEQ.fetch_add(1, Ordering::SeqCst);
    let new_addr = format!("10.251.{}.9/24", new_addr_n % 250);
    veth.exec_ip(&ns, &["addr", "add", &new_addr, "dev", &veth.ns_side]);

    let result = worker.join().expect("netns worker thread panicked");
    let (identity, _addr) = result
        .expect("re-announce must be observed within the bounded wait")
        .expect("the observed broadcast must parse as a valid identity");
    assert_eq!(identity.device_name, "netns-addr-sender");
}

/// Scenario 3 (brief): mDNS-down fallback cadence — observe two
/// backoff intervals, bounded wall-clock, via the injectable-interval
/// seam (`broadcast_fallback::run_fallback_schedule`) rather than the
/// multi-minute production constants (`FALLBACK_INITIAL_INTERVAL` =
/// 5s, `FALLBACK_MAX_INTERVAL` = 5min — waiting those out for real
/// would make this suite impractical to actually run). This scenario
/// doesn't depend on netlink/interface events at all, but runs inside
/// the same netns machinery as the other two for consistency and to
/// keep its UDP traffic isolated from the host.
#[test]
fn netns_mdns_down_fallback_observes_two_backoff_intervals() {
    if !preconditions_met("netns_mdns_down_fallback_observes_two_backoff_intervals") {
        return;
    }

    let ns = NetnsGuard::create("fallback");
    let _veth = VethGuard::create(&ns);
    let ns_name = ns.name.clone();

    let worker = std::thread::spawn(move || {
        run_in_netns(&ns_name, move || async move {
            let port = 17163u16;
            let sender_identity = test_identity("netns-fallback-sender");
            let listener_identity = test_identity("netns-fallback-listener");

            let sender = std::sync::Arc::new(
                DiscoveryService::new(sender_identity, port)
                    .await
                    .expect("bind sender DiscoveryService inside the netns"),
            );
            let listener = DiscoveryService::new(listener_identity, port)
                .await
                .expect("bind listener DiscoveryService inside the netns (SO_REUSEADDR)");

            let shutdown = CancellationToken::new();
            let schedule_shutdown = shutdown.clone();
            let schedule_sender = sender.clone();
            // Injected short intervals via the test seam — NOT
            // broadcast_fallback::FALLBACK_INITIAL_INTERVAL /
            // FALLBACK_MAX_INTERVAL, which are production-sized
            // (5s / 5min). "Always ineligible-if-not-called" doesn't
            // apply here: this test drives the schedule directly with
            // an always-eligible closure, since mDNS-down-ness itself
            // is exercised at the service_manager wiring level (piece
            // 3's unit tests), not this suite's job — this scenario's
            // job is proving the CADENCE holds over a REAL socket.
            tokio::spawn(async move {
                broadcast_fallback::run_fallback_schedule(
                    || async { true },
                    move || {
                        let sender = schedule_sender.clone();
                        async move {
                            let _ = sender.broadcast().await;
                        }
                    },
                    schedule_shutdown,
                    // DISTINCT initial/max so the doubling leg is
                    // exercised end-to-end, not just in the paused-time
                    // unit tests: expected real gaps ~200ms, ~400ms,
                    // ~800ms(cap) (PR #13 review — with initial == max a
                    // regression stuck at the initial interval forever
                    // still passed this test).
                    Duration::from_millis(200),
                    Duration::from_millis(800),
                    Duration::from_millis(100),
                )
                .await;
            });

            // Observe four broadcasts: the immediate one, then three
            // more on the doubling schedule (~200ms, ~400ms, ~800ms-cap
            // gaps) — real-socket confirmation that the backoff actually
            // GROWS, complementing the paused-time unit tests.
            let mut timestamps = Vec::new();
            for _ in 0..4 {
                tokio::time::timeout(Duration::from_secs(5), listener.listen())
                    .await
                    .expect("each scheduled broadcast must arrive within 5s")
                    .expect("broadcast must parse");
                timestamps.push(tokio::time::Instant::now());
            }
            shutdown.cancel();
            timestamps
        })
    });

    let timestamps = worker.join().expect("netns worker thread panicked");
    assert_eq!(timestamps.len(), 4);
    let gap1 = timestamps[1] - timestamps[0];
    let gap2 = timestamps[2] - timestamps[1];
    let gap3 = timestamps[3] - timestamps[2];
    // Real wall-clock cadence: assert the SHAPE (each gap meaningfully
    // larger than the last, i.e. the schedule grows) with generous slack
    // bands rather than exact values — timer coalescing and scheduler
    // noise on a live kernel make exact-equality flaky, but a regression
    // stuck at the initial interval cannot produce growing gaps.
    assert!(
        gap1 >= Duration::from_millis(120) && gap1 <= Duration::from_millis(600),
        "first backoff gap was {gap1:?}, expected ~200ms"
    );
    assert!(
        gap2 > gap1 && gap2 >= Duration::from_millis(280),
        "second gap ({gap2:?}) must grow past the first ({gap1:?}) — doubling"
    );
    assert!(
        gap3 > gap2 && gap3 >= Duration::from_millis(560) && gap3 <= Duration::from_secs(3),
        "third gap ({gap3:?}) must grow to the ~800ms cap (was {gap2:?} before)"
    );
}

//! In-process fault-recovery scenarios for Task 2.4 (vk #990): duplicate-
//! dial storm, delayed stale-loop cleanup, peer restart, and daemon
//! restart. None of these need root or a network namespace, so they run in
//! ordinary `cargo test`. The keepalive-blackhole and suspend/resume
//! scenarios genuinely need real network-fault injection (a silent
//! blackhole, an interface down/up) and live in the root-gated
//! `tests/fault_suite.rs` instead — see that file's header for why and how
//! to run it.
//!
//! Every scenario drives the REAL production entry points
//! (`connect_to_device`, `register_cancel_token_if_current`,
//! `ensure_and_transition`, `run_packet_loop`, `spawn_discovered_connection`)
//! rather than reimplementing their logic, so a regression in the actual
//! guard code trips these tests, not a shadow copy of it. The peer side
//! reuses the existing test-only TLS harness
//! (`ConnectionManager::accept_test_as_client`, gated on `#[cfg(any(test,
//! feature = "test-helpers"))]`) already exercised by
//! `src/services/connection_orchestrator.rs`'s own test module, rather than
//! hand-rolling a second TLS peer implementation.
//!
//! Scenario placement note: the task brief groups scenario 4 (peer restart)
//! with the root-only netns file by default numbering, but its production
//! assertions (generation cleanup, one live generation, Connected with a
//! fresh `last_seen`) don't actually need a network namespace — an
//! in-process peer restart (fresh `ConnectionManager` loading the SAME
//! on-disk certs, real TCP close in between) exercises the identical
//! guard code. Keeping it here means ordinary CI runs it instead of only
//! the root-gated suite; flagged explicitly for the integrator rather than
//! silently deviating from the brief's file layout.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use rust_connect::device::{DeviceState, DeviceType};
use rust_connect::protocol::connection_loop::{run_packet_loop, LoopResult};
use rust_connect::protocol::{CertificateManager, ConnectionManager, Identity};
use rust_connect::services::connection_orchestrator::spawn_discovered_connection;
use tokio_util::sync::CancellationToken;

fn create_test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
    let state = Arc::new(AppState::new_without_input(settings).expect("AppState::new"));
    (state, temp_dir)
}

/// Poll an async condition until it holds or ~2s elapse (same shape as
/// `tests/protocol_integration.rs`'s helper of the same name — duplicated
/// rather than shared because integration test binaries are separate
/// crates with no existing shared-support convention in this repo).
async fn wait_until<F, Fut>(mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..80 {
        if cond().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

/// A device id in `crypto::validate_device_id`'s 32-38 char window, unique
/// enough per scenario/role to avoid collisions between certs.
fn dev_id(tag: &str) -> String {
    let mut id = format!("f24-{tag}-");
    while id.len() < 34 {
        id.push('a');
    }
    id.truncate(34);
    id
}

/// Peer-side acceptor: a `ConnectionManager` that answers our outbound
/// dials the way `spawn_discovered_connection` expects a real peer to
/// (`accept_test_as_client`'s existing test-only role — see
/// `connection_orchestrator.rs`'s own tests). Loops so more than one dial
/// can land, and counts every TCP accept so a scenario can assert a
/// duplicate dial never even reached the wire.
struct PeerAcceptor {
    handle: tokio::task::JoinHandle<()>,
    accept_count: Arc<AtomicUsize>,
    addr: std::net::SocketAddr,
}

async fn spawn_peer_acceptor(peer_cm: Arc<ConnectionManager>, our_id: String) -> PeerAcceptor {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind peer listener");
    let addr = listener.local_addr().expect("local_addr");
    let accept_count = Arc::new(AtomicUsize::new(0));
    let count = accept_count.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            let peer_cm = peer_cm.clone();
            let our_id = our_id.clone();
            tokio::spawn(async move {
                let _ = peer_cm
                    .accept_test_as_client(our_id, stream, addr.port())
                    .await;
            });
        }
    });
    PeerAcceptor {
        handle,
        accept_count,
        addr,
    }
}

/// `connect_to_device` rate-limits successive dials to the SAME remote IP
/// (keyed by IP alone, not IP+port) to `CONNECTION_RATE_LIMIT`. Every
/// scenario here redials 127.0.0.1, so a redial fired inside that window
/// fails with a rate-limit error unrelated to what the scenario is
/// actually testing — pace off the real constant, plus slack, rather than
/// a guessed sleep that could silently fall out of sync with it.
async fn sleep_past_rate_limit() {
    tokio::time::sleep(
        rust_connect::protocol::connection::CONNECTION_RATE_LIMIT
            + std::time::Duration::from_millis(100),
    )
    .await;
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

/// Scenario 2 (brief): N rapid same-identity dials against an already
/// HEALTHY held link. `spawn_discovered_connection`'s `is_connected()`
/// early-return (checked BEFORE the pending-connection set is even
/// touched) is the actual dedup mechanism in production — this drives that
/// real function, not a reimplementation of its guard.
#[tokio::test]
async fn test_duplicate_dial_storm_against_healthy_link_is_deduped() {
    let (state, _temp) = create_test_state();
    let our_id = dev_id("storm-our");
    let peer_id = dev_id("storm-peer");

    state.connection_manager.set_device_identity(&our_id, "Us");
    state
        .cert_manager
        .ensure_certificate(&our_id, "Us")
        .expect("our cert");

    let peer_temp = tempfile::TempDir::new().expect("peer tempdir");
    let peer_certs = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
    peer_certs.init().expect("peer cert init");
    peer_certs
        .ensure_certificate(&peer_id, "Peer")
        .expect("peer cert");
    peer_certs
        .ensure_certificate(&our_id, "Us")
        .expect("peer's copy of our cert");
    let peer_cm = Arc::new(ConnectionManager::new(peer_certs).expect("peer cm"));
    peer_cm.set_device_identity(&peer_id, "Peer");

    let acceptor = spawn_peer_acceptor(peer_cm, our_id.clone()).await;
    let identity = peer_identity(&peer_id, acceptor.addr.port());

    // gen1: establish a real, healthy link via the production dial path.
    spawn_discovered_connection(state.clone(), identity.clone(), acceptor.addr);
    assert!(
        wait_until(|| {
            let state = state.clone();
            let peer_id = peer_id.clone();
            async move { state.connection_manager.is_connected(&peer_id).await }
        })
        .await,
        "the first dial must establish a healthy link"
    );
    let gen1 = state
        .connection_manager
        .get_generation(&peer_id)
        .await
        .expect("gen1");
    assert_eq!(
        acceptor.accept_count.load(Ordering::SeqCst),
        1,
        "exactly one TCP dial for the first connect"
    );

    // Storm: N rapid same-identity dials while the link is healthy.
    const STORM_SIZE: usize = 25;
    for _ in 0..STORM_SIZE {
        spawn_discovered_connection(state.clone(), identity.clone(), acceptor.addr);
    }

    // Settle: the storm's spawned tasks resolve almost instantly (a single
    // is_connected() check, no I/O) — this window is generous, not tight.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_eq!(
        acceptor.accept_count.load(Ordering::SeqCst),
        1,
        "the storm must never reach the wire — is_connected() in \
         spawn_discovered_connection must dedup every duplicate before it dials"
    );
    assert_eq!(
        state.connection_manager.get_generation(&peer_id).await,
        Some(gen1),
        "generation must be unchanged — no replacement occurred"
    );
    assert_eq!(
        state.connection_manager.connected_device_ids().await.len(),
        1,
        "connections map must hold exactly one entry"
    );
    assert_eq!(
        state.connection_manager.cancel_token_count().await,
        1,
        "cancel_tokens map must hold exactly one entry — no churn from the storm"
    );
    assert!(
        state
            .connection_manager
            .pending_connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty(),
        "pending_connections must be empty once the storm settles"
    );
    assert_eq!(
        state.lifecycle.get_state(&peer_id).await.ok(),
        Some(DeviceState::Connected),
        "lifecycle must still read Connected — no churn events"
    );

    acceptor.handle.abort();
}

/// Scenario 3 (brief): delayed stale-loop cleanup. Forces a same-device
/// redial while the OLD generation's packet loop is genuinely parked on a
/// live read (steady state, past identity exchange — distinct from
/// `connection_orchestrator::tests::test_run_connection_setup_rejects_stale_generation`
/// and `listener.rs`'s exchange-failure tests, which drive the guard
/// functions directly or fail mid-handshake, never through the actual
/// established `run_packet_loop` racing a real replacement). gen2's setup
/// runs to completion and this test sleeps for half a second BEFORE
/// asserting anything — real wall-clock time for gen1's belated cleanup to
/// run in the background, the "delayed teardown" this scenario is named
/// for — then asserts the generation-guard invariant held throughout.
#[tokio::test]
async fn test_delayed_stale_loop_cleanup_cannot_strip_replacement_state() {
    let (state, _temp) = create_test_state();
    let our_id = dev_id("stale-our");
    let peer_id = dev_id("stale-peer");

    state.connection_manager.set_device_identity(&our_id, "Us");
    state
        .cert_manager
        .ensure_certificate(&our_id, "Us")
        .expect("our cert");

    let peer_temp = tempfile::TempDir::new().expect("peer tempdir");
    let peer_certs = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
    peer_certs.init().expect("peer cert init");
    peer_certs
        .ensure_certificate(&peer_id, "Peer")
        .expect("peer cert");
    peer_certs
        .ensure_certificate(&our_id, "Us")
        .expect("peer's copy of our cert");
    let peer_cm = Arc::new(ConnectionManager::new(peer_certs).expect("peer cm"));
    peer_cm.set_device_identity(&peer_id, "Peer");

    let acceptor = spawn_peer_acceptor(peer_cm, our_id.clone()).await;
    let identity = peer_identity(&peer_id, acceptor.addr.port());
    let our_identity = state
        .connection_manager
        .get_identity()
        .expect("our identity");

    // gen1: real dial + setup + packet loop, held in steady state (the peer
    // never sends anything after the handshake, so the loop blocks on a
    // genuine read).
    let (gen1_id, gen1_remote_identity, gen1) = state
        .connection_manager
        .connect_to_device(&our_identity, acceptor.addr, Some(&identity))
        .await
        .expect("gen1 connect");
    assert_eq!(gen1_id, peer_id);

    let gen1_token = CancellationToken::new();
    assert!(
        state
            .connection_manager
            .register_cancel_token_if_current(&gen1_id, gen1_token.clone(), gen1)
            .await,
        "gen1 token registration must succeed on a fresh connection"
    );
    state
        .lifecycle
        .ensure_and_transition(&gen1_id, &gen1_remote_identity, DeviceState::Connected)
        .await
        .expect("gen1 lifecycle transition");
    let h1 = {
        let state = state.clone();
        let device_id = gen1_id.clone();
        let token = gen1_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen1, 0).await })
    };

    assert!(
        wait_until(|| {
            let state = state.clone();
            let peer_id = peer_id.clone();
            async move {
                state
                    .lifecycle
                    .get_state(&peer_id)
                    .await
                    .is_ok_and(|s| s == DeviceState::Connected)
            }
        })
        .await,
        "gen1 must reach Connected"
    );

    // gen2: same-device redial — connect_to_device's own replace path
    // cancels gen1's token and shuts down its write half SYNCHRONOUSLY, but
    // gen1's task only notices once tokio actually schedules it.
    sleep_past_rate_limit().await;
    let (gen2_id, gen2_remote_identity, gen2) = state
        .connection_manager
        .connect_to_device(&our_identity, acceptor.addr, Some(&identity))
        .await
        .expect("gen2 connect");
    assert!(gen2 > gen1, "gen2 must be a newer generation");

    let gen2_token = CancellationToken::new();
    assert!(
        state
            .connection_manager
            .register_cancel_token_if_current(&gen2_id, gen2_token.clone(), gen2)
            .await,
        "gen2 token registration must succeed immediately after the replace"
    );
    state
        .lifecycle
        .ensure_and_transition(&gen2_id, &gen2_remote_identity, DeviceState::Connected)
        .await
        .expect("gen2 lifecycle transition");
    let h2 = {
        let state = state.clone();
        let device_id = gen2_id.clone();
        let token = gen2_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen2, 0).await })
    };

    // The delay: give gen1's belated cleanup a generous real-time window to
    // run in the background BEFORE asserting anything about gen2.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let h1_result = tokio::time::timeout(std::time::Duration::from_secs(5), h1)
        .await
        .expect("gen1's stale loop must actually finish, not hang")
        .expect("gen1 task must not panic");
    assert!(
        matches!(h1_result, LoopResult::Disconnected),
        "gen1's stale loop must exit Disconnected"
    );

    // The generation-guard invariant: gen1's belated cleanup must not have
    // touched gen2's state.
    assert_eq!(
        state.connection_manager.get_generation(&peer_id).await,
        Some(gen2)
    );
    assert_eq!(
        state.connection_manager.connected_device_ids().await.len(),
        1
    );
    assert_eq!(state.connection_manager.cancel_token_count().await, 1);
    assert_eq!(
        state.lifecycle.get_state(&peer_id).await.ok(),
        Some(DeviceState::Connected),
        "gen1's stale disconnect must not clobber gen2's Connected lifecycle"
    );
    assert!(
        state.connection_manager.is_connected(&peer_id).await,
        "gen2's live link must still be registered"
    );

    gen2_token.cancel();
    let h2_result = tokio::time::timeout(std::time::Duration::from_secs(5), h2)
        .await
        .expect("gen2's loop must exit cleanly on cancellation")
        .expect("gen2 task must not panic");
    assert!(matches!(h2_result, LoopResult::Disconnected));
    acceptor.handle.abort();
}

/// Scenario 4 (brief): peer restart with the SAME certificate. Simulates
/// the peer daemon dying (its `ConnectionManager` — and with it every
/// socket it owns — is dropped, producing a real TCP close our side
/// observes as EOF) and restarting (a FRESH `ConnectionManager` loading
/// certificate PEM bytes from the identical on-disk directory, then
/// accepting a fresh dial). WE stay up the whole time and redial once the
/// old link is confirmed dead — the reconnection trigger a real daemon
/// would get from a fresh discovery broadcast, driven directly here for
/// determinism (documented simplification, matching the brief's own
/// allowance for scenario 5's in-process restart).
#[tokio::test]
async fn test_peer_restart_same_cert_reconnects_with_one_live_generation() {
    let (state, _temp) = create_test_state();
    let our_id = dev_id("restart-our");
    let peer_id = dev_id("restart-peer");

    state.connection_manager.set_device_identity(&our_id, "Us");
    state
        .cert_manager
        .ensure_certificate(&our_id, "Us")
        .expect("our cert");
    let our_identity = state
        .connection_manager
        .get_identity()
        .expect("our identity");

    // The peer's cert directory OUTLIVES its ConnectionManager — a
    // restarted daemon reloads the SAME PEM bytes from disk, which is the
    // whole point of "same cert" here.
    let peer_temp = tempfile::TempDir::new().expect("peer tempdir");
    let peer_certs_1 = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
    peer_certs_1.init().expect("peer cert init");
    peer_certs_1
        .ensure_certificate(&peer_id, "Peer")
        .expect("peer cert");
    peer_certs_1
        .ensure_certificate(&our_id, "Us")
        .expect("peer's copy of our cert");
    let peer_cm_1 = Arc::new(ConnectionManager::new(peer_certs_1).expect("peer cm"));
    peer_cm_1.set_device_identity(&peer_id, "Peer");

    let acceptor_1 = spawn_peer_acceptor(peer_cm_1.clone(), our_id.clone()).await;
    let identity_1 = peer_identity(&peer_id, acceptor_1.addr.port());

    let (gen1_id, gen1_remote_identity, gen1) = state
        .connection_manager
        .connect_to_device(&our_identity, acceptor_1.addr, Some(&identity_1))
        .await
        .expect("gen1 connect");
    let gen1_token = CancellationToken::new();
    assert!(
        state
            .connection_manager
            .register_cancel_token_if_current(&gen1_id, gen1_token.clone(), gen1)
            .await
    );
    state
        .lifecycle
        .ensure_and_transition(&gen1_id, &gen1_remote_identity, DeviceState::Connected)
        .await
        .expect("gen1 lifecycle transition");
    let h1 = {
        let state = state.clone();
        let device_id = gen1_id.clone();
        let token = gen1_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen1, 0).await })
    };

    assert!(
        wait_until(|| {
            let state = state.clone();
            let peer_id = peer_id.clone();
            async move { state.connection_manager.is_connected(&peer_id).await }
        })
        .await,
        "gen1 must establish before the peer 'restarts'"
    );
    let last_seen_before = state
        .registry
        .get(&peer_id)
        .await
        .expect("device registered")
        .last_seen;

    // Kill the peer daemon: stop its acceptor loop, then drop every
    // ConnectionManager reference so its held sockets (and thus the TCP
    // connection to us) actually close.
    acceptor_1.handle.abort();
    tokio::task::yield_now().await;
    drop(peer_cm_1);

    assert!(
        wait_until(|| {
            let state = state.clone();
            let peer_id = peer_id.clone();
            async move { !state.connection_manager.is_connected(&peer_id).await }
        })
        .await,
        "our side must observe the peer's death and drop the dead link"
    );
    let h1_result = tokio::time::timeout(std::time::Duration::from_secs(5), h1)
        .await
        .expect("gen1's loop must exit on the peer's real close")
        .expect("gen1 task must not panic");
    assert!(matches!(h1_result, LoopResult::Disconnected));
    assert_eq!(
        state.lifecycle.get_state(&peer_id).await.ok(),
        Some(DeviceState::Disconnected),
        "the dead link must be reflected as Disconnected before the restart"
    );

    // "Restart": a brand-new ConnectionManager loading the IDENTICAL PEM
    // bytes from the same directory.
    let peer_certs_2 = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
    peer_certs_2
        .init()
        .expect("peer cert re-init after restart");
    let peer_cm_2 = Arc::new(ConnectionManager::new(peer_certs_2).expect("peer cm 2"));
    peer_cm_2.set_device_identity(&peer_id, "Peer");
    let acceptor_2 = spawn_peer_acceptor(peer_cm_2, our_id.clone()).await;
    let identity_2 = peer_identity(&peer_id, acceptor_2.addr.port());

    sleep_past_rate_limit().await;
    let (gen2_id, gen2_remote_identity, gen2) = state
        .connection_manager
        .connect_to_device(&our_identity, acceptor_2.addr, Some(&identity_2))
        .await
        .expect("gen2 connect must land against the restarted peer");
    assert!(gen2 > gen1, "gen2 must be a strictly newer generation");
    assert_eq!(
        gen2_id, peer_id,
        "the same device id (same cert) must be recognized on reconnect"
    );

    let gen2_token = CancellationToken::new();
    assert!(
        state
            .connection_manager
            .register_cancel_token_if_current(&gen2_id, gen2_token.clone(), gen2)
            .await
    );
    state
        .lifecycle
        .ensure_and_transition(&gen2_id, &gen2_remote_identity, DeviceState::Connected)
        .await
        .expect("gen2 lifecycle transition");
    let h2 = {
        let state = state.clone();
        let device_id = gen2_id.clone();
        let token = gen2_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen2, 0).await })
    };

    assert!(
        wait_until(|| {
            let state = state.clone();
            let peer_id = peer_id.clone();
            async move {
                state
                    .lifecycle
                    .get_state(&peer_id)
                    .await
                    .is_ok_and(|s| s == DeviceState::Connected)
            }
        })
        .await,
        "reconnect after peer restart must reach Connected"
    );
    assert_eq!(
        state.connection_manager.connected_device_ids().await.len(),
        1,
        "exactly one live generation after the restart-reconnect"
    );
    assert_eq!(state.connection_manager.cancel_token_count().await, 1);
    let last_seen_after = state
        .registry
        .get(&peer_id)
        .await
        .expect("device still registered")
        .last_seen;
    assert!(
        last_seen_after > last_seen_before,
        "reconnect must refresh last_seen ({last_seen_before} -> {last_seen_after})"
    );

    gen2_token.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h2).await;
    acceptor_2.handle.abort();
}

/// Scenario 5 (brief): daemon restart (ours). Full in-process restart —
/// documented per the brief's own allowance ("In-process restart is
/// acceptable; full binary restart is the deployed systemd path, exercised
/// daily"). A second `AppState` is built from the SAME data directory
/// (same certs, same `devices.json`/`paired.json`), matching what a real
/// process restart reloads from disk.
#[tokio::test]
async fn test_daemon_restart_no_leaked_tasks_and_clean_reconnect() {
    let temp_dir = tempfile::TempDir::new().expect("shared data dir");
    let our_id = dev_id("daemon-our");
    let peer_id = dev_id("daemon-peer");

    let settings_1 = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
    let state_1 = Arc::new(AppState::new_without_input(settings_1).expect("AppState 1"));
    state_1
        .connection_manager
        .set_device_identity(&our_id, "Us");
    state_1
        .cert_manager
        .ensure_certificate(&our_id, "Us")
        .expect("our cert");
    let our_identity = state_1
        .connection_manager
        .get_identity()
        .expect("our identity");

    let peer_temp = tempfile::TempDir::new().expect("peer tempdir");
    let peer_certs = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
    peer_certs.init().expect("peer cert init");
    peer_certs
        .ensure_certificate(&peer_id, "Peer")
        .expect("peer cert");
    peer_certs
        .ensure_certificate(&our_id, "Us")
        .expect("peer's copy of our cert");
    let peer_cm = Arc::new(ConnectionManager::new(peer_certs).expect("peer cm"));
    peer_cm.set_device_identity(&peer_id, "Peer");

    let acceptor = spawn_peer_acceptor(peer_cm, our_id.clone()).await;
    let identity = peer_identity(&peer_id, acceptor.addr.port());

    let (gen1_id, gen1_remote_identity, gen1) = state_1
        .connection_manager
        .connect_to_device(&our_identity, acceptor.addr, Some(&identity))
        .await
        .expect("gen1 connect");
    let gen1_token = CancellationToken::new();
    assert!(
        state_1
            .connection_manager
            .register_cancel_token_if_current(&gen1_id, gen1_token.clone(), gen1)
            .await
    );
    state_1
        .lifecycle
        .ensure_and_transition(&gen1_id, &gen1_remote_identity, DeviceState::Connected)
        .await
        .expect("gen1 lifecycle transition");
    let h1 = {
        let state = state_1.clone();
        let device_id = gen1_id.clone();
        let token = gen1_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen1, 0).await })
    };

    assert!(
        wait_until(|| {
            let state = state_1.clone();
            let peer_id = peer_id.clone();
            async move { state.connection_manager.is_connected(&peer_id).await }
        })
        .await,
        "gen1 must establish before the (simulated) daemon restart"
    );

    // "Restart": the real shutdown signal (what SIGTERM ultimately drives)
    // rather than dropping AppState out from under the running task, which
    // would prove nothing about graceful teardown.
    state_1.shutdown.cancel();
    let h1_result = tokio::time::timeout(std::time::Duration::from_secs(5), h1)
        .await
        .expect("the old packet loop must exit promptly on shutdown, not hang")
        .expect("gen1 task must not panic");
    assert!(
        matches!(h1_result, LoopResult::Shutdown),
        "a shutdown-driven exit must report Shutdown, not Disconnected"
    );

    // No leaked tokens/tasks from the previous incarnation: the OLD
    // instance's own maps must be empty once its loop has actually exited.
    assert_eq!(
        state_1.connection_manager.cancel_token_count().await,
        0,
        "the old incarnation must not leak a cancel token"
    );
    assert_eq!(
        state_1
            .connection_manager
            .connected_device_ids()
            .await
            .len(),
        0,
        "the old incarnation must not leak a connection entry"
    );

    // Fresh incarnation, same data directory — reloads the same certs and
    // the same devices.json/paired.json a real process restart would.
    let settings_2 = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
    let state_2 = Arc::new(AppState::new_without_input(settings_2).expect("AppState 2"));
    state_2
        .connection_manager
        .set_device_identity(&our_id, "Us");
    assert!(
        state_2.cert_manager.has_certificate(&our_id),
        "the fresh incarnation must load our persisted certificate"
    );
    assert_eq!(
        state_2
            .connection_manager
            .connected_device_ids()
            .await
            .len(),
        0,
        "a fresh incarnation starts with no connections (control)"
    );
    assert_eq!(state_2.connection_manager.cancel_token_count().await, 0);
    assert!(state_2
        .connection_manager
        .pending_connections
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty());

    // The peer is still up (its ConnectionManager/acceptor never stopped) —
    // the fresh daemon incarnation redials it, mirroring what a real
    // rediscovery broadcast would trigger, driven directly for determinism.
    let (gen2_id, gen2_remote_identity, gen2) = state_2
        .connection_manager
        .connect_to_device(&our_identity, acceptor.addr, Some(&identity))
        .await
        .expect("the fresh incarnation must be able to redial the peer");
    let gen2_token = CancellationToken::new();
    assert!(
        state_2
            .connection_manager
            .register_cancel_token_if_current(&gen2_id, gen2_token.clone(), gen2)
            .await
    );
    state_2
        .lifecycle
        .ensure_and_transition(&gen2_id, &gen2_remote_identity, DeviceState::Connected)
        .await
        .expect("gen2 lifecycle transition");
    let h2 = {
        let state = state_2.clone();
        let device_id = gen2_id.clone();
        let token = gen2_token.clone();
        tokio::spawn(async move { run_packet_loop(state, token, &device_id, gen2, 0).await })
    };

    assert!(
        wait_until(|| {
            let state = state_2.clone();
            let peer_id = peer_id.clone();
            async move {
                state
                    .lifecycle
                    .get_state(&peer_id)
                    .await
                    .is_ok_and(|s| s == DeviceState::Connected)
            }
        })
        .await,
        "the fresh incarnation must reach Connected against the still-live peer"
    );
    assert_eq!(
        state_2
            .connection_manager
            .connected_device_ids()
            .await
            .len(),
        1
    );
    assert_eq!(state_2.connection_manager.cancel_token_count().await, 1);
    // Registry state converges: the peer device record persisted across the
    // restart (same devices.json) and is now current, not a stale ghost.
    let reloaded_peer = state_2
        .registry
        .get(&peer_id)
        .await
        .expect("the peer device record must have persisted across the restart");
    assert_eq!(reloaded_peer.state, DeviceState::Connected);

    gen2_token.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h2).await;
    acceptor.handle.abort();
}

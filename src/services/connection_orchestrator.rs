//! Connection orchestrator
//!
//! Single Responsibility: Manage the full connection lifecycle for a device.
//! This includes connecting, pairing, running the packet loop, and reconnecting.
//!
//! Eliminates duplicated connection logic from daemon.rs and API handlers.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::app::AppState;
use crate::device::DeviceState;
use crate::protocol::connection_loop::{self, LoopResult};
use crate::protocol::types::Identity;

/// Computes exponential backoff delay with jitter.
pub fn backoff_delay(
    attempt: u32,
    base: std::time::Duration,
    cap: std::time::Duration,
) -> std::time::Duration {
    let mut delay = base;
    for _ in 1..attempt {
        delay = std::cmp::min(delay * 2, cap);
    }
    let jitter = delay.as_millis() as u64 / 4;
    let jitter = rand::random::<u64>() % (jitter * 2 + 1);
    delay + std::time::Duration::from_millis(jitter)
}

/// Runs the post-connection setup: token registration, lifecycle transition,
/// pairing. Token registration is generation-scoped and atomic — an inbound
/// same-cert redial that replaced this outbound link during
/// `connect_to_device` must not let the stale task overwrite the
/// replacement's token or claim its lifecycle; in that case this errs and
/// the caller abandons the setup.
async fn run_connection_setup(
    state: &Arc<AppState>,
    connected_id: &String,
    remote_identity: &Identity,
    generation: u64,
) -> crate::utils::Result<CancellationToken> {
    let conn_cancel = CancellationToken::new();
    if !state
        .connection_manager
        .register_cancel_token_if_current(connected_id, conn_cancel.clone(), generation)
        .await
    {
        return Err(crate::utils::errors::Error::ConnectionError(format!(
            "Connection for {} replaced during setup (stale generation {})",
            connected_id, generation
        )));
    }

    let _ = state
        .lifecycle
        .ensure_and_transition(connected_id, remote_identity, DeviceState::Connected)
        .await;

    // If a pair request is queued (INITIATE call from the API that landed
    // while the link was down — handlers/device.rs:245-258 silently drops
    // the packet when not connected), send it now that we have a live
    // socket. Without this the queued request sits in PairingHandler.outgoing
    // until timeout, the peer never sees a pair_request packet, and the
    // D-Bus pairStateChanged(2,) signal never fires (M2 finding, vk #991).
    if !state.pairing_handler.is_paired(connected_id).await {
        if let Some(timestamp) = state
            .pairing_handler
            .pending_outgoing_timestamp(connected_id)
            .await
        {
            let pkt = crate::protocol::types::Packet::pair_request_with_timestamp(timestamp);
            match state
                .connection_manager
                .send_packet(connected_id, &pkt)
                .await
            {
                Ok(()) => info!(
                    device_id = %connected_id,
                    event = "pair_request_sent_on_connect",
                    "Sent queued pair_request on link re-establish"
                ),
                Err(e) => warn!(
                    device_id = %connected_id,
                    error = %e,
                    event = "pair_request_send_failed_on_connect",
                    "Failed to send queued pair_request on link re-establish"
                ),
            }
        }
    }

    if state.pairing_handler.is_paired(connected_id).await {
        let state_clone = state.clone();
        let device_id_clone = connected_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            state_clone.send_plugin_init_packets(&device_id_clone).await;
        });
    }

    Ok(conn_cancel)
}

/// Runs the full connection sequence: setup + packet loop.
async fn run_connection_sequence(
    state: Arc<AppState>,
    connected_id: String,
    remote_identity: Identity,
    generation: u64,
) -> LoopResult {
    let conn_cancel = match run_connection_setup(
        &state,
        &connected_id,
        &remote_identity,
        generation,
    )
    .await
    {
        Ok(cancel) => cancel,
        Err(e) => {
            warn!(error = %e, device_id = %connected_id, event = "connection_setup_failed", "Failed to set up connection");
            return LoopResult::Disconnected;
        }
    };

    connection_loop::run_packet_loop(
        state.clone(),
        conn_cancel,
        &connected_id,
        generation,
        state.settings.idle_timeout_secs,
    )
    .await
}

/// Removes a device id from `ConnectionManager::pending_connections` when
/// dropped, so every exit path of a dial — success, error, panic — releases
/// the in-flight marker. Shared by the discovery dial and the reconnect loop
/// (2026-09-02 audit, C1b: the reconnect loop never registered, so a
/// discovery event could dial the same device concurrently).
struct PendingGuard {
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        // Poison-tolerant: the add path recovers via into_inner(), so
        // the remove path must too or entries leak forever.
        let mut lock = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        lock.remove(&self.id);
    }
}

/// Reconnect loop with exponential backoff.
async fn reconnect_loop(
    state: Arc<AppState>,
    identity: Identity,
    addr: SocketAddr,
    device_id: String,
) {
    let max_retries: u32 = 5;

    for attempt in 1..=max_retries {
        if state.shutdown.is_cancelled() {
            return;
        }

        let delay = backoff_delay(
            attempt,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(30),
        );

        info!(
            device_id = %device_id,
            attempt = attempt,
            max_retries = max_retries,
            delay_secs = delay.as_secs(),
            event = "reconnect_attempt",
            "Attempting reconnection with exponential backoff"
        );

        // Shutdown-interruptible backoff: a bare sleep would hold the
        // reconnect task alive for up to the full delay past shutdown.
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = state.shutdown.cancelled() => return,
        }

        if state.shutdown.is_cancelled() || state.connection_manager.is_connected(&device_id).await
        {
            return;
        }

        let our_identity = match state.connection_manager.get_identity() {
            Some(id) => id,
            None => return,
        };

        // Same in-flight marker as the discovery dial: if another dial for
        // this device is already under way, this attempt yields to it.
        if !state
            .connection_manager
            .add_pending_connection(&device_id)
            .await
        {
            debug!(
                device_id = %device_id,
                attempt = attempt,
                event = "reconnect_yielded_to_pending_dial",
                "Another dial to this device is in flight; skipping this attempt"
            );
            continue;
        }
        let connect_result = {
            let _guard = PendingGuard {
                pending: state.connection_manager.pending_connections.clone(),
                id: device_id.clone(),
            };
            state
                .connection_manager
                .connect_to_device(&our_identity, addr, Some(&identity))
                .await
        };

        match connect_result {
            Ok((connected_id, remote_identity, new_generation)) => {
                info!(
                    device_id = %connected_id,
                    attempt = attempt,
                    event = "reconnect_success",
                    "Successfully reconnected to device"
                );

                let result = run_connection_sequence(
                    state.clone(),
                    connected_id.clone(),
                    remote_identity,
                    new_generation,
                )
                .await;

                if matches!(result, LoopResult::Disconnected)
                    && !state.shutdown.is_cancelled()
                    && attempt < max_retries
                {
                    continue;
                }
                return;
            }
            Err(e) => {
                debug!(
                    device_id = %device_id,
                    attempt = attempt,
                    error = %e,
                    event = "reconnect_failed",
                    "Reconnection attempt failed"
                );
            }
        }
    }

    warn!(
        device_id = %device_id,
        event = "reconnect_exhausted",
        "Exhausted reconnection attempts"
    );
}

/// Spawns a connection task for a discovered device.
/// Includes a 2-second delay to avoid connecting to devices that are already connecting.
pub fn spawn_discovered_connection(state: Arc<AppState>, identity: Identity, addr: SocketAddr) {
    let device_id = identity.device_id.clone();

    // Self-guard: never dial our own advertised identity (reflected broadcast,
    // second interface, or spoof).
    {
        let our_id = state
            .connection_manager
            .device_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if device_id == our_id {
            return;
        }
    }

    let tcp_port = identity.tcp_port.unwrap_or(1716);
    // SocketAddr::new, not "ip:port" interpolation: a bare IPv6 literal is
    // not valid SocketAddr text, and the old parse-fallback chain ended in
    // an expect() — a live panic on IPv6 discovery.
    let connect_addr = SocketAddr::new(addr.ip(), tcp_port);

    tokio::spawn(async move {
        if state.connection_manager.is_connected(&device_id).await {
            return;
        }

        // Android isProtocolDowngrade (LanLinkProvider.java:280-283,
        // 351-354): a trusted device re-announcing a LOWER protocolVersion
        // than we last knew is refused — never dial it.
        if state.pairing_handler.is_paired(&device_id).await {
            if let Ok(known) = state.registry.get(&device_id).await {
                if known.protocol_version > identity.protocol_version {
                    info!(
                        device_id = %device_id,
                        stored_version = known.protocol_version,
                        announced_version = identity.protocol_version,
                        event = "protocol_downgrade_refused",
                        "Refusing to connect to a paired device using an older protocol version"
                    );
                    return;
                }
            }
        }

        // Identity check BEFORE adding to the pending set — an early return
        // after add_pending_connection without a guard is a permanent leak.
        let our_identity = match state.connection_manager.get_identity() {
            Some(id) => id,
            None => return,
        };

        if !state
            .connection_manager
            .add_pending_connection(&device_id)
            .await
        {
            return;
        }

        let connect_result = {
            let _guard = PendingGuard {
                pending: state.connection_manager.pending_connections.clone(),
                id: device_id.clone(),
            };

            info!(
                device_id = %device_id,
                address = %connect_addr,
                event = "initiating_outgoing_connection",
                "Initiating outgoing connection to discovered device"
            );

            state
                .connection_manager
                .connect_to_device(&our_identity, connect_addr, Some(&identity))
                .await
        };

        match connect_result {
            Ok((connected_id, remote_identity, generation)) => {
                info!(
                    device_id = %connected_id,
                    device_name = %remote_identity.device_name,
                    event = "outgoing_connection_success",
                    "Successfully connected to device"
                );

                let result = run_connection_sequence(
                    state.clone(),
                    connected_id.clone(),
                    remote_identity,
                    generation,
                )
                .await;

                if matches!(result, LoopResult::Disconnected)
                    && state.pairing_handler.is_paired(&connected_id).await
                    && !state.shutdown.is_cancelled()
                {
                    reconnect_loop(state, identity, connect_addr, connected_id).await;
                }
            }
            Err(e) => {
                debug!(
                    device_id = %device_id,
                    error = %e,
                    event = "outgoing_connection_failed",
                    "Failed outgoing connection attempt"
                );
            }
        }
    });
}

/// Connects to a device, runs setup, and spawns the packet loop in the background.
/// Returns the connected device ID and generation immediately.
/// Used by the API's connect_device endpoint.
pub async fn connect_and_spawn_loop(
    state: Arc<AppState>,
    our_identity: Identity,
    addr: SocketAddr,
) -> crate::utils::Result<(String, u64)> {
    let (connected_id, remote_identity, generation) = state
        .connection_manager
        .connect_to_device(&our_identity, addr, None)
        .await
        .map_err(|e| crate::utils::errors::Error::ConnectionError(e.to_string()))?;

    let idle_timeout_secs = state.settings.idle_timeout_secs;

    let conn_cancel =
        run_connection_setup(&state, &connected_id, &remote_identity, generation).await?;

    let packet_loop_id = connected_id.clone();
    tokio::spawn(async move {
        let _ = connection_loop::run_packet_loop(
            state,
            conn_cancel,
            &packet_loop_id,
            generation,
            idle_timeout_secs,
        )
        .await;
    });

    Ok((connected_id, generation))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! Outbound setup is generation-guarded symmetrically with the inbound
    //! listener: a stale generation must not register its token or claim
    //! the replacement's lifecycle.

    use super::*;
    use crate::config::settings::AppSettings;
    use crate::protocol::{CertificateManager, ConnectionManager};
    use std::sync::Arc;

    const OUR_ID: &str = "orch-our-device-aaaaaaaaaaaaaaaaaaa";
    const PEER_ID: &str = "orch-peer-device-aaaaaaaaaaaaaaaaaa";

    /// Test helper: build a peer cert DER in a throwaway cert dir so the
    /// cert-anchor gate (vk #1056) can be staged into a handler that
    /// would otherwise refuse a cert-less force-accept. The cert's id is
    /// pad-extended to the cert manager's 32-char minimum.
    fn make_external_cert_der(peer_id: &str) -> Vec<u8> {
        const MIN_DEVICE_ID_LEN: usize = 32;
        let mut valid_id = String::from(peer_id);
        while valid_id.len() < MIN_DEVICE_ID_LEN {
            valid_id.push('a');
        }
        let cm_for_cert = Arc::new(CertificateManager::new(
            tempfile::TempDir::new()
                .expect("Value expected to be present")
                .path()
                .to_path_buf(),
        ));
        let (cert_pem, _) = cm_for_cert
            .generate_certificate(&valid_id, "Peer")
            .expect("Value expected to be present");
        openssl::x509::X509::from_pem(&cert_pem)
            .expect("Value expected to be present")
            .to_der()
            .expect("Value expected to be present")
    }

    fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
        let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
        let state =
            Arc::new(AppState::new_without_input(settings).expect("Value expected to be present"));
        state.connection_manager.set_device_identity(OUR_ID, "Us");
        (state, temp_dir)
    }

    /// In-process link: state's connection manager (client role) connected
    /// to a peer manager holding the server side. A second call for the
    /// same PEER_ID replaces the first link with a new generation.
    async fn connect_peer(
        state: &Arc<AppState>,
    ) -> (tokio::task::JoinHandle<()>, tempfile::TempDir, u64) {
        let peer_temp = tempfile::TempDir::new().expect("Value expected to be present");
        let peer_certs = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
        peer_certs.init().expect("Value expected to be present");
        let peer_cm = Arc::new(
            ConnectionManager::new(peer_certs.clone()).expect("Value expected to be present"),
        );
        peer_cm.set_device_identity(PEER_ID, "Peer");
        peer_certs
            .ensure_certificate(PEER_ID, "Peer")
            .expect("Value expected to be present");
        state
            .cert_manager
            .ensure_certificate(OUR_ID, "Us")
            .expect("Value expected to be present");
        peer_certs
            .ensure_certificate(OUR_ID, "Us")
            .expect("Value expected to be present");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            peer_cm
                .accept_test(OUR_ID.to_string(), stream)
                .await
                .expect("Value expected to be present");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        let generation = state
            .connection_manager
            .connect(&PEER_ID.to_string(), addr)
            .await
            .expect("Value expected to be present");
        (server_handle, peer_temp, generation)
    }

    fn peer_identity() -> Identity {
        Identity::new(
            PEER_ID.to_string(),
            "Peer".to_string(),
            crate::device::types::DeviceType::Phone,
            vec![],
            vec![],
        )
    }

    #[tokio::test]
    async fn test_run_connection_setup_rejects_stale_generation() {
        let (state, _t) = test_state();
        let (_h1, _t1, gen1) = connect_peer(&state).await;
        let (_h2, _t2, gen2) = connect_peer(&state).await;
        assert!(gen2 > gen1, "second link must replace the first");

        // The stale outbound task: setup must refuse — no token
        // registration, no lifecycle claim (before the guard it registered
        // unconditionally and overwrote the replacement's token).
        let err = run_connection_setup(&state, &PEER_ID.to_string(), &peer_identity(), gen1)
            .await
            .expect_err("a stale generation must not complete setup");
        assert!(
            err.to_string().contains("replaced during setup"),
            "unexpected error: {err}"
        );
        assert!(
            state
                .lifecycle
                .get_state(&PEER_ID.to_string())
                .await
                .is_err(),
            "stale setup must not claim the lifecycle"
        );

        // The live generation sets up normally: token registered and
        // cancellable, lifecycle Connected.
        let token = run_connection_setup(&state, &PEER_ID.to_string(), &peer_identity(), gen2)
            .await
            .expect("current generation must complete setup");
        state
            .connection_manager
            .cancel_loop(&PEER_ID.to_string())
            .await;
        assert!(
            token.is_cancelled(),
            "the live generation's token must be registered"
        );
        assert!(
            state
                .lifecycle
                .get_state(&PEER_ID.to_string())
                .await
                .is_ok_and(|s| s == DeviceState::Connected),
            "live setup must mark the device Connected"
        );
    }

    /// Shutdown during the backoff sleep must end the reconnect loop
    /// promptly instead of waiting out the full delay.
    #[tokio::test]
    async fn test_reconnect_loop_backoff_aborts_on_shutdown() {
        let (state, _t) = test_state();

        let addr: SocketAddr = "127.0.0.1:1".parse().expect("Value expected to be present");
        let state_clone = state.clone();
        let handle = tokio::spawn(async move {
            reconnect_loop(state_clone, peer_identity(), addr, PEER_ID.to_string()).await;
        });

        // Let the loop enter its first backoff sleep, then shut down.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        state.shutdown.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("reconnect loop must exit promptly on shutdown during backoff")
            .expect("Value expected to be present");
    }

    /// Lane-C repro (audit-2026-09-02-remaining-brief.md, section C, lead
    /// 1, second anchor): `reconnect_loop` (`:141`) dials via
    /// `connect_to_device` with no `add_pending_connection` guard at all,
    /// unlike `spawn_discovered_connection` (`:284-328`), which registers
    /// in `pending_connections` before it dials and is deduped by it on a
    /// second concurrent call for the same device. This marks the device
    /// pending FIRST — exactly what a concurrent discovery-triggered dial
    /// would have done — and shows `reconnect_loop` still completes a
    /// real TCP dial to it, unaware the device is already "in flight" per
    /// that guard. Deterministic, not a timing race: `reconnect_loop`
    /// simply never reads `pending_connections`.
    #[tokio::test]
    async fn test_reconnect_loop_ignores_pending_connections_guard() {
        let (state, _t) = test_state();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        // Simulate a discovery-triggered dial already in flight for this
        // device (what `spawn_discovered_connection` registers before it
        // calls `connect_to_device`).
        assert!(
            state
                .connection_manager
                .add_pending_connection(&PEER_ID.to_string())
                .await,
            "first pending registration must succeed"
        );
        // The guard works as intended for ITS OWN callers: a second
        // discovery-triggered dial for the same device is deduped.
        assert!(
            !state
                .connection_manager
                .add_pending_connection(&PEER_ID.to_string())
                .await,
            "a second discovery-triggered dial for the same device must be deduped by the pending guard"
        );

        let state_clone = state.clone();
        let peer_id = PEER_ID.to_string();
        tokio::spawn(async move {
            reconnect_loop(state_clone, peer_identity(), addr, peer_id).await;
        });

        // reconnect_loop's first attempt fires after the attempt=1 backoff
        // (base 1s + up to 25% jitter). If it respected
        // `pending_connections` the way `spawn_discovered_connection`
        // does, it would see the device already pending and skip dialing
        // — the listener would never observe a connection.
        let accept =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept()).await;
        assert!(
            accept.is_err(),
            "reconnect_loop dialed {addr} despite {PEER_ID} already being marked pending by \
             another caller — it never checks or registers in `pending_connections`, unlike \
             spawn_discovered_connection's add_pending_connection guard which would have deduped \
             a second discovery-triggered dial for the same id"
        );
    }

    /// P2 self-guard: a discovered identity carrying OUR OWN device id
    /// (reflected broadcast, second interface, spoof) must be dropped
    /// before anything is spawned, dialed, or registered.
    #[tokio::test]
    async fn test_spawn_discovered_connection_ignores_self_identity() {
        let (state, _t) = test_state();

        // A listener standing by: if the guard regresses, the connection
        // task dials it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        let mut self_identity = peer_identity();
        self_identity.device_id = OUR_ID.to_string();
        self_identity.tcp_port = Some(addr.port());

        spawn_discovered_connection(state.clone(), self_identity, addr);

        let dial =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await;
        assert!(dial.is_err(), "our own identity must never be dialed");
        {
            let pending = state
                .connection_manager
                .pending_connections
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(
                !pending.contains(OUR_ID),
                "our own id must never enter the pending set"
            );
        }
        assert!(
            !state
                .connection_manager
                .is_connected(&OUR_ID.to_string())
                .await,
            "our own id must never be registered as a connection"
        );
    }

    /// Android isProtocolDowngrade (LanLinkProvider.java:280-283): a PAIRED
    /// device re-announcing a lower protocolVersion than we last knew is
    /// refused — never dialed.
    #[tokio::test]
    async fn test_spawn_discovered_connection_refuses_protocol_downgrade() {
        let (state, _t) = test_state();

        // The device is paired and last known at protocol version 8.
        state
            .registry
            .add(crate::device::Device::new(
                PEER_ID.to_string(),
                "Peer".to_string(),
                crate::device::types::DeviceType::Phone,
                8,
            ))
            .await
            .expect("Value expected to be present");
        // The cert-anchor gate (vk #1056) refuses a force-accept without
        // a pending or pinned cert. Stage a synthetic peer cert so the
        // test exercises the protocol-downgrade path, not the gate.
        let cert_der = make_external_cert_der(PEER_ID);
        state
            .pairing_handler
            .set_pending_peer_cert(&PEER_ID.to_string(), cert_der)
            .await;
        state
            .pairing_handler
            .force_accept_pairing(&PEER_ID.to_string())
            .await
            .expect("Value expected to be present");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        let mut identity = peer_identity();
        identity.protocol_version = 7;
        identity.tcp_port = Some(addr.port());
        spawn_discovered_connection(state.clone(), identity, addr);

        let dial =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await;
        assert!(
            dial.is_err(),
            "a paired device announcing a lower protocolVersion must never be dialed"
        );
    }

    /// Control for the self-guard: a real peer identity IS dialed, so the
    /// early return above is specific to self, not a dead discovery path.
    #[tokio::test]
    async fn test_spawn_discovered_connection_dials_peer_identity() {
        let (state, _t) = test_state();

        // Peer side: an accept_test server, same shape as connect_peer's.
        let peer_temp = tempfile::TempDir::new().expect("Value expected to be present");
        let peer_certs = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
        peer_certs.init().expect("Value expected to be present");
        let peer_cm = Arc::new(
            ConnectionManager::new(peer_certs.clone()).expect("Value expected to be present"),
        );
        peer_cm.set_device_identity(PEER_ID, "Peer");
        peer_certs
            .ensure_certificate(PEER_ID, "Peer")
            .expect("Value expected to be present");
        state
            .cert_manager
            .ensure_certificate(OUR_ID, "Us")
            .expect("Value expected to be present");
        peer_certs
            .ensure_certificate(OUR_ID, "Us")
            .expect("Value expected to be present");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        // Peer side: spawn_discovered_connection dials via connect_to_device
        // (plaintext identity, then TLS-server role), so the peer answers
        // with the matching test helper — accept_test_as_client, not the
        // raw-TLS accept_test that pairs with the connect() helper.
        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            peer_cm
                .accept_test_as_client(OUR_ID.to_string(), stream, addr.port())
                .await
                .expect("Value expected to be present");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        let mut identity = peer_identity();
        identity.tcp_port = Some(addr.port());
        spawn_discovered_connection(state.clone(), identity, addr);

        let mut connected = false;
        for _ in 0..80 {
            if state
                .connection_manager
                .is_connected(&PEER_ID.to_string())
                .await
            {
                connected = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(connected, "a discovered peer identity must be dialed");

        // Stop the spawned packet loop (it owns the link until shutdown).
        state.shutdown.cancel();
        server_handle.abort();
    }
}

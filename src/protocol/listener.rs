//! TCP listener for incoming device connections
//!
//! Single Responsibility: Accept incoming TCP connections, perform identity exchange,
//! and run per-connection packet receive loops.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::app::AppState;
use crate::device::types::{DeviceId, DeviceState};
use crate::protocol::connection_loop::{run_packet_loop, LoopResult};
use crate::protocol::types::Identity;
use crate::utils::errors::Error;

/// Cap on concurrent inbound connection handlers. Each accepted socket
/// spawns a task that holds TLS and connection state through identity
/// exchange, so an unbounded accept loop is an FD/memory exhaustion
/// vector; 64 is far above the device count on any real LAN.
const MAX_INBOUND_CONNECTIONS: usize = 64;

/// Take an inbound-handler slot, or None when the cap is reached (the
/// caller drops the connection). The returned permit releases the slot
/// when the handler task finishes.
fn try_acquire_inbound_slot(slots: &Arc<Semaphore>) -> Option<tokio::sync::OwnedSemaphorePermit> {
    slots.clone().try_acquire_owned().ok()
}

pub struct TcpListenerService {
    state: Arc<AppState>,
    identity: Identity,
}

impl TcpListenerService {
    pub fn new(state: Arc<AppState>, identity: Identity) -> Self {
        Self { state, identity }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), std::io::Error> {
        let preferred_port = self.state.settings.tcp_port;
        let (listener, actual_port) = Self::bind_port(preferred_port).await?;
        info!(
            port = actual_port,
            preferred = preferred_port,
            event = "tcp_listener_ready",
            "TCP listener bound for incoming device connections"
        );
        self.run_from_bound(listener, shutdown).await
    }

    pub async fn bind_port(preferred: u16) -> Result<(TcpListener, u16), std::io::Error> {
        TcpListener::bind(format!("0.0.0.0:{preferred}"))
            .await
            .map(|listener| (listener, preferred))
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "failed to bind protocol port {preferred}: {error}; another rust-connect instance is already running"
                    ),
                )
            })
    }

    pub async fn run_from_bound(
        &self,
        listener: TcpListener,
        shutdown: CancellationToken,
    ) -> Result<(), std::io::Error> {
        info!(
            event = "tcp_listener_ready",
            "TCP listener running from pre-bound socket"
        );

        let inbound_slots = Arc::new(Semaphore::new(MAX_INBOUND_CONNECTIONS));
        loop {
            // A4 (2026-09-02 audit): a bare accept loop never ended, and
            // every daemon stop paid a 5 s join timeout for it.
            let accepted = tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(event = "tcp_listener_stopped", "TCP listener stopped on shutdown");
                    return Ok(());
                }
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, addr)) => {
                    info!(peer = %addr, event = "tcp_connection_received", "Incoming TCP connection");
                    let Some(permit) = try_acquire_inbound_slot(&inbound_slots) else {
                        warn!(peer = %addr, event = "inbound_connection_dropped_at_cap", "Inbound handler cap reached, dropping connection");
                        continue;
                    };
                    let state = self.state.clone();
                    let identity = self.identity.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        Self::handle_connection(state, stream, addr, identity).await;
                    });
                }
                Err(e) => {
                    warn!(error = %e, event = "tcp_accept_failed", "Failed to accept TCP connection");
                }
            }
        }
    }

    async fn handle_connection(
        state: Arc<AppState>,
        stream: tokio::net::TcpStream,
        addr: std::net::SocketAddr,
        our_identity: Identity,
    ) {
        let cm = &state.connection_manager;
        let pairing = &state.pairing_handler;

        trace!(peer = %addr, event = "handle_connection_start", "Starting handle_connection");

        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            cm.accept_incoming(stream),
        )
        .await
        {
            Ok(Ok((device_id, remote_identity, generation))) => {
                info!(
                    device_id = %device_id,
                    device_name = %remote_identity.device_name,
                    peer = %addr,
                    event = "incoming_connection_established",
                    "Incoming connection established"
                );

                trace!(device_id = %device_id, event = "starting_identity_exchange", "Starting encrypted identity exchange");

                let exchange_result = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    Self::identity_exchange(
                        cm,
                        &our_identity,
                        &remote_identity,
                        &device_id,
                        generation,
                    ),
                )
                .await;

                match exchange_result {
                    Err(_) => {
                        warn!(device_id = %device_id, event = "identity_exchange_timeout", "Identity exchange timed out");
                        // Teardown only when THIS generation owns the link:
                        // a replacement's disconnect returns false, and the
                        // replacement owns lifecycle/plugin state (same
                        // ownership gate as run_packet_loop's exit arms).
                        match cm.disconnect(&device_id, generation).await {
                            Ok(true) => {
                                state
                                    .lifecycle
                                    .try_transition(&device_id, DeviceState::Disconnected)
                                    .await;
                                state.plugin_registry.notify_disconnected(&device_id).await;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                warn!(device_id = %device_id, error = %e, event = "disconnect_failed", "Failed to disconnect after identity exchange timeout");
                            }
                        }
                        return;
                    }
                    Ok(Err(e)) => {
                        // A failed encrypted-identity exchange is a failed
                        // connection — never a "connected" lifecycle on a
                        // dead link (the 2026-07-29 audit's
                        // failure-converted-to-success defect).
                        warn!(device_id = %device_id, error = %e, event = "identity_exchange_failed", "Encrypted identity exchange failed; aborting connection");
                        match cm.disconnect(&device_id, generation).await {
                            Ok(true) => {
                                state
                                    .lifecycle
                                    .try_transition(&device_id, DeviceState::Disconnected)
                                    .await;
                                state.plugin_registry.notify_disconnected(&device_id).await;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                warn!(device_id = %device_id, error = %e, event = "disconnect_failed", "Failed to disconnect after identity exchange failure");
                            }
                        }
                        return;
                    }
                    Ok(Ok(())) => {}
                }

                trace!(device_id = %device_id, event = "identity_exchange_success", "Identity exchange completed successfully");

                // Generation-validate and token-register ATOMICALLY: a
                // same-cert redial landing between a bare check and the
                // insert would let this stale handler overwrite the
                // replacement's token (its loop uncancellable) and claim
                // its lifecycle. A stale task exits here without touching
                // shared state — the replacement owns it.
                let conn_cancel = CancellationToken::new();
                if !cm
                    .register_cancel_token_if_current(&device_id, conn_cancel.clone(), generation)
                    .await
                {
                    info!(
                        device_id = %device_id,
                        event = "stale_generation_after_identity_exchange",
                        "Connection replaced during identity exchange; aborting stale setup"
                    );
                    return;
                }

                if let Err(e) = state
                    .lifecycle
                    .ensure_and_transition(&device_id, &remote_identity, DeviceState::Connected)
                    .await
                {
                    warn!(device_id = %device_id, error = %e, event = "connected_transition_failed", "Failed to transition device to Connected");
                }

                if pairing.is_paired(&device_id).await {
                    trace!(device_id = %device_id, event = "notifying_plugins", "Device already paired, sending plugin init packets");
                    let state_clone = state.clone();
                    let device_id_clone = device_id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        state_clone.send_plugin_init_packets(&device_id_clone).await;
                    });
                } else {
                    trace!(device_id = %device_id, event = "skipping_init_packets_unpaired", "Device not paired, deferring plugin init packets until pairing completes");
                }

                trace!(device_id = %device_id, event = "starting_packet_loop", "Entering packet receive loop");
                let loop_result = run_packet_loop(
                    state.clone(),
                    conn_cancel,
                    &device_id,
                    generation,
                    state.settings.idle_timeout_secs,
                )
                .await;

                info!(
                    device_id = %device_id,
                    loop_result = match &loop_result { LoopResult::Shutdown => "Shutdown", LoopResult::Disconnected => "Disconnected" },
                    event = "packet_loop_ended",
                    "Packet loop exited"
                );

                if let Err(e) = cm.disconnect(&device_id, generation).await {
                    debug!(device_id = %device_id, error = %e, event = "disconnect_after_loop", "Disconnect after packet loop");
                }

                if matches!(loop_result, LoopResult::Disconnected)
                    && state.pairing_handler.is_paired(&device_id).await
                    && !state.shutdown.is_cancelled()
                {
                    info!(
                        device_id = %device_id,
                        event = "incoming_reconnect_triggered",
                        "Incoming connection disconnected for paired device, discovery will handle reconnect"
                    );
                }
            }
            Ok(Err(e)) => {
                warn!(error = %e, peer = %addr, event = "incoming_connection_failed", "Failed to accept incoming connection");
            }
            Err(_) => {
                warn!(peer = %addr, event = "incoming_connection_timeout", "Incoming connection timed out");
            }
        }
    }

    /// Encrypted identity exchange (inbound side). Any failure — read error,
    /// wrong packet type, unparseable identity, deviceId or protocolVersion
    /// mismatch vs the plaintext identity, send failure — is an Err, and the
    /// caller aborts the connection. `plaintext_identity` is the identity the
    /// peer sent pre-TLS; Android rejects a mid-handshake deviceId or
    /// protocolVersion change (LanLinkProvider.java:316-321). The read is
    /// generation-scoped: a redial that replaced this connection mid-exchange
    /// turns the read into a fast stale-generation error instead of stealing
    /// packets from the replacement's TLS stream.
    async fn identity_exchange(
        cm: &crate::protocol::ConnectionManager,
        our_identity: &Identity,
        plaintext_identity: &Identity,
        device_id: &DeviceId,
        generation: u64,
    ) -> crate::utils::errors::Result<()> {
        trace!(device_id = %device_id, event = "identity_exchange_waiting_for_encrypted", "Waiting for encrypted identity packet from device");

        let encrypted_identity = cm
            .recv_packet_current(device_id, generation)
            .await
            .map_err(|e| {
                Error::ConnectionError(format!("Failed to read encrypted identity: {}", e))
            })?;

        trace!(device_id = %device_id, packet_type = %encrypted_identity.packet_type, event = "encrypted_identity_received_raw", "Received raw packet over TLS");

        if !encrypted_identity.is_identity() {
            return Err(Error::InvalidPacket(format!(
                "Expected encrypted identity packet, got {}",
                encrypted_identity.packet_type
            )));
        }

        let enc_identity = Identity::from_packet(encrypted_identity)
            .map_err(|e| Error::InvalidPacket(format!("Malformed encrypted identity: {}", e)))?;

        if enc_identity.device_id != *device_id {
            return Err(Error::InvalidPacket(format!(
                "Encrypted identity deviceId '{}' does not match plaintext identity '{}'",
                enc_identity.device_id, device_id
            )));
        }

        if enc_identity.protocol_version != plaintext_identity.protocol_version {
            return Err(Error::InvalidPacket(format!(
                "Encrypted identity protocolVersion {} differs from plaintext {} (mid-handshake change rejected)",
                enc_identity.protocol_version, plaintext_identity.protocol_version
            )));
        }

        debug!(device_id = %device_id, event = "encrypted_identity_received", "Received encrypted identity from device");

        trace!(device_id = %device_id, event = "sending_our_encrypted_identity", "Sending our encrypted identity to device");
        let our_identity_packet = our_identity.to_tcp_packet()?;
        cm.send_packet(device_id, &our_identity_packet)
            .await
            .map_err(|e| {
                Error::ConnectionError(format!("Failed to send encrypted identity: {}", e))
            })?;
        trace!(device_id = %device_id, event = "our_encrypted_identity_sent", "Our encrypted identity sent successfully");

        info!(device_id = %device_id, event = "identity_exchange_complete", "Identity exchange complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    // A std Mutex is deliberate here: it serializes the port-binding tests, and the
    // guard must stay held across the bind awaits or the tests race each other.
    #![allow(clippy::await_holding_lock)]
    use super::*;

    static PORT_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn test_inbound_slot_cap_rejects_overflow_and_recovers() {
        let slots = Arc::new(Semaphore::new(MAX_INBOUND_CONNECTIONS));
        let permits: Vec<_> = (0..MAX_INBOUND_CONNECTIONS)
            .map(|_| {
                try_acquire_inbound_slot(&slots).expect("a slot below the cap must be available")
            })
            .collect();
        assert!(
            try_acquire_inbound_slot(&slots).is_none(),
            "the cap must reject the next inbound connection"
        );
        drop(permits);
        assert!(
            try_acquire_inbound_slot(&slots).is_some(),
            "a freed slot must be reusable"
        );
    }

    #[tokio::test]
    async fn test_bind_port_zero() {
        let _guard = PORT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let (listener, port) = TcpListenerService::bind_port(0)
            .await
            .expect("Value expected to be present");
        assert_eq!(port, 0);
        drop(listener);
    }

    #[tokio::test]
    async fn test_bind_port_fails_when_requested_port_is_taken() {
        let _guard = PORT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let occupied = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let port = occupied
            .local_addr()
            .expect("Value expected to be present")
            .port();

        let error = TcpListenerService::bind_port(port)
            .await
            .expect_err("a configured protocol port conflict must be fatal");

        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(error.to_string().contains(&port.to_string()));
        assert!(error
            .to_string()
            .contains("another rust-connect instance is already running"));
        drop(occupied);
    }

    #[tokio::test]
    async fn test_bind_port_prefers_requested_when_free() {
        let _guard = PORT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let test_port = 17500u16;
        let (listener, port) = TcpListenerService::bind_port(test_port)
            .await
            .expect("Value expected to be present");
        assert_eq!(port, test_port);
        drop(listener);
    }
}

#[cfg(test)]
mod identity_exchange_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! P6: a failed encrypted-identity exchange must abort the connection.
    //! These tests drive the real exchange over an in-process TLS link.

    use super::*;
    use crate::protocol::connection::tls;
    use crate::protocol::crypto::CertificateManager;
    use crate::protocol::packet::PacketSerializer;
    use crate::protocol::{ConnectionManager, Packet};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const OUR_ID: &str = "listener-our-device-aaaaaaaaaaaaaaaa";
    const PEER_ID: &str = "listener-peer-device-aaaaaaaaaaaaaa";

    fn make_identity(id: &str, protocol_version: u32) -> Identity {
        let mut identity = Identity::new(
            id.to_string(),
            "Peer".to_string(),
            crate::device::DeviceType::Phone,
            vec![],
            vec![],
        );
        identity.protocol_version = protocol_version;
        identity
    }

    /// A live TLS link: `mgr` holds the server side (peer id PEER_ID), and
    /// the returned stream is the peer's client end. Also returns the
    /// connection generation accept produced, for the generation-scoped
    /// exchange under test.
    async fn setup_link() -> (
        Arc<ConnectionManager>,
        tls::TlsStream,
        tempfile::TempDir,
        u64,
    ) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let cm = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
        cm.init().expect("init");
        let mgr = Arc::new(ConnectionManager::new(cm).expect("cm"));
        mgr.set_device_identity(OUR_ID, "Us");

        let peer_temp = tempfile::TempDir::new().expect("tempdir");
        let peer_cm = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
        peer_cm.init().expect("init");
        peer_cm
            .ensure_own_certificate(PEER_ID, "Peer")
            .expect("peer cert");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client = tokio::spawn(async move {
            let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
            tls::tls_connect(peer_cm, OUR_ID, tcp)
                .await
                .expect("peer tls_connect")
                .0
        });

        let (server_tcp, _) = listener.accept().await.expect("accept");
        let generation = mgr
            .accept_test(PEER_ID.to_string(), server_tcp)
            .await
            .expect("accept_test");

        let peer_stream = client.await.expect("join");
        (mgr, peer_stream, temp, generation)
    }

    async fn peer_send(peer: &mut tls::TlsStream, packet: &Packet) {
        let bytes = PacketSerializer::serialize(packet).expect("serialize");
        peer.write_all(&bytes).await.expect("write");
        peer.flush().await.expect("flush");
    }

    #[tokio::test]
    async fn test_exchange_rejects_non_identity_packet() {
        let (mgr, mut peer, _t, gen) = setup_link().await;
        peer_send(&mut peer, &Packet::ping()).await;

        let our_identity = make_identity(OUR_ID, 8);
        let plaintext = make_identity(PEER_ID, 8);
        let result = TcpListenerService::identity_exchange(
            &mgr,
            &our_identity,
            &plaintext,
            &PEER_ID.to_string(),
            gen,
        )
        .await;
        let err = result.expect_err("non-identity packet must fail the exchange");
        assert!(
            err.to_string().contains("Expected encrypted identity"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_exchange_rejects_device_id_mismatch() {
        let (mgr, mut peer, _t, gen) = setup_link().await;
        let wrong = make_identity("listener-impostor-device-aaaaaaaaaa", 8);
        peer_send(
            &mut peer,
            &wrong.to_tcp_packet().expect("Value expected to be present"),
        )
        .await;

        let our_identity = make_identity(OUR_ID, 8);
        let plaintext = make_identity(PEER_ID, 8);
        let result = TcpListenerService::identity_exchange(
            &mgr,
            &our_identity,
            &plaintext,
            &PEER_ID.to_string(),
            gen,
        )
        .await;
        let err = result.expect_err("deviceId mismatch must fail the exchange");
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[tokio::test]
    async fn test_exchange_rejects_protocol_version_change() {
        let (mgr, mut peer, _t, gen) = setup_link().await;
        // Same device id as the plaintext identity, but a mid-handshake
        // protocolVersion change (LanLinkProvider.java:316-321 rejects this).
        let tampered = make_identity(PEER_ID, 7);
        peer_send(
            &mut peer,
            &tampered
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .await;

        let our_identity = make_identity(OUR_ID, 8);
        let plaintext = make_identity(PEER_ID, 8);
        let result = TcpListenerService::identity_exchange(
            &mgr,
            &our_identity,
            &plaintext,
            &PEER_ID.to_string(),
            gen,
        )
        .await;
        let err = result.expect_err("protocolVersion change must fail the exchange");
        assert!(err.to_string().contains("mid-handshake"), "{err}");
    }

    #[tokio::test]
    async fn test_exchange_happy_path_sends_our_identity() {
        let (mgr, mut peer, _t, gen) = setup_link().await;
        let plaintext = make_identity(PEER_ID, 8);
        peer_send(
            &mut peer,
            &plaintext
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .await;

        let our_identity = make_identity(OUR_ID, 8);
        TcpListenerService::identity_exchange(
            &mgr,
            &our_identity,
            &plaintext,
            &PEER_ID.to_string(),
            gen,
        )
        .await
        .expect("valid exchange must succeed");

        // The peer must receive OUR encrypted identity back.
        let mut reader = BufReader::new(peer);
        let mut line = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_until(b'\n', &mut line),
        )
        .await
        .expect("read timeout")
        .expect("read");
        let packet = PacketSerializer::deserialize(&line).expect("deserialize");
        assert!(packet.is_identity());
        let identity = Identity::from_packet(packet).expect("identity");
        assert_eq!(identity.device_id, OUR_ID);
    }
}

#[cfg(test)]
mod handle_connection_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! Ownership-gated teardown in handle_connection's exchange-failure
    //! arms: the generation that OWNS the link tears lifecycle down, a
    //! generation replaced mid-exchange touches nothing. Driven end-to-end
    //! through the real accept/identity-exchange path over in-process TLS.

    use super::*;
    use crate::config::settings::AppSettings;
    use crate::protocol::connection::tls;
    use crate::protocol::crypto::CertificateManager;
    use crate::protocol::packet::PacketSerializer;
    use crate::protocol::Packet;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const OUR_ID: &str = "handler-our-device-aaaaaaaaaaaaaaaa";
    const PEER_ID: &str = "handler-peer-device-aaaaaaaaaaaaaa";

    fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
        let state = Arc::new(AppState::new_without_input(settings).expect("AppState"));
        state.connection_manager.set_device_identity(OUR_ID, "Us");
        state
            .cert_manager
            .ensure_own_certificate(OUR_ID, "Us")
            .expect("our cert");
        (state, temp_dir)
    }

    fn peer_cert_manager() -> (Arc<CertificateManager>, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let cm = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
        cm.init().expect("init");
        cm.ensure_own_certificate(PEER_ID, "Peer")
            .expect("peer cert");
        (cm, temp)
    }

    fn make_identity() -> Identity {
        let mut identity = Identity::new(
            PEER_ID.to_string(),
            "Peer".to_string(),
            crate::device::DeviceType::Phone,
            vec![],
            vec![],
        );
        identity.protocol_version = 8;
        identity
    }

    /// Accept one TCP connection and run the real handler on it.
    fn spawn_handler(
        state: &Arc<AppState>,
        listener: &Arc<tokio::net::TcpListener>,
    ) -> tokio::task::JoinHandle<()> {
        let state = state.clone();
        let listener = listener.clone();
        tokio::spawn(async move {
            let (stream, peer_addr) = listener.accept().await.expect("accept");
            let identity = state.connection_manager.get_identity().expect("identity");
            TcpListenerService::handle_connection(state, stream, peer_addr, identity).await;
        })
    }

    /// Peer side of the inbound path: TCP connect, plaintext identity, then
    /// TLS server role (handle_connection is the TLS client).
    async fn peer_connect(
        addr: std::net::SocketAddr,
        peer_certs: Arc<CertificateManager>,
    ) -> tls::TlsStream {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let bytes = PacketSerializer::serialize(
            &make_identity()
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .expect("serialize");
        tcp.write_all(&bytes).await.expect("write identity");
        tcp.flush().await.expect("flush");
        tls::tls_accept(peer_certs, None, tcp)
            .await
            .expect("peer tls_accept")
            .0
    }

    async fn peer_send(peer: &mut tls::TlsStream, packet: &Packet) {
        let bytes = PacketSerializer::serialize(packet).expect("serialize");
        peer.write_all(&bytes).await.expect("write");
        peer.flush().await.expect("flush");
    }

    /// Complete the encrypted identity exchange from the peer side (send
    /// ours, read theirs) so the handler proceeds to Connected + loop.
    async fn peer_complete_exchange(peer: &mut tls::TlsStream) {
        peer_send(
            peer,
            &make_identity()
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .await;
        let mut reader = BufReader::new(peer);
        let mut line = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_until(b'\n', &mut line),
        )
        .await
        .expect("read our identity")
        .expect("read");
    }

    async fn wait_lifecycle(state: &Arc<AppState>, want: DeviceState) -> bool {
        for _ in 0..75 {
            if state
                .lifecycle
                .get_state(&PEER_ID.to_string())
                .await
                .is_ok_and(|s| s == want)
            {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        false
    }

    /// The generation that OWNS the link must tear lifecycle down when its
    /// exchange fails — the pre-fix arms disconnected without teardown and
    /// left a zombie Connected behind (this test fails on that code).
    #[tokio::test]
    async fn test_owned_exchange_failure_tears_down_lifecycle() {
        let (state, _t) = test_state();
        let (peer_certs, _pt) = peer_cert_manager();
        let listener = Arc::new(
            tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind"),
        );
        let addr = listener.local_addr().expect("addr");

        // gen1: full exchange → Connected → packet loop.
        let _h1 = spawn_handler(&state, &listener);
        let mut peer1 = peer_connect(addr, peer_certs.clone()).await;
        peer_complete_exchange(&mut peer1).await;
        assert!(
            wait_lifecycle(&state, DeviceState::Connected).await,
            "gen1 must reach Connected"
        );

        // gen2: same-cert redial — replaces the gen1 link (Android
        // LanLink.reset; no health precondition) — then fails the exchange
        // with a non-identity packet. gen2 owns the link.
        let _h2 = spawn_handler(&state, &listener);
        let mut peer2 = peer_connect(addr, peer_certs.clone()).await;
        peer_send(&mut peer2, &Packet::ping()).await;

        assert!(
            wait_lifecycle(&state, DeviceState::Disconnected).await,
            "the owning generation's exchange failure must mark the device Disconnected"
        );
        assert!(
            !state
                .connection_manager
                .is_connected(&PEER_ID.to_string())
                .await
        );
    }

    /// A generation replaced MID-exchange must touch nothing when its
    /// exchange dies: disconnect returns not-owned, and the replacement's
    /// Connected lifecycle stands (guard against an over-eager gate).
    #[tokio::test]
    async fn test_stale_exchange_failure_leaves_replacement_lifecycle_untouched() {
        let (state, _t) = test_state();
        let (peer_certs, _pt) = peer_cert_manager();
        let listener = Arc::new(
            tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind"),
        );
        let addr = listener.local_addr().expect("addr");

        // gen1 fully connected.
        let _h1 = spawn_handler(&state, &listener);
        let mut peer1 = peer_connect(addr, peer_certs.clone()).await;
        peer_complete_exchange(&mut peer1).await;
        assert!(wait_lifecycle(&state, DeviceState::Connected).await);
        let gen1 = state
            .connection_manager
            .get_generation(&PEER_ID.to_string())
            .await
            .expect("gen1");

        // gen2: redial, exchange left unfinished (peer2 stays silent). A
        // same-cert redial replaces a healthy link too (Android
        // LanLink.reset).
        let h2 = spawn_handler(&state, &listener);
        let peer2 = peer_connect(addr, peer_certs.clone()).await;
        let mut replaced = false;
        for _ in 0..75 {
            if state
                .connection_manager
                .get_generation(&PEER_ID.to_string())
                .await
                != Some(gen1)
            {
                replaced = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        assert!(replaced, "gen2 must replace gen1");

        // gen3: redial replaces gen2 mid-exchange; complete it.
        let _h3 = spawn_handler(&state, &listener);
        let mut peer3 = peer_connect(addr, peer_certs.clone()).await;
        peer_complete_exchange(&mut peer3).await;
        assert!(
            wait_lifecycle(&state, DeviceState::Connected).await,
            "gen3 must reach Connected"
        );

        // gen2's exchange read is still parked on its evicted stream (the
        // eviction shut only the write half; the task's in-flight read
        // holds the read half). The peer giving up on the replaced socket
        // EOFs it — as the phone does when its redial wins.
        drop(peer2);

        // gen2's failure arm: its disconnect is not-owned (gen3 holds the
        // slot), so it must have touched nothing.
        tokio::time::timeout(std::time::Duration::from_secs(5), h2)
            .await
            .expect("stale handler must finish")
            .expect("join");
        assert!(
            state
                .lifecycle
                .get_state(&PEER_ID.to_string())
                .await
                .is_ok_and(|s| s == DeviceState::Connected),
            "stale exchange failure must not clobber the replacement's lifecycle"
        );
        assert!(
            state
                .connection_manager
                .is_connected(&PEER_ID.to_string())
                .await
        );
    }

    /// Hostile-peer scenario: the peer completes the link, sends a PARTIAL
    /// JSON packet (a truncated pair request), and closes. The truncated
    /// bytes can never deserialize, so nothing is dispatched (pair state
    /// untouched) — the read path treats it as link death: Disconnected,
    /// connection slot freed, handler task ends, and the next dial gets a
    /// fresh link.
    #[tokio::test]
    async fn test_mid_packet_close_is_link_death_not_dispatch() {
        let (state, _t) = test_state();
        let (peer_certs, _pt) = peer_cert_manager();
        let listener = Arc::new(
            tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind"),
        );
        let addr = listener.local_addr().expect("addr");

        let handler = spawn_handler(&state, &listener);
        let mut peer = peer_connect(addr, peer_certs.clone()).await;
        peer_complete_exchange(&mut peer).await;
        assert!(wait_lifecycle(&state, DeviceState::Connected).await);

        // Partial packet — a truncated pair request. If these bytes were
        // ever dispatched, the pairing handler would move to
        // RequestedByPeer.
        peer.write_all(b"{\"id\": 1, \"type\": \"kdeconnect.pair\", \"body\": {\"pair\": tr")
            .await
            .expect("write partial");
        peer.flush().await.expect("flush partial");
        drop(peer);

        assert!(
            wait_lifecycle(&state, DeviceState::Disconnected).await,
            "a mid-packet close must transition the device to Disconnected"
        );
        assert!(
            !state
                .connection_manager
                .is_connected(&PEER_ID.to_string())
                .await,
            "the dead link's slot must be freed"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), handler)
            .await
            .expect("the handler must not hang on a mid-packet close")
            .expect("join");
        assert_eq!(
            state.pairing_handler.pair_state(&PEER_ID.to_string()).await,
            crate::protocol::pairing::PairState::NotPaired,
            "the truncated packet must not have been dispatched"
        );

        // The next dial gets a fresh, working link.
        let _h2 = spawn_handler(&state, &listener);
        let mut peer2 = peer_connect(addr, peer_certs.clone()).await;
        peer_complete_exchange(&mut peer2).await;
        assert!(
            wait_lifecycle(&state, DeviceState::Connected).await,
            "the redial after a mid-packet close must connect"
        );
    }

    /// Hostile-peer scenario, RST variant: same truncated-packet trick, but
    /// the peer aborts the socket (SO_LINGER=0 → RST on close). The read
    /// path sees a reset error instead of an EOF — same required outcome:
    /// link death, Disconnected, slot freed, no dispatch.
    #[tokio::test]
    async fn test_mid_packet_rst_is_link_death_not_dispatch() {
        let (state, _t) = test_state();
        let (peer_certs, _pt) = peer_cert_manager();
        let listener = Arc::new(
            tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind"),
        );
        let addr = listener.local_addr().expect("addr");

        let handler = spawn_handler(&state, &listener);
        let mut peer = peer_connect(addr, peer_certs.clone()).await;
        peer_complete_exchange(&mut peer).await;
        assert!(wait_lifecycle(&state, DeviceState::Connected).await);

        peer.write_all(b"{\"id\": 1, \"type\": \"kdeconnect.pair\", \"body\": {\"pair\": tr")
            .await
            .expect("write partial");
        peer.flush().await.expect("flush partial");
        // Abortive close: unread/queued data is discarded with a RST.
        let (tcp, _conn) = peer.get_ref();
        socket2::SockRef::from(tcp)
            .set_linger(Some(std::time::Duration::from_secs(0)))
            .expect("set linger");
        drop(peer);

        assert!(
            wait_lifecycle(&state, DeviceState::Disconnected).await,
            "a mid-packet RST must transition the device to Disconnected"
        );
        assert!(
            !state
                .connection_manager
                .is_connected(&PEER_ID.to_string())
                .await,
            "the reset link's slot must be freed"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), handler)
            .await
            .expect("the handler must not hang on a mid-packet RST")
            .expect("join");
        assert_eq!(
            state.pairing_handler.pair_state(&PEER_ID.to_string()).await,
            crate::protocol::pairing::PairState::NotPaired,
            "the truncated packet must not have been dispatched"
        );
    }
}

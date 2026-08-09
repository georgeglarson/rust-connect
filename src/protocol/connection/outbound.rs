use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::device::types::DeviceId;
use crate::protocol::keepalive::configure_keepalive;
use crate::protocol::packet::PacketSerializer;
use crate::protocol::types::Identity;
use crate::utils::errors::{Error, Result};

use super::{
    read_line_bounded, tls, Connection, ConnectionInfo, ConnectionManager, CONNECTION_RATE_LIMIT,
    MAX_PACKET_SIZE,
};

impl ConnectionManager {
    /// Raw TLS connect with NO identity exchange or validation beyond TOFU.
    /// Test-only: production connection establishment is `connect_to_device`.
    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn connect(&self, device_id: &DeviceId, addr: SocketAddr) -> Result<u64> {
        let tcp_stream =
            tokio::time::timeout(std::time::Duration::from_secs(10), TcpStream::connect(addr))
                .await
                .map_err(|_| {
                    Error::ConnectionTimeout(format!("TCP connect to {} timed out", addr))
                })?
                .map_err(|e| {
                    Error::ConnectionError(format!(
                        "Failed to connect to {} at {}: {}",
                        device_id, addr, e
                    ))
                })?;

        configure_keepalive(&tcp_stream);

        let generation = {
            let mut gens = self.generations.write().await;
            let gen = gens.get(device_id).unwrap_or(&0) + 1;
            gens.insert(device_id.clone(), gen);
            gen
        };

        let (tls_stream, peer_cert_der) =
            tls::tls_connect(self.cert_manager.clone(), device_id, tcp_stream).await?;

        if let Some(ref cert_der) = peer_cert_der {
            if let Err(e) = self
                .cert_manager
                .verify_peer_certificate(device_id, cert_der)
            {
                error!(device_id = %device_id, error = %e, "Certificate fingerprint verification failed - rejecting connection");
                return Err(e);
            }
        }

        let (read_half, write_half) = tokio::io::split(tls_stream);
        let connection = Connection {
            read_stream: Mutex::new(BufReader::new(read_half)),
            write_stream: Mutex::new(write_half),
            info: Mutex::new(ConnectionInfo::new(device_id.clone(), generation)),
            generation,
            peer_cert: peer_cert_der,
            peer_addr: Some(addr),
        };

        let mut connections = self.connections.write().await;
        connections.insert(device_id.clone(), Arc::new(connection));

        info!(
            device_id = %device_id,
            address = %addr,
            event = "connection_established",
            "Established TLS connection"
        );

        Ok(generation)
    }

    /// Outbound connection to a discovered (or manually specified) peer.
    ///
    /// `expected_identity` is the pre-TLS identity we dialed on (the UDP
    /// broadcast), when there is one. Mirroring the inbound exchange (P6;
    /// LanLinkProvider.java:316-327), the encrypted identity received after
    /// the TLS handshake must carry the SAME deviceId and protocolVersion —
    /// a mid-handshake change aborts the connection. `None` (manual connect
    /// by address) skips the comparison, as Android has no pre-TLS identity
    /// to compare against in that flow either.
    pub async fn connect_to_device(
        &self,
        our_identity: &Identity,
        addr: SocketAddr,
        expected_identity: Option<&Identity>,
    ) -> Result<(DeviceId, Identity, u64)> {
        self.connect_to_device_with_fallback_port(
            our_identity,
            addr,
            expected_identity,
            crate::protocol::types::DEFAULT_UDP_PORT,
        )
        .await
    }

    /// `connect_to_device`'s real implementation, parameterized by the UDP
    /// port the reverse-connection fallback (gap 5) targets. The port is
    /// ALWAYS `DEFAULT_UDP_PORT` in production (the public wrapper above
    /// hardcodes it) — the parameter exists so tests can point the
    /// fallback at a private capture socket instead of the real port
    /// 1716, which a live kdeconnect/rust-connect daemon on the test host
    /// may already hold (observed on this dev box; a hardcoded-1716
    /// capture test would be flaky-to-failing on any host actually
    /// running the daemon it's testing).
    async fn connect_to_device_with_fallback_port(
        &self,
        our_identity: &Identity,
        addr: SocketAddr,
        expected_identity: Option<&Identity>,
        fallback_udp_port: u16,
    ) -> Result<(DeviceId, Identity, u64)> {
        {
            let remote_ip = addr.ip().to_string();
            let mut attempts = self.last_connection_attempt.write().await;
            if let Some(last) = attempts.get(&remote_ip) {
                if last.elapsed() < CONNECTION_RATE_LIMIT {
                    return Err(Error::ConnectionError(format!(
                        "Rate limited: too soon since last connection attempt to {}",
                        remote_ip
                    )));
                }
            }
            attempts.insert(remote_ip.clone(), Instant::now());
            attempts.retain(|_, v| v.elapsed() < Duration::from_secs(60));
        }

        // Fallback 1 (parity-checklist.md gap 5; lanlinkprovider.cpp
        // connectError, :343-354): the TCP dial we're making in response
        // to a received UDP/mDNS identity can fail outright. Tell the
        // peer to try dialing US instead, by sending our identity back to
        // them as a UDP unicast — they handle a unicast identity exactly
        // like a broadcast (Android's udpPacketReceived draws no
        // distinction) and dial us.
        let tcp_stream = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            TcpStream::connect(addr),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                send_reverse_connection_fallback(our_identity, addr.ip(), fallback_udp_port).await;
                return Err(Error::ConnectionError(format!(
                    "Failed to connect to {}: {}",
                    addr, e
                )));
            }
            Err(_) => {
                send_reverse_connection_fallback(our_identity, addr.ip(), fallback_udp_port).await;
                return Err(Error::ConnectionTimeout(format!(
                    "TCP connect to {} timed out",
                    addr
                )));
            }
        };

        configure_keepalive(&tcp_stream);

        // Fallback 2 (parity-checklist.md gap 5; lanlinkprovider.cpp
        // tcpSocketConnected's write-failure leg, :395-399): the dial
        // succeeded but the plaintext identity write itself failed — same
        // recovery.
        let identity_packet = our_identity.to_tcp_packet()?;
        let identity_bytes = PacketSerializer::serialize(&identity_packet)?;
        let mut tcp_stream = tcp_stream;
        if let Err(e) = tcp_stream.write_all(&identity_bytes).await {
            send_reverse_connection_fallback(our_identity, addr.ip(), fallback_udp_port).await;
            return Err(Error::ConnectionError(format!(
                "Failed to send plaintext identity: {}",
                e
            )));
        }
        if let Err(e) = tcp_stream.flush().await {
            send_reverse_connection_fallback(our_identity, addr.ip(), fallback_udp_port).await;
            return Err(Error::ConnectionError(format!(
                "Failed to flush plaintext identity: {}",
                e
            )));
        }

        info!(
            target_device = %addr,
            event = "plaintext_identity_sent",
            "Sent plaintext identity to device"
        );

        let our_id = {
            let guard = self.device_id.read().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };

        self.cert_manager
            .ensure_own_certificate(&our_id, "rust-connect")?;

        // We initiated TCP, so the peer drives the TLS client role; we are
        // the TLS server and must REQUEST its client cert (SslHelper
        // wantClientAuth — the peer's device id is not known until the
        // encrypted identity arrives below).
        debug!(target_device = %addr, "Waiting for Android to initiate TLS as client...");
        let (mut tls_stream, peer_cert_der) = match tokio::time::timeout(
            tokio::time::Duration::from_secs(15),
            tls::tls_accept(self.cert_manager.clone(), None, tcp_stream),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(Error::ConnectionError(format!(
                    "TLS server handshake timed out connecting to {}",
                    addr
                )));
            }
        };

        info!(
            target_device = %addr,
            event = "tls_server_handshake_complete",
            "TLS handshake completed (acting as TLS server)"
        );

        let our_encrypted_identity = our_identity.to_tcp_packet()?;
        let identity_bytes = PacketSerializer::serialize(&our_encrypted_identity)?;
        {
            tls_stream.write_all(&identity_bytes).await.map_err(|e| {
                Error::ConnectionError(format!("Failed to send encrypted identity: {}", e))
            })?;
            tls_stream.flush().await.map_err(|e| {
                Error::ConnectionError(format!("Failed to flush encrypted identity: {}", e))
            })?;
        }

        info!(
            target_device = %addr,
            event = "encrypted_identity_sent",
            "Sent encrypted identity over TLS"
        );

        let mut reader = BufReader::new(tls_stream);
        let mut line = Vec::new();
        let len = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_line_bounded(&mut reader, &mut line, MAX_PACKET_SIZE),
        )
        .await
        .map_err(|_| Error::ConnectionTimeout(format!("Timed out reading identity from {}", addr)))?
        .map_err(|e| match e {
            Error::PacketTooLarge { .. } => e,
            other => {
                Error::ConnectionError(format!("Failed to read encrypted identity: {}", other))
            }
        })?;

        if len == 0 {
            return Err(Error::ConnectionError(
                "Connection closed before sending identity".to_string(),
            ));
        }

        let packet = PacketSerializer::deserialize(&line)?;
        if !packet.is_identity() {
            return Err(Error::InvalidPacket(format!(
                "Expected identity packet, got {}",
                packet.packet_type
            )));
        }

        let remote_identity = Identity::from_packet(packet)?;
        let device_id = remote_identity.device_id.clone();
        crate::protocol::crypto::validate_device_id(&device_id)?;

        if let Some(expected) = expected_identity {
            if device_id != expected.device_id {
                return Err(Error::InvalidPacket(format!(
                    "Encrypted identity deviceId '{}' does not match pre-TLS identity '{}'",
                    device_id, expected.device_id
                )));
            }
            if remote_identity.protocol_version != expected.protocol_version {
                return Err(Error::InvalidPacket(format!(
                    "Encrypted identity protocolVersion {} differs from pre-TLS {} (mid-handshake change rejected)",
                    remote_identity.protocol_version, expected.protocol_version
                )));
            }
        }

        if let Some(ref cert_der) = peer_cert_der {
            let cert_cn = crate::protocol::crypto::extract_cn_from_der(cert_der)?;
            if cert_cn != device_id {
                return Err(Error::CertificateError(format!(
                    "Certificate CN '{}' does not match device ID '{}' from identity packet",
                    cert_cn, device_id
                )));
            }
            if let Err(e) = self
                .cert_manager
                .verify_peer_certificate(&device_id, cert_der)
            {
                error!(device_id = %device_id, error = %e, "Certificate fingerprint verification failed - rejecting connection");
                return Err(e);
            }
        } else if self.cert_manager.has_peer_fingerprint(&device_id) {
            return Err(Error::CertificateError(format!(
                "Expected peer certificate for device {} but none presented",
                device_id
            )));
        }

        self.record_peer_capabilities(
            &device_id,
            &remote_identity.incoming_capabilities,
            &remote_identity.outgoing_capabilities,
        )
        .await;

        let generation = {
            let mut gens = self.generations.write().await;
            let gen = gens.get(&device_id).unwrap_or(&0) + 1;
            gens.insert(device_id.clone(), gen);
            gen
        };

        let (read_half, write_half) = tokio::io::split(reader.into_inner());
        let connection = Connection {
            read_stream: Mutex::new(BufReader::new(read_half)),
            write_stream: Mutex::new(write_half),
            info: Mutex::new(ConnectionInfo::new(device_id.clone(), generation)),
            generation,
            peer_cert: peer_cert_der,
            peer_addr: Some(addr),
        };

        let old_handle = {
            let connections = self.connections.read().await;
            connections.get(&device_id).cloned()
        };

        if let Some(ref old) = old_handle {
            info!(
                device_id = %device_id,
                event = "outgoing_connection_replacing",
                "Replacing existing connection with new outgoing connection"
            );
            // Cancel the old packet loop BEFORE removing its token — removing
            // without cancelling leaves a zombie run_packet_loop reading a
            // dead stream until it errors out on its own.
            self.cancel_loop(&device_id).await;
            self.remove_cancel_token(&device_id).await;
            {
                let mut write_stream = old.write_stream.lock().await;
                let _ = write_stream.shutdown().await;
            }
        }

        {
            let mut connections = self.connections.write().await;
            connections.insert(device_id.clone(), Arc::new(connection));
        }

        self.cleanup_stale_generations().await;

        info!(
            device_id = %device_id,
            device_name = %remote_identity.device_name,
            address = %addr,
            event = "outgoing_device_connected",
            "Outgoing connection established with TOFU verification"
        );

        Ok((device_id, remote_identity, generation))
    }
}

/// Reverse-connection fallback (parity-checklist.md gap 5; kde
/// `lanlinkprovider.cpp` `connectError()` :343-354 and the
/// `tcpSocketConnected()` write-failure leg :395-399): send OUR identity
/// back to `peer_ip` as a UDP unicast on `udp_port`, the same
/// `to_packet()`-less UDP shape the broadcaster uses (tcpPort present —
/// Android silently drops an identity outside 1716-1764, live-proven
/// 2026-07-29). Fires at most once per failed dial — no retry loop; the
/// caller's own connection rate limit covers repeat attempts to the same
/// address. Best-effort: every failure here is logged and swallowed, never
/// masking the ORIGINAL dial failure the caller is already returning.
async fn send_reverse_connection_fallback(
    our_identity: &Identity,
    peer_ip: std::net::IpAddr,
    udp_port: u16,
) {
    let packet = match our_identity.to_packet() {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error = %e,
                event = "reverse_fallback_build_failed",
                "Failed to build the reverse-connection fallback identity"
            );
            return;
        }
    };
    let bytes = match PacketSerializer::serialize(&packet) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                error = %e,
                event = "reverse_fallback_serialize_failed",
                "Failed to serialize the reverse-connection fallback identity"
            );
            return;
        }
    };
    // Bind in the peer's address family: an `0.0.0.0` socket cannot
    // `send_to` an IPv6 target (the fallback would silently no-op on v6
    // links — bot round, PR #14).
    let bind_addr: SocketAddr = match peer_ip {
        std::net::IpAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        std::net::IpAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = match tokio::net::UdpSocket::bind(bind_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                error = %e,
                event = "reverse_fallback_bind_failed",
                "Failed to bind a UDP socket for the reverse-connection fallback"
            );
            return;
        }
    };
    let target = SocketAddr::new(peer_ip, udp_port);
    match socket.send_to(&bytes, target).await {
        Ok(_) => {
            info!(
                target = %target,
                event = "reverse_fallback_sent",
                "Sent reverse-connection identity fallback after a failed outbound dial"
            );
        }
        Err(e) => {
            warn!(
                error = %e,
                target = %target,
                event = "reverse_fallback_send_failed",
                "Failed to send the reverse-connection fallback identity"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::DeviceType;
    use crate::protocol::crypto::CertificateManager;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn test_identity() -> Identity {
        Identity::new(
            "fallback-senderaaaaaaaaaaaaaaaaaaaa".to_string(),
            "Fallback Sender".to_string(),
            DeviceType::Desktop,
            vec!["kdeconnect.ping".to_string()],
            vec!["kdeconnect.ping".to_string()],
        )
    }

    /// The identity sent back must be the SAME UDP-broadcast shape the
    /// discovery broadcaster uses (tcpPort present, no `to_tcp_packet()`
    /// stripping) — Android silently drops an identity with no tcpPort in
    /// range (live-proven 2026-07-29, per the brief).
    #[tokio::test]
    async fn test_reverse_fallback_sends_valid_udp_shaped_identity() {
        let capture = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind capture socket");
        let capture_port = capture.local_addr().expect("local_addr").port();

        let identity = test_identity();
        send_reverse_connection_fallback(&identity, Ipv4Addr::LOCALHOST.into(), capture_port).await;

        let mut buf = vec![0u8; 65536];
        let (len, _addr) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            capture.recv_from(&mut buf),
        )
        .await
        .expect("the fallback datagram must arrive")
        .expect("recv_from");

        let packet = PacketSerializer::deserialize(&buf[..len]).expect("must deserialize");
        assert!(
            packet.is_identity(),
            "fallback must send an identity packet"
        );
        let received = Identity::from_packet(packet).expect("must be a valid identity");
        assert_eq!(received.device_id, identity.device_id);
        let tcp_port = received.tcp_port.expect("tcpPort must be present");
        assert!(
            (crate::protocol::types::MIN_PORT..=crate::protocol::types::MAX_PORT)
                .contains(&tcp_port),
            "tcpPort must be in kdeconnect's 1716-1764 range, got {tcp_port}"
        );
    }

    /// An IPv6 peer must receive the fallback too: a socket bound to
    /// `0.0.0.0` cannot `send_to` a v6 target, so the bind must follow the
    /// peer's address family (bot round, PR #14 — same defect class as the
    /// cypher round-1 IPv6 `format!` fix in connection_orchestrator).
    #[tokio::test]
    async fn test_reverse_fallback_reaches_an_ipv6_peer() {
        let capture = match tokio::net::UdpSocket::bind("[::1]:0").await {
            Ok(s) => s,
            // Host without IPv6 loopback (some CI containers): nothing to
            // test — the v4 leg is covered above.
            Err(_) => return,
        };
        let capture_port = capture.local_addr().expect("local_addr").port();

        let identity = test_identity();
        send_reverse_connection_fallback(&identity, Ipv6Addr::LOCALHOST.into(), capture_port).await;

        let mut buf = vec![0u8; 65536];
        let (len, _addr) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            capture.recv_from(&mut buf),
        )
        .await
        .expect("the fallback datagram must arrive on the v6 capture socket")
        .expect("recv_from");

        let packet = PacketSerializer::deserialize(&buf[..len]).expect("must deserialize");
        assert!(
            packet.is_identity(),
            "fallback must send an identity packet"
        );
    }

    /// A build/serialize/send failure must never panic — it's a best-
    /// effort recovery path riding on top of an already-failed dial.
    #[tokio::test]
    async fn test_reverse_fallback_send_failure_does_not_panic() {
        // Port 0 as a DESTINATION is invalid (nothing can bind or listen
        // there); the send itself either no-ops or errors depending on
        // platform — either way this must not panic.
        let identity = test_identity();
        send_reverse_connection_fallback(&identity, Ipv4Addr::LOCALHOST.into(), 0).await;
    }

    /// Gap 5 behavioral test 1: a TCP dial that never connects (nothing
    /// listening) must trigger exactly one fallback datagram, and the
    /// ORIGINAL connect failure must still be the error returned — the
    /// fallback is a side effect, never a replacement for honest failure
    /// reporting.
    #[tokio::test]
    async fn test_connect_to_device_dial_failure_sends_reverse_fallback_once() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
        cert_manager.init().expect("init");
        let cm = ConnectionManager::new(cert_manager).expect("new");
        cm.set_device_identity("dial-fail-senderaaaaaaaaaaaaaaaaaa", "Us");
        let our_identity = cm.get_identity().expect("identity");

        // Bind then immediately drop: guarantees nothing is listening at
        // this exact port, so the TCP connect is refused (not merely slow).
        let hermetic = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let dead_addr = hermetic.local_addr().expect("local_addr");
        drop(hermetic);

        let capture = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind capture");
        let capture_port = capture.local_addr().expect("local_addr").port();

        let result = cm
            .connect_to_device_with_fallback_port(&our_identity, dead_addr, None, capture_port)
            .await;
        assert!(
            result.is_err(),
            "a refused dial must still surface as an error"
        );

        let mut buf = vec![0u8; 65536];
        let (len, _addr) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            capture.recv_from(&mut buf),
        )
        .await
        .expect("the fallback datagram must arrive after the dial failure")
        .expect("recv_from");
        let received =
            Identity::from_packet(PacketSerializer::deserialize(&buf[..len]).expect("deserialize"))
                .expect("valid identity");
        assert_eq!(received.device_id, "dial-fail-senderaaaaaaaaaaaaaaaaaa");

        // Exactly once — no retry loop.
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            capture.recv_from(&mut buf),
        )
        .await;
        assert!(
            second.is_err(),
            "the fallback must fire at most once per failed dial"
        );
    }

    /// Gap 5 behavioral test 2: the plaintext-identity-write failure leg
    /// specifically (not fallback 1's TCP-connect leg, and not a later TLS
    /// failure). Simulated by accepting the TCP connection and immediately
    /// resetting it (SO_LINGER(0) + drop) before our side writes.
    ///
    /// A SMALL identity write silently succeeds regardless of the peer's
    /// state — under this project's single-threaded test runtime, the
    /// client task runs write_all() to completion (buffered locally by the
    /// kernel) before the separately-spawned acceptor task ever gets a
    /// chance to run its reset, EVERY time (confirmed empirically across
    /// repeated runs: the write always "succeeded" and the error surfaced
    /// a step later, at the TLS handshake — never at this leg). Padding
    /// the identity's capability lists forces write_all() into multiple
    /// real syscalls, which yields control between them and lets the
    /// acceptor's reset land first.
    ///
    /// That same padding is a real, unavoidable tension with observing the
    /// FALLBACK datagram this leg sends: `send_reverse_connection_fallback`
    /// resends this exact (now-huge) identity as a raw UDP datagram with
    /// NO size retry (upstream's own connectError()/tcpSocketConnected()
    /// fallbacks don't apply gap 6's empty-caps retry either — confirmed
    /// against lanlinkprovider.cpp, a plain `writeDatagram` with no error
    /// handling at all), so a payload big enough to force the write
    /// failure is also too big for its own fallback send to fit under
    /// IPv4's 65507-byte UDP ceiling. Empirically, no payload size sits on
    /// both sides of that line on this host's default socket buffers:
    /// asserting on the SPECIFIC write-failure error message (proving leg
    /// 2's branch fired, not leg 1's or TLS's) is the honest ceiling here.
    /// The fallback SEND mechanism itself — a valid UDP-shaped identity,
    /// correct target, fires once — is independently proven by
    /// `test_reverse_fallback_sends_valid_udp_shaped_identity` and by
    /// `test_connect_to_device_dial_failure_sends_reverse_fallback_once`
    /// (leg 1), which call the exact same `send_reverse_connection_fallback`
    /// this leg calls.
    #[tokio::test]
    async fn test_connect_to_device_write_failure_triggers_the_write_failure_branch() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
        cert_manager.init().expect("init");
        let cm = ConnectionManager::new(cert_manager).expect("new");
        cm.set_device_identity("write-fail-senderaaaaaaaaaaaaaaaa", "Us");
        let padding: Vec<String> = (0..40_000)
            .map(|i| format!("kdeconnect.padding.capability.number.{i}"))
            .collect();
        cm.set_capabilities(padding.clone(), padding);
        let our_identity = cm.get_identity().expect("identity");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        // MUST be spawned before the connect below (accept() blocks until
        // something connects) — awaiting the acceptor first would deadlock,
        // nothing left to connect it.
        let acceptor = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            // Force an RST on our side's next write/flush instead of a
            // graceful close: set SO_LINGER(0) then drop. `set_linger` on
            // std::net::TcpStream is unstable; socket2::SockRef reaches
            // the same setsockopt on stable (same crate discovery.rs
            // already uses for SO_RCVBUF).
            let std_stream = stream.into_std().expect("into_std");
            socket2::SockRef::from(&std_stream)
                .set_linger(Some(std::time::Duration::ZERO))
                .expect("set_linger");
            drop(std_stream);
        });

        // A throwaway port: the fallback datagram this leg sends is too
        // large to actually land (see the doc comment above) — nothing
        // needs to be listening here, only the write-failure branch itself
        // is under test.
        let unused_fallback_port = 1;

        let result = cm
            .connect_to_device_with_fallback_port(&our_identity, addr, None, unused_fallback_port)
            .await;
        acceptor.await.expect("acceptor task");

        let err = result.expect_err("a write against a reset peer must still surface as an error");
        assert!(
            err.to_string()
                .contains("Failed to send plaintext identity"),
            "must fail specifically in the write-failure branch (leg 2), not leg 1 or TLS: {err}"
        );
    }

    /// Regression pin: a successful connect must send NO fallback
    /// datagram at all.
    #[tokio::test]
    async fn test_connect_to_device_success_sends_no_fallback() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
        cert_manager.init().expect("init");
        let cm = Arc::new(ConnectionManager::new(cert_manager.clone()).expect("new"));
        let our_id = "success-senderaaaaaaaaaaaaaaaaaaaa";
        cert_manager
            .ensure_own_certificate(our_id, "Us")
            .expect("own cert");
        cm.set_device_identity(our_id, "Us");
        let our_identity = cm.get_identity().expect("identity");

        let remote_id = "success-receiveraaaaaaaaaaaaaaaaaa";
        let remote_temp = tempfile::TempDir::new().expect("tempdir");
        let remote_certs = Arc::new(CertificateManager::new(remote_temp.path().to_path_buf()));
        remote_certs.init().expect("init");
        let remote_cm = ConnectionManager::new(remote_certs).expect("new");
        remote_cm.set_device_identity(remote_id, "Receiver");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            remote_cm
                .accept_test_as_client(our_id.to_string(), stream, addr.port())
                .await
                .expect("accept_test_as_client");
        });

        let capture = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind capture");
        let capture_port = capture.local_addr().expect("local_addr").port();

        let result = cm
            .connect_to_device_with_fallback_port(&our_identity, addr, None, capture_port)
            .await;
        assert!(result.is_ok(), "the dial must succeed: {result:?}");

        let mut buf = vec![0u8; 65536];
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            capture.recv_from(&mut buf),
        )
        .await;
        assert!(
            second.is_err(),
            "a successful connect must never send a reverse-connection fallback"
        );

        server_handle.await.expect("server task");
    }
}

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

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

        let tcp_stream =
            tokio::time::timeout(std::time::Duration::from_secs(10), TcpStream::connect(addr))
                .await
                .map_err(|_| {
                    Error::ConnectionTimeout(format!("TCP connect to {} timed out", addr))
                })?
                .map_err(|e| {
                    Error::ConnectionError(format!("Failed to connect to {}: {}", addr, e))
                })?;

        configure_keepalive(&tcp_stream);

        let identity_packet = our_identity.to_tcp_packet()?;
        let identity_bytes = PacketSerializer::serialize(&identity_packet)?;
        let mut tcp_stream = tcp_stream;
        tcp_stream.write_all(&identity_bytes).await.map_err(|e| {
            Error::ConnectionError(format!("Failed to send plaintext identity: {}", e))
        })?;
        tcp_stream.flush().await.map_err(|e| {
            Error::ConnectionError(format!("Failed to flush plaintext identity: {}", e))
        })?;

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

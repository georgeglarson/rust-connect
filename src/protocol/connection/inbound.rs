use std::sync::Arc;

#[cfg(any(test, feature = "test-helpers"))]
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
#[cfg(any(test, feature = "test-helpers"))]
use tracing::debug;
use tracing::{error, info, trace, warn};

use crate::device::types::DeviceId;
use crate::protocol::keepalive::configure_keepalive;
use crate::protocol::packet::PacketSerializer;
use crate::protocol::types::Identity;
use crate::utils::errors::{Error, Result};

use super::{
    read_line_bounded, tls, Connection, ConnectionInfo, ConnectionManager, MAX_PACKET_SIZE,
};

impl ConnectionManager {
    /// Accept an inbound TCP connection: plaintext identity → TLS (client
    /// role) → register the link. Returns the link the caller must drive
    /// (identity exchange + packet loop). A same-cert duplicate REPLACES
    /// the existing link (Android `LanLink.reset` semantics).
    pub async fn accept_incoming(
        &self,
        tcp_stream: TcpStream,
    ) -> Result<(DeviceId, Identity, u64)> {
        let peer_addr = tcp_stream.peer_addr().ok();
        trace!(peer = ?peer_addr, event = "accept_incoming_start", "Starting accept_incoming");

        // Same keepalive as the outbound path (30s idle / 10s interval /
        // 3 probes, kdeconnectd BUG 476747): a half-dead accepted link is
        // observed dead within ~60s instead of hanging forever.
        configure_keepalive(&tcp_stream);

        // Android tcpPacketReceived (LanLinkProvider.java:138-141): refuse
        // connections from non-local (non-private) IPs.
        if let Some(addr) = peer_addr {
            if !crate::protocol::is_private_address(&addr.ip()) {
                return Err(Error::ConnectionError(format!(
                    "Discarding connection from a non-local IP: {}",
                    addr.ip()
                )));
            }
        }

        let mut reader = BufReader::new(tcp_stream);
        let mut line = Vec::new();
        let len = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_line_bounded(&mut reader, &mut line, MAX_PACKET_SIZE),
        )
        .await
        .map_err(|_| Error::ConnectionTimeout("Timed out reading plaintext identity".to_string()))?
        .map_err(|e| match e {
            Error::PacketTooLarge { .. } => e,
            other => {
                Error::ConnectionError(format!("Failed to read plaintext identity: {}", other))
            }
        })?;

        trace!(
            bytes_read = len,
            event = "plaintext_identity_read",
            "Read {} bytes of plaintext identity",
            len
        );

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

        let our_id = self
            .device_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        // Self-guard: never accept a peer claiming our own device id (e.g. a
        // reflected broadcast or a spoofed identity). Android discards
        // self-ids; so do we, on every path, not just UDP.
        if device_id == our_id {
            return Err(Error::ConnectionError(format!(
                "Rejecting incoming connection from self (device_id {})",
                device_id
            )));
        }

        // Android tcpPacketReceived (LanLinkProvider.java:169-178): an
        // identity addressed to a different targetDeviceId or
        // targetProtocolVersion is not meant for us — drop the connection.
        if let Some(ref target) = remote_identity.target_device_id {
            if *target != our_id {
                return Err(Error::ConnectionError(format!(
                    "Received a connection request for a device that isn't us: {}",
                    target
                )));
            }
        }
        if let Some(target_version) = remote_identity.target_protocol_version {
            if target_version != crate::protocol::types::PROTOCOL_VERSION {
                return Err(Error::ConnectionError(format!(
                    "Received a connection request for a protocol version that isn't ours: {}",
                    target_version
                )));
            }
        }

        info!(
            device_id = %device_id,
            device_name = %remote_identity.device_name,
            device_type = %remote_identity.device_type,
            protocol_version = remote_identity.protocol_version,
            event = "plaintext_identity_received",
            "Received plaintext identity from device"
        );

        trace!(device_id = %device_id, event = "starting_tls_handshake", "Starting TLS handshake as CLIENT (Android is TLS server)");

        let raw_tcp = reader.into_inner();

        let (tls_stream, peer_cert_der) =
            tls::tls_connect(self.cert_manager.clone(), &device_id, raw_tcp).await?;

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
                error!(device_id = %device_id, error = %e, "Certificate fingerprint verification failed - rejecting incoming connection");
                return Err(e);
            }
            trace!(device_id = %device_id, event = "peer_cert_stored", "Peer certificate stored successfully");
        } else if self.cert_manager.has_peer_fingerprint(&device_id) {
            return Err(Error::CertificateError(format!(
                "Expected peer certificate for device {} but none presented",
                device_id
            )));
        }

        info!(
            device_id = %device_id,
            event = "tls_handshake_complete",
            "TLS handshake completed (rustls client, Android was TLS server)"
        );

        self.record_peer_capabilities(
            &device_id,
            &remote_identity.incoming_capabilities,
            &remote_identity.outgoing_capabilities,
        )
        .await;

        let (read_half, write_half) = tokio::io::split(tls_stream);

        // Duplicate-inbound policy lives here, after the handshake, because
        // it keys on the certificate. On a same-cert duplicate, Android's
        // LanLinkProvider.addOrUpdateLink calls LanLink.reset(newSocket,
        // deviceInfo): the NEW socket is adopted into the existing link
        // object and the OLD socket is closed (the "Reusing same link" log
        // line refers to the link object, not the old socket). kdeconnectd's
        // LanLinkProvider::addLink does the same (deviceLink->reset(socket)).
        // So we ALWAYS replace on a same-cert duplicate, healthy or dead:
        // the earlier keep-old behavior fought both reference implementations
        // and caused a reconnect storm against any broadcasting peer — the
        // dialer completed a full TLS handshake per broadcast, we dropped
        // the new socket, and it redialed on the next announcement.
        // Cert-compare, generation bump, and insert run under one
        // connections write-lock scope so two simultaneous duplicates can't
        // interleave; generations are always locked inside `connections`
        // here, never the reverse.
        let (generation, evicted) = {
            let mut connections = self.connections.write().await;
            if let Some(old) = connections.get(&device_id) {
                // (None, None) can't reach this branch: on the inbound path
                // we are the TLS CLIENT (tls_connect), and a TLS server that
                // presents no certificate cannot complete the rustls
                // handshake, so `peer_cert_der` is always Some here (the
                // None arm above is defensive). A None on `old` alone is
                // reachable (an outbound pre-pairing link from a certless
                // peer) and correctly reads as "cert changed" → reject.
                let same_cert = matches!(
                    (&old.peer_cert, &peer_cert_der),
                    (Some(old_cert), Some(new_cert)) if old_cert == new_cert
                );
                if !same_cert {
                    warn!(
                        device_id = %device_id,
                        event = "incoming_replacement_cert_changed",
                        "Replacement connection presented a different certificate, aborting"
                    );
                    return Err(Error::CertificateError(format!(
                        "Device {} already connected and presented a different certificate, rejecting replacement",
                        device_id
                    )));
                }
                info!(
                    device_id = %device_id,
                    event = "incoming_connection_replacing",
                    "Same-cert redial: adopting the new socket, evicting the existing link (Android LanLink.reset)"
                );
            }

            let generation = {
                let mut gens = self.generations.write().await;
                let gen = gens.get(&device_id).unwrap_or(&0) + 1;
                gens.insert(device_id.clone(), gen);
                gen
            };

            let connection = Connection {
                read_stream: Mutex::new(BufReader::new(read_half)),
                write_stream: Mutex::new(write_half),
                info: Mutex::new(ConnectionInfo::new(device_id.clone(), generation)),
                generation,
                peer_cert: peer_cert_der.clone(),
                peer_addr,
            };

            let evicted = connections.insert(device_id.clone(), Arc::new(connection));
            (generation, evicted)
        };

        if let Some(old) = evicted {
            // Cancel the old packet loop BEFORE removing its token — removing
            // without cancelling leaves a zombie run_packet_loop reading the
            // new stream (same ordering as the outbound replace path).
            self.cancel_loop(&device_id).await;
            self.remove_cancel_token(&device_id).await;
            {
                let mut write_stream = old.write_stream.lock().await;
                let _ = write_stream.shutdown().await;
            }
        }

        trace!(device_id = %device_id, event = "accept_incoming_complete", "accept_incoming completed successfully");

        Ok((device_id, remote_identity, generation))
    }

    /// Raw TLS accept-as-client with NO identity exchange. Dead in
    /// production (no callers); kept for test harnesses only.
    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn accept_as_client(
        &self,
        device_id: DeviceId,
        tcp_stream: TcpStream,
    ) -> Result<u64> {
        let peer_addr = tcp_stream.peer_addr().ok();
        let (tls_stream, peer_cert_der) =
            tls::tls_connect(self.cert_manager.clone(), &device_id, tcp_stream).await?;

        if let Some(ref cert_der) = peer_cert_der {
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

        let (read_half, write_half) = tokio::io::split(tls_stream);
        let connection = Connection {
            read_stream: Mutex::new(BufReader::new(read_half)),
            write_stream: Mutex::new(write_half),
            info: Mutex::new(ConnectionInfo::new(device_id.clone(), generation)),
            generation,
            peer_cert: peer_cert_der.clone(),
            peer_addr,
        };

        let mut connections = self.connections.write().await;
        connections.insert(device_id.clone(), Arc::new(connection));

        info!(
            device_id = %device_id,
            event = "connection_accepted_as_client",
            "Accepted TCP connection and initiated TLS as client"
        );

        Ok(generation)
    }

    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn accept_test(&self, device_id: DeviceId, tcp_stream: TcpStream) -> Result<u64> {
        let peer_addr = tcp_stream.peer_addr().ok();
        let our_id = {
            let guard = self.device_id.read().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };

        self.cert_manager
            .ensure_own_certificate(&our_id, "Test Server")?;

        let (tls_stream, peer_cert) =
            tls::tls_accept(self.cert_manager.clone(), None, tcp_stream).await?;

        let generation = {
            let mut gens = self.generations.write().await;
            let gen = gens.get(&device_id).unwrap_or(&0) + 1;
            gens.insert(device_id.clone(), gen);
            gen
        };

        let (read_half, write_half) = tokio::io::split(tls_stream);
        let connection = Connection {
            read_stream: Mutex::new(BufReader::new(read_half)),
            write_stream: Mutex::new(write_half),
            info: Mutex::new(ConnectionInfo::new(device_id.clone(), generation)),
            generation,
            peer_cert,
            peer_addr,
        };

        let mut connections = self.connections.write().await;
        connections.insert(device_id.clone(), Arc::new(connection));

        info!(device_id = %device_id, event = "test_connection_accepted", "Accepted test TLS connection");
        Ok(generation)
    }

    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn accept_test_as_client(
        &self,
        device_id: DeviceId,
        tcp_stream: TcpStream,
        tcp_port: u16,
    ) -> Result<u64> {
        let peer_addr = tcp_stream.peer_addr().ok();
        let our_id = {
            let guard = self.device_id.read().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };

        self.cert_manager
            .ensure_own_certificate(&our_id, "Test Client")?;

        let mut reader = BufReader::new(tcp_stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.map_err(|e| {
            Error::ConnectionError(format!("Failed to read plaintext identity: {}", e))
        })?;

        let _peer_identity_packet =
            PacketSerializer::deserialize(line.as_bytes()).map_err(|e| {
                Error::DeserializationError(format!("Invalid plaintext identity: {}", e))
            })?;

        let raw_tcp = reader.into_inner();

        let (raw_tls_stream, handshake_peer_cert) =
            tls::tls_connect(self.cert_manager.clone(), &device_id, raw_tcp).await?;

        let our_identity = {
            let guard = self.device_id.read().unwrap_or_else(|e| e.into_inner());
            let name = self
                .device_name
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut identity = Identity::new(
                guard.clone(),
                name,
                crate::device::DeviceType::Desktop,
                self.incoming_caps
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                self.outgoing_caps
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            );
            identity.tcp_port = Some(tcp_port);
            identity.target_device_id = Some(device_id.clone());
            identity.target_protocol_version = Some(8);
            identity
        };
        let our_identity_packet = our_identity.to_tcp_packet()?;

        let identity_bytes = PacketSerializer::serialize(&our_identity_packet)?;
        let mut tls_stream = raw_tls_stream;
        tls_stream.write_all(&identity_bytes).await.map_err(|e| {
            Error::ConnectionError(format!("Failed to send encrypted identity: {}", e))
        })?;
        tls_stream.flush().await.map_err(|e| {
            Error::ConnectionError(format!("Failed to flush encrypted identity: {}", e))
        })?;

        let mut tls_reader = BufReader::new(tls_stream);
        let mut line = String::new();
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tls_reader.read_line(&mut line),
        )
        .await
        .map_err(|_| Error::ConnectionError("Timed out reading encrypted identity".to_string()))?
        .map_err(|e| Error::ConnectionError(format!("Failed to read encrypted identity: {}", e)))?;

        if read_result == 0 {
            return Err(Error::ConnectionError(
                "Connection closed during encrypted identity exchange".to_string(),
            ));
        }

        let _encrypted_identity = PacketSerializer::deserialize(line.as_bytes()).map_err(|e| {
            Error::DeserializationError(format!("Invalid encrypted identity: {}", e))
        })?;

        let generation = {
            let mut gens = self.generations.write().await;
            let gen = gens.get(&device_id).unwrap_or(&0) + 1;
            gens.insert(device_id.clone(), gen);
            gen
        };

        let peer_cert = handshake_peer_cert;
        let (read_half, write_half) = tokio::io::split(tls_reader.into_inner());
        let connection = Connection {
            read_stream: Mutex::new(BufReader::new(read_half)),
            write_stream: Mutex::new(write_half),
            info: Mutex::new(ConnectionInfo::new(device_id.clone(), generation)),
            generation,
            peer_cert,
            peer_addr,
        };

        let mut connections = self.connections.write().await;
        connections.insert(device_id.clone(), Arc::new(connection));

        Ok(generation)
    }

    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn accept_test_with_port(
        &self,
        device_id: DeviceId,
        tcp_stream: TcpStream,
        tcp_port: u16,
    ) -> Result<u64> {
        let peer_addr = tcp_stream.peer_addr().ok();
        let our_id = {
            let guard = self.device_id.read().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };

        self.cert_manager
            .ensure_own_certificate(&our_id, "Test Server")?;

        let mut reader = BufReader::new(tcp_stream);
        debug!(device_id = %device_id, "Waiting for plaintext identity from peer");
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await.map_err(|e| {
            Error::ConnectionError(format!("Failed to read plaintext identity: {}", e))
        })?;
        debug!(device_id = %device_id, bytes_read, "Read {} bytes of plaintext identity", bytes_read);

        let _peer_identity_packet =
            PacketSerializer::deserialize(line.as_bytes()).map_err(|e| {
                Error::DeserializationError(format!("Invalid plaintext identity: {}", e))
            })?;

        info!(device_id = %device_id, event = "plaintext_identity_received",
            "Received plaintext identity from peer");

        let raw_tcp = reader.into_inner();

        debug!(device_id = %device_id, "Initiating TLS as client");

        let (raw_tls_stream, handshake_peer_cert) =
            tls::tls_connect(self.cert_manager.clone(), &device_id, raw_tcp).await?;

        debug!(device_id = %device_id, "TLS handshake complete");

        let our_identity = {
            let guard = self.device_id.read().unwrap_or_else(|e| e.into_inner());
            let name = self
                .device_name
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut identity = Identity::new(
                guard.clone(),
                name,
                crate::device::DeviceType::Desktop,
                self.incoming_caps
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                self.outgoing_caps
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            );
            identity.tcp_port = Some(tcp_port);
            identity.target_device_id = Some(device_id.clone());
            identity.target_protocol_version = Some(8);
            identity
        };
        let our_identity_packet = our_identity.to_tcp_packet()?;

        let identity_bytes = PacketSerializer::serialize(&our_identity_packet)?;
        debug!(device_id = %device_id, "Sending encrypted identity ({} bytes)", identity_bytes.len());
        let mut tls_stream = raw_tls_stream;
        tls_stream.write_all(&identity_bytes).await.map_err(|e| {
            Error::ConnectionError(format!("Failed to send encrypted identity: {}", e))
        })?;
        tls_stream.flush().await.map_err(|e| {
            Error::ConnectionError(format!("Failed to flush encrypted identity: {}", e))
        })?;

        let mut tls_reader = BufReader::new(tls_stream);
        let mut line = String::new();
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tls_reader.read_line(&mut line),
        )
        .await
        .map_err(|_| Error::ConnectionError("Timed out reading encrypted identity".to_string()))?
        .map_err(|e| Error::ConnectionError(format!("Failed to read encrypted identity: {}", e)))?;

        if read_result == 0 {
            return Err(Error::ConnectionError(
                "Connection closed during encrypted identity exchange".to_string(),
            ));
        }

        debug!(device_id = %device_id, "Received encrypted identity: {}", line.trim());
        let _encrypted_identity = PacketSerializer::deserialize(line.as_bytes()).map_err(|e| {
            Error::DeserializationError(format!("Invalid encrypted identity: {}", e))
        })?;

        let generation = {
            let mut gens = self.generations.write().await;
            let gen = gens.get(&device_id).unwrap_or(&0) + 1;
            gens.insert(device_id.clone(), gen);
            gen
        };

        let peer_cert = handshake_peer_cert;
        let (read_half, write_half) = tokio::io::split(tls_reader.into_inner());
        let connection = Connection {
            read_stream: Mutex::new(BufReader::new(read_half)),
            write_stream: Mutex::new(write_half),
            info: Mutex::new(ConnectionInfo::new(device_id.clone(), generation)),
            generation,
            peer_cert,
            peer_addr,
        };

        let mut connections = self.connections.write().await;
        connections.insert(device_id.clone(), Arc::new(connection));

        info!(
            device_id = %device_id,
            event = "test_connection_accepted",
            "Accepted test connection with full protocol handshake"
        );

        Ok(generation)
    }
}

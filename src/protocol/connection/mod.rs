use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, trace, warn};

use tokio_util::sync::CancellationToken;

use crate::device::types::DeviceId;
use crate::protocol::crypto::CertificateManager;
use crate::protocol::packet::PacketSerializer;
use crate::protocol::transcript::{self, Direction};
use crate::protocol::types::{ConnectionInfo, Identity, Packet};
use crate::utils::errors::{Error, Result};

mod inbound;
mod outbound;
pub(crate) mod tls;

pub(crate) use tls::TlsStream;

pub(crate) struct Connection {
    pub(crate) read_stream: Mutex<BufReader<tokio::io::ReadHalf<TlsStream>>>,
    pub(crate) write_stream: Mutex<tokio::io::WriteHalf<TlsStream>>,
    pub(crate) info: Mutex<ConnectionInfo>,
    pub(crate) generation: u64,
    pub(crate) peer_cert: Option<Vec<u8>>,
    pub(crate) peer_addr: Option<std::net::SocketAddr>,
}

pub struct ConnectionManager {
    pub(crate) connections: Arc<RwLock<HashMap<DeviceId, Arc<Connection>>>>,
    pub(crate) cancel_tokens: Arc<RwLock<HashMap<DeviceId, CancellationToken>>>,
    pub(crate) last_connection_attempt: Arc<RwLock<HashMap<DeviceId, Instant>>>,
    pub pending_connections: Arc<std::sync::Mutex<std::collections::HashSet<DeviceId>>>,
    pub(crate) cert_manager: Arc<CertificateManager>,
    pub(crate) device_id: Arc<std::sync::RwLock<String>>,
    pub(crate) device_name: Arc<std::sync::RwLock<String>>,
    pub incoming_caps: Arc<std::sync::RwLock<Vec<String>>>,
    pub outgoing_caps: Arc<std::sync::RwLock<Vec<String>>>,
    pub(crate) generations: Arc<RwLock<HashMap<DeviceId, u64>>>,
    /// The actual bound TCP port, once known (Task 2.3 gap A plumbing):
    /// `get_identity()` uses this instead of always advertising
    /// `DEFAULT_TCP_PORT`, which matters for the reverse-connection
    /// fallback's UDP identity — Android silently drops a tcpPort outside
    /// 1716-1764 (live-proven 2026-07-29), and if the daemon actually
    /// bound a DIFFERENT free port in that range (1716 taken), the
    /// fallback identity must carry the real one. `None` (the default for
    /// any caller that never calls `set_tcp_port`, e.g. tests) preserves
    /// the prior DEFAULT_TCP_PORT behavior exactly.
    pub(crate) tcp_port: Arc<std::sync::RwLock<Option<u16>>>,
    /// Peer's last-known advertised `incomingCapabilities` (Task 2.3 gap D;
    /// `core/device.cpp:358-363` `Device::sendPacket`). Populated on a
    /// fresh identity exchange (`accept_incoming`, `connect_to_device`)
    /// with the SAME non-empty-both guard as
    /// `Device::apply_capability_update` (device/types.rs) — a
    /// legitimately empty pre-exchange identity, or a hostile empty-caps
    /// one, must not erase capabilities already learned. No entry means
    /// "unknown" (never gates) rather than "empty" (would gate
    /// everything) — see `record_peer_capabilities` and `send_packet`'s
    /// gate for why that distinction is load-bearing for the pairing flow.
    pub(crate) peer_capabilities: Arc<RwLock<HashMap<DeviceId, Vec<String>>>>,
}

/// Android caps identity/packet lines at 512 KiB (LanLinkProvider.java:68,
/// MAX_IDENTITY_PACKET_SIZE).
pub(crate) const MAX_PACKET_SIZE: usize = 524_288;
/// The references' STEADY-STATE line cap: 32 MiB (landevicelink.cpp:19,
/// LanLink.java:46). Applies only after the link is established — the
/// pre-auth identity reads keep the 512 KiB cap above.
pub(crate) const MAX_STEADY_PACKET_SIZE: usize = 32 * 1024 * 1024;
/// Android allows a minimum of 1 second between connections to the same
/// device (LanLinkProvider.java:71,
/// MILLIS_DELAY_BETWEEN_CONNECTIONS_TO_SAME_DEVICE). `pub` (not
/// `pub(crate)`) so the fault suite (Task 2.4, vk #990) can pace its
/// redial scenarios off the real value instead of guessing a sleep
/// duration that silently drifts from it.
pub const CONNECTION_RATE_LIMIT: Duration = Duration::from_millis(1000);

/// Read one `\n`-delimited packet line, enforcing `max_len` DURING the read
/// rather than after it: as soon as the accumulated bytes would exceed the
/// cap the read fails with `PacketTooLarge`, without waiting for the
/// delimiter or EOF. Mirrors Android's `readLineBounded`
/// (LanLinkProvider.java:153,308) — a plain `read_until(b'\n', …)` buffers
/// unboundedly from a peer that never sends `\n`.
///
/// Semantics otherwise match `read_until(b'\n', ...)`: the delimiter is
/// included in `buf`, the number of bytes appended is returned, and 0 means
/// EOF with nothing read.
pub(crate) async fn read_line_bounded<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_len: usize,
) -> Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    let start = buf.len();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len() - start);
        }
        let newline_at = available.iter().position(|&b| b == b'\n');
        let take = newline_at.map_or(available.len(), |p| p + 1);
        if buf.len() + take > max_len {
            return Err(Error::PacketTooLarge {
                size: buf.len() + take,
                max: max_len,
            });
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline_at.is_some() {
            return Ok(buf.len() - start);
        }
    }
}

/// Outcome of one steady-state line read.
pub(crate) enum SteadyLine {
    /// A `\n`-terminated line (or a partial line at EOF) is in `buf`.
    Line(usize),
    /// A line exceeded the cap: it was consumed to its delimiter and
    /// DISCARDED. The references skip oversized lines instead of killing
    /// the link (landevicelink.cpp:98-101, LanLink.java:85-88).
    OversizeSkipped,
    /// EOF with nothing read.
    Eof,
}

/// Steady-state variant of `read_line_bounded`: same framing, but a line
/// over `max_len` is skipped (consumed and discarded, reported as
/// `OversizeSkipped`) instead of failing the read — the next call reads the
/// NEXT line. The link must survive a peer's oversized packet.
pub(crate) async fn read_steady_line<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_len: usize,
) -> Result<SteadyLine>
where
    R: AsyncBufRead + Unpin,
{
    let start = buf.len();
    let mut discarding = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if discarding {
                // EOF mid-discard: the link is closing anyway; let the
                // caller take its normal EOF path.
                SteadyLine::Eof
            } else if buf.len() == start {
                SteadyLine::Eof
            } else {
                SteadyLine::Line(buf.len() - start)
            });
        }
        let newline_at = available.iter().position(|&b| b == b'\n');
        let take = newline_at.map_or(available.len(), |p| p + 1);
        if discarding {
            reader.consume(take);
            if newline_at.is_some() {
                return Ok(SteadyLine::OversizeSkipped);
            }
            continue;
        }
        if buf.len() + take > max_len {
            // Cap exceeded: drop the accumulated bytes and consume the rest
            // of the line without buffering it.
            buf.truncate(start);
            discarding = true;
            reader.consume(take);
            if newline_at.is_some() {
                return Ok(SteadyLine::OversizeSkipped);
            }
            continue;
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline_at.is_some() {
            return Ok(SteadyLine::Line(buf.len() - start));
        }
    }
}

impl ConnectionManager {
    pub fn new(cert_manager: Arc<CertificateManager>) -> Result<Self> {
        Ok(Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            cancel_tokens: Arc::new(RwLock::new(HashMap::new())),
            last_connection_attempt: Arc::new(RwLock::new(HashMap::new())),
            pending_connections: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            cert_manager,
            device_id: Arc::new(std::sync::RwLock::new(String::new())),
            device_name: Arc::new(std::sync::RwLock::new("Rust Connect".to_string())),
            incoming_caps: Arc::new(std::sync::RwLock::new(vec![])),
            outgoing_caps: Arc::new(std::sync::RwLock::new(vec![])),
            generations: Arc::new(RwLock::new(HashMap::new())),
            tcp_port: Arc::new(std::sync::RwLock::new(None)),
            peer_capabilities: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn add_pending_connection(&self, device_id: &DeviceId) -> bool {
        let mut pending = self
            .pending_connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let added = pending.insert(device_id.clone());
        if !added {
            tracing::info!(device_id = %device_id, event = "duplicate_pending", "Connection already pending");
        }
        added
    }

    pub async fn remove_pending_connection(&self, device_id: &DeviceId) {
        let mut pending = self
            .pending_connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.remove(device_id);
    }

    pub fn set_capabilities(&self, incoming: Vec<String>, outgoing: Vec<String>) {
        *self
            .incoming_caps
            .write()
            .unwrap_or_else(|e| e.into_inner()) = incoming;
        *self
            .outgoing_caps
            .write()
            .unwrap_or_else(|e| e.into_inner()) = outgoing;
    }

    pub fn set_device_identity(&self, device_id: &str, device_name: &str) {
        *self.device_id.write().unwrap_or_else(|e| e.into_inner()) = device_id.to_string();
        *self.device_name.write().unwrap_or_else(|e| e.into_inner()) = device_name.to_string();
    }

    /// Records the ACTUAL bound TCP port, once `listener::bind_port` has
    /// resolved it (Task 2.3 gap A plumbing — see the `tcp_port` field's
    /// doc). Call once at startup, after binding, alongside
    /// `set_device_identity`/`set_capabilities`.
    pub fn set_tcp_port(&self, port: u16) {
        *self.tcp_port.write().unwrap_or_else(|e| e.into_inner()) = Some(port);
    }

    pub fn get_identity(&self) -> Option<Identity> {
        let device_id = self
            .device_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let device_name = self
            .device_name
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if device_id.is_empty() {
            return None;
        }
        let mut identity = Identity::new(
            device_id,
            device_name,
            crate::device::types::DeviceType::Desktop,
            self.incoming_caps
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            self.outgoing_caps
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        );
        if let Some(port) = *self.tcp_port.read().unwrap_or_else(|e| e.into_inner()) {
            identity.tcp_port = Some(port);
        }
        Some(identity)
    }

    /// Records the peer's advertised `incomingCapabilities` from a fresh
    /// identity exchange, applying the same non-empty-both guard as
    /// `Device::apply_capability_update` (device/types.rs): the update
    /// lands only when BOTH the incoming and outgoing lists on the new
    /// identity are non-empty. Real peers always send both; a legitimately
    /// empty pre-exchange identity, or a hostile empty-caps one, must not
    /// erase (or falsely establish) gating capabilities. Feeds
    /// `send_packet`'s gate (Task 2.3 gap D, `core/device.cpp:358-363`).
    pub(crate) async fn record_peer_capabilities(
        &self,
        device_id: &DeviceId,
        incoming: &[String],
        outgoing: &[String],
    ) {
        if incoming.is_empty() || outgoing.is_empty() {
            return;
        }
        let mut caps = self.peer_capabilities.write().await;
        caps.insert(device_id.clone(), incoming.to_vec());
    }

    pub async fn send_packet(&self, device_id: &DeviceId, packet: &Packet) -> Result<()> {
        trace!(
            device_id = %device_id,
            packet_type = %packet.packet_type,
            event = "send_packet_start",
            "Starting send_packet"
        );

        let conn_handle = {
            let connections = self.connections.read().await;
            connections.get(device_id).cloned().ok_or_else(|| {
                Error::ConnectionError(format!("No connection for device: {}", device_id))
            })?
        };

        // Send-side capability gating (Task 2.3 gap D, `core/device.cpp:
        // 358-363` `Device::sendPacket`): kdeconnect.identity and
        // kdeconnect.pair are exempt exactly as upstream exempts
        // isProtocolPacket(); everything else is refused when the peer's
        // last-known incomingCapabilities are NON-EMPTY and don't list
        // this packet type. A device we've never exchanged capabilities
        // with (no map entry — legitimately empty pre-identity-exchange,
        // e.g. mid-pairing) is NOT gated: upstream gates unconditionally
        // because its caps always arrive with identity, but ordering here
        // isn't guaranteed the same way, and gating on unknown caps would
        // silently start failing plugin sends in the existing pairing-flow
        // test suite (see record_peer_capabilities's doc + the brief's
        // named call).
        if !packet.is_identity() && !packet.is_pair() {
            let caps = self.peer_capabilities.read().await;
            if let Some(incoming) = caps.get(device_id) {
                if !incoming.iter().any(|c| c == &packet.packet_type) {
                    warn!(
                        device_id = %device_id,
                        packet_type = %packet.packet_type,
                        event = "capability_not_supported",
                        "Refusing to send a packet type the peer hasn't advertised support for"
                    );
                    return Err(Error::CapabilityNotSupported {
                        device_id: device_id.clone(),
                        packet_type: packet.packet_type.clone(),
                    });
                }
            }
        }

        let bytes = PacketSerializer::serialize(packet)?;
        trace!(
            device_id = %device_id,
            packet_type = %packet.packet_type,
            bytes_len = bytes.len(),
            event = "packet_serialized",
            "Packet serialized to {} bytes", bytes.len()
        );
        conn_handle.info.lock().await.last_activity = chrono::Utc::now();

        let mut write_stream = conn_handle.write_stream.lock().await;

        match tokio::time::timeout(
            std::time::Duration::from_millis(100),
            write_stream.write_all(&bytes),
        )
        .await
        {
            Ok(Ok(())) => {
                trace!(
                    device_id = %device_id,
                    event = "write_all_complete",
                    "write_all completed successfully, flushing..."
                );
                let flush_result = tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    write_stream.flush(),
                )
                .await;
                match flush_result {
                    Ok(Ok(())) => {
                        trace!(device_id = %device_id, event = "flush_complete", "Flush completed");
                    }
                    Ok(Err(e)) => {
                        error!(
                            device_id = %device_id,
                            error = %e,
                            event = "packet_flush_failed",
                            "Failed to flush packet"
                        );
                        return Err(Error::ConnectionError(format!(
                            "Failed to flush packet to {}: {}",
                            device_id, e
                        )));
                    }
                    Err(_) => {
                        error!(
                            device_id = %device_id,
                            event = "packet_flush_timeout",
                            "Timed out flushing packet"
                        );
                        return Err(Error::ConnectionTimeout(format!(
                            "Timed out flushing packet to {}",
                            device_id
                        )));
                    }
                }
                conn_handle.info.lock().await.increment_sent();
                debug!(
                    device_id = %device_id,
                    packet_type = %packet.packet_type,
                    event = "packet_sent",
                    "Sent packet"
                );
                transcript::record(device_id, Direction::Out, packet);
                Ok(())
            }
            Ok(Err(e)) => {
                error!(
                    device_id = %device_id,
                    error = %e,
                    event = "packet_send_failed",
                    "Failed to send packet"
                );
                Err(Error::ConnectionError(format!(
                    "Failed to send packet to {}: {}",
                    device_id, e
                )))
            }
            Err(_) => {
                error!(
                    device_id = %device_id,
                    event = "packet_send_timeout",
                    "Timed out sending packet"
                );
                Err(Error::ConnectionTimeout(format!(
                    "Timed out sending packet to {}",
                    device_id
                )))
            }
        }
    }

    /// Unguarded read, for test harnesses that hold no loop generation.
    /// Production readers (packet loop, identity exchange) MUST use
    /// `recv_packet_current` — see its doc for the replacement race.
    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn recv_packet(&self, device_id: &DeviceId) -> Result<Packet> {
        self.recv_packet_inner(device_id, None).await
    }

    /// Read the next packet for a SPECIFIC connection generation. After a
    /// same-cert replacement (accept_incoming / connect_to_device) the map
    /// slot holds the NEW connection; a stale task reading through the slot
    /// would consume the replacement's bytes off its TLS stream — two
    /// readers interleave packets, the replacement's own identity exchange
    /// then fails and tears down the good link. Failing fast on a
    /// generation mismatch confines a stale reader to its own dead stream.
    pub async fn recv_packet_current(
        &self,
        device_id: &DeviceId,
        generation: u64,
    ) -> Result<Packet> {
        self.recv_packet_inner(device_id, Some(generation)).await
    }

    async fn recv_packet_inner(
        &self,
        device_id: &DeviceId,
        generation: Option<u64>,
    ) -> Result<Packet> {
        let conn_handle = {
            let connections = self.connections.read().await;
            let handle = connections.get(device_id).cloned().ok_or_else(|| {
                Error::ConnectionError(format!("No connection for device: {}", device_id))
            })?;
            if let Some(generation) = generation {
                if handle.generation != generation {
                    return Err(Error::ConnectionError(format!(
                        "Stale generation {} for device {} (link replaced; current generation {})",
                        generation, device_id, handle.generation
                    )));
                }
            }
            handle
        };

        // Steady-state read loop: oversized lines are consumed and skipped
        // and blank lines are ignored — neither kills the link (the
        // references' behavior, parity-checklist gaps 2 and 4). A MALFORMED
        // packet line still fails the link (Android LanLink.java:92-103).
        let packet = loop {
            let mut line = Vec::new();
            let read_result = {
                let mut read_stream = conn_handle.read_stream.lock().await;
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    read_steady_line(&mut *read_stream, &mut line, MAX_STEADY_PACKET_SIZE),
                )
                .await
            };

            match read_result {
                Err(_) => {
                    warn!(
                        device_id = %device_id,
                        event = "packet_recv_timeout",
                        "Timed out receiving packet"
                    );
                    return Err(Error::ConnectionTimeout(format!(
                        "Timed out receiving packet from {}",
                        device_id
                    )));
                }
                Ok(Ok(SteadyLine::Eof)) => {
                    // EOF: the peer closed the link.
                    warn!(
                        device_id = %device_id,
                        event = "connection_closed",
                        "Connection closed by remote (read returned 0 bytes)"
                    );
                    return Err(Error::ConnectionError(format!(
                        "Connection closed by {}",
                        device_id
                    )));
                }
                Ok(Ok(SteadyLine::OversizeSkipped)) => {
                    warn!(
                        device_id = %device_id,
                        event = "packet_oversize_skipped",
                        "Skipped a packet line over 32 MiB (link kept alive)"
                    );
                    continue;
                }
                Ok(Ok(SteadyLine::Line(n))) => {
                    if line.iter().all(|b| b.is_ascii_whitespace()) {
                        trace!(
                            device_id = %device_id,
                            event = "blank_line_skipped",
                            "Skipped a blank line"
                        );
                        continue;
                    }
                    trace!(
                        device_id = %device_id,
                        bytes_read = n,
                        event = "raw_bytes_received",
                        "Read {} bytes from TLS stream", n
                    );
                    conn_handle.info.lock().await.increment_received();
                    break PacketSerializer::deserialize(&line)?;
                }
                Ok(Err(e)) => {
                    error!(
                        device_id = %device_id,
                        error = %e,
                        event = "packet_recv_failed",
                        "Failed to receive packet"
                    );
                    return Err(Error::ConnectionError(format!(
                        "Failed to receive packet from {}: {}",
                        device_id, e
                    )));
                }
            }
        };
        debug!(
            device_id = %device_id,
            packet_type = %packet.packet_type,
            event = "packet_received",
            "Received packet"
        );
        transcript::record(device_id, Direction::In, &packet);
        Ok(packet)
    }

    /// Disconnect, guarded by connection generation. Returns `true` when the
    /// caller's generation still owns the link state — it removed the
    /// connection, or the slot was already empty — and `false` when a NEWER
    /// generation has replaced it. On `false` the replacement owns the map
    /// slot, the cancel-token slot, and the lifecycle/plugin state, and the
    /// caller must not touch any of them.
    pub async fn disconnect(&self, device_id: &DeviceId, generation: u64) -> Result<bool> {
        let mut connections = self.connections.write().await;
        if let Some(handle) = connections.get(device_id) {
            if handle.generation != generation {
                info!(
                    device_id = %device_id,
                    stale_generation = generation,
                    current_generation = handle.generation,
                    event = "stale_disconnect",
                    "Ignoring disconnect from stale connection"
                );
                return Ok(false);
            }
        }
        self.remove_cancel_token(device_id).await;
        let conn_handle = connections.remove(device_id);

        if let Some(handle) = conn_handle {
            let mut write_stream = handle.write_stream.lock().await;
            let _ = write_stream.shutdown().await;
            info!(
                device_id = %device_id,
                event = "connection_disconnected",
                "Disconnected device"
            );
        }
        Ok(true)
    }

    pub async fn is_connected(&self, device_id: &DeviceId) -> bool {
        let connections = self.connections.read().await;
        connections.contains_key(device_id)
    }

    pub async fn get_generation(&self, device_id: &DeviceId) -> Option<u64> {
        let connections = self.connections.read().await;
        let handle = connections.get(device_id)?;
        Some(handle.generation)
    }

    /// True while the live connection for `device_id` is exactly
    /// `generation` — i.e. the caller has not been replaced by a same-cert
    /// redial.
    pub async fn is_current_generation(&self, device_id: &DeviceId, generation: u64) -> bool {
        let connections = self.connections.read().await;
        connections
            .get(device_id)
            .is_some_and(|handle| handle.generation == generation)
    }

    /// Unguarded token registration, for test harnesses that hold no loop
    /// generation. Production paths MUST use
    /// `register_cancel_token_if_current` — see its doc for the race.
    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn register_cancel_token(&self, device_id: &DeviceId, token: CancellationToken) {
        let mut tokens = self.cancel_tokens.write().await;
        tokens.insert(device_id.clone(), token);
    }

    /// Cancel-token registration scoped to a connection generation:
    /// validates the generation and inserts atomically (connections →
    /// cancel_tokens, the established lock order). A same-cert replacement
    /// landing between the caller's last generation check and registration
    /// must not have its token slot overwritten by a stale handler — the
    /// replacement's loop would become uncancellable. Returns false,
    /// registering nothing, when the caller's generation is no longer live.
    pub async fn register_cancel_token_if_current(
        &self,
        device_id: &DeviceId,
        token: CancellationToken,
        generation: u64,
    ) -> bool {
        let connections = self.connections.read().await;
        let mut tokens = self.cancel_tokens.write().await;
        let current = connections
            .get(device_id)
            .is_some_and(|handle| handle.generation == generation);
        if current {
            tokens.insert(device_id.clone(), token);
        }
        current
    }

    pub async fn cancel_loop(&self, device_id: &DeviceId) {
        let tokens = self.cancel_tokens.read().await;
        if let Some(token) = tokens.get(device_id) {
            token.cancel();
        }
    }

    pub async fn remove_cancel_token(&self, device_id: &DeviceId) {
        let mut tokens = self.cancel_tokens.write().await;
        tokens.remove(device_id);
    }

    /// Cancel-token removal scoped to a connection generation: a stale
    /// packet loop (replaced by a same-cert redial) must not strip the
    /// replacement's freshly-registered token, or the replacement's loop
    /// can never be cancelled. Returns whether the caller's generation is
    /// still the live one; the token is removed only when it is. Lock order
    /// is `connections` then `cancel_tokens` — the same order `disconnect`
    /// takes them, and no path takes them in reverse.
    pub async fn remove_cancel_token_if_current(
        &self,
        device_id: &DeviceId,
        generation: u64,
    ) -> bool {
        let connections = self.connections.read().await;
        let mut tokens = self.cancel_tokens.write().await;
        let current = connections
            .get(device_id)
            .is_some_and(|handle| handle.generation == generation);
        if current {
            tokens.remove(device_id);
        }
        current
    }

    pub async fn get_connection_info(&self, device_id: &DeviceId) -> Option<ConnectionInfo> {
        let connections = self.connections.read().await;
        let handle = connections.get(device_id)?;
        let info = handle.info.lock().await;
        Some(info.clone())
    }

    pub async fn get_peer_certificate(&self, device_id: &DeviceId) -> Option<Vec<u8>> {
        let connections = self.connections.read().await;
        if let Some(handle) = connections.get(device_id) {
            return handle.peer_cert.clone();
        }
        None
    }

    /// The remote address of the live link, if known. Fallback transfer
    /// address when a payloadTransferInfo arrives without an `ip` — the
    /// sender is necessarily the connected peer.
    pub async fn get_peer_addr(&self, device_id: &DeviceId) -> Option<std::net::SocketAddr> {
        let connections = self.connections.read().await;
        connections.get(device_id)?.peer_addr
    }

    pub async fn connected_device_ids(&self) -> Vec<DeviceId> {
        let connections = self.connections.read().await;
        connections.keys().cloned().collect()
    }

    pub async fn cleanup_stale_generations(&self) {
        let connected = self.connected_device_ids().await;
        let mut generations = self.generations.write().await;
        generations.retain(|device_id, _| connected.contains(device_id));
    }

    /// Test-only observability: number of registered cancel tokens. The
    /// `cancel_tokens` map itself is `pub(crate)` (no production caller needs
    /// its size), but the fault suite's resource-count assertions (Task 2.4,
    /// vk #990 — no token/map growth across a duplicate-dial storm or a
    /// stale-loop replacement) need to observe it from an external
    /// integration test. Mirrors the existing test/debug-only pattern
    /// (`recv_packet`, `register_cancel_token`) rather than widening the
    /// field's own visibility.
    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn cancel_token_count(&self) -> usize {
        self.cancel_tokens.read().await.len()
    }
}

#[cfg(test)]
mod tests;

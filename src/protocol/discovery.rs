//! Discovery service for KDE Connect protocol
//!
//! Following AI-first design principles:
//! - Structured logging
//! - Clear error messages
//! - Single responsibility
//!
//! Single Responsibility: Handle UDP broadcast and listening for device discovery

use socket2::{Domain, Protocol, Socket, Type as SockType};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::protocol::packet::PacketSerializer;
use crate::protocol::types::{Identity, MAX_PORT, MIN_PORT};
use crate::utils::errors::{Error, Result};

/// Target UDP receive-buffer capacity (parity-checklist.md § Robustness
/// gap 4). Matches android's `LanLinkProvider.java:69`. Used both as the
/// requested SO_RCVBUF (kernel queue capacity — see the comment at the
/// `set_recv_buffer_size` call site for why that's the real fix here,
/// not single-datagram size) and as the userspace read buffer in
/// `listen()`. The read buffer was already comfortably above the max
/// possible IPv4 UDP payload (65507 bytes) at its old 64 KiB size, so
/// raising it changes no observable truncation behavior for real
/// traffic; it's raised anyway to match this constant and remove any
/// doubt for a future reader.
const RECV_BUFFER_SIZE: usize = 512 * 1024;

/// EMSGSIZE, the OS error a UDP send raises when a datagram is rejected as
/// too large: errno 90 on Linux, errno 40 on macOS/FreeBSD (outpost is the
/// fleet's only BSD-kernel host — see AGENTS.md's fleet roster). Rust's
/// stable `std::io::ErrorKind` has no dedicated variant for this, so match
/// the raw errno on both kernels this daemon ships on rather than string-
/// matching `to_string()`. Upstream's own comment names this as a
/// macOS/FreeBSD-specific MTU behavior for BROADCASTS specifically; Linux
/// can also hit it, but only past IPv4's hard 65507-byte UDP payload
/// ceiling (parity-checklist.md § Discovery "UDP receive buffer" row) —
/// either way, the fix is the same retry.
fn is_message_too_large(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(90) | Some(40))
}

/// Discovery service
///
/// Handles broadcasting identity packets and listening for other devices.
pub struct DiscoveryService {
    pub socket: UdpSocket,
    pub identity: Identity,
    pub broadcast_addr: SocketAddr,
}

impl DiscoveryService {
    /// Create a new discovery service
    ///
    /// # Arguments
    /// * `identity` - This device's identity
    /// * `udp_port` - Port to bind and broadcast on
    ///
    /// # Returns
    /// * `Ok(DiscoveryService)` - New discovery service
    /// * `Err(Error)` - Failed to bind UDP socket
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rust_connect::protocol::discovery::DiscoveryService;
    /// use rust_connect::protocol::types::{Identity, DEFAULT_UDP_PORT};
    /// use rust_connect::device::types::DeviceType;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let identity = Identity::new(
    ///     "my-device-id".to_string(),
    ///     "My Device".to_string(),
    ///     DeviceType::Desktop,
    ///     vec![],
    ///     vec![],
    /// );
    ///
    /// let service = DiscoveryService::new(identity, DEFAULT_UDP_PORT).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(identity: Identity, udp_port: u16) -> Result<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), udp_port);

        let socket = Socket::new(Domain::IPV4, SockType::DGRAM, Some(Protocol::UDP))
            .map_err(|e| Error::DiscoveryError(format!("Failed to create UDP socket: {}", e)))?;

        socket
            .set_reuse_address(true)
            .map_err(|e| Error::DiscoveryError(format!("Failed to set SO_REUSEADDR: {}", e)))?;

        // Kernel receive-QUEUE capacity (parity-checklist.md § Robustness
        // gap 4; android LanLinkProvider.java:69 sets 512 KiB). This is
        // NOT about the size of any single datagram — IPv4 caps a UDP
        // payload at 65507 bytes regardless of any buffer setting (65535
        // max IP total length - 20 byte IP header - 8 byte UDP header;
        // the kernel refuses to even send anything past that with
        // EMSGSIZE, verified empirically). It IS about how many
        // already-arrived-but-not-yet-read datagrams the kernel will
        // queue before dropping new ones — under a burst (several devices
        // broadcasting near-simultaneously, or a retry storm) a bigger
        // queue survives more of it. The OS default here
        // (`net.core.rmem_default`, ~208 KiB) sits below android's 512
        // KiB target; we were relying on it implicitly instead of setting
        // our own, unlike android. A failure to raise it is logged, not
        // fatal — discovery still works with a smaller queue, just with
        // less burst headroom.
        if let Err(e) = socket.set_recv_buffer_size(RECV_BUFFER_SIZE) {
            warn!(
                error = %e,
                requested = RECV_BUFFER_SIZE,
                event = "discovery_rcvbuf_not_set",
                "Could not raise the UDP receive buffer; falling back to the OS default"
            );
        } else if let Ok(effective) = socket.recv_buffer_size() {
            // Linux clamps the request to net.core.rmem_max and reports the
            // (doubled) result via getsockopt — setsockopt itself succeeds
            // silently even when clamped (socket(7)). On hosts with a low
            // rmem_max the effective queue is smaller than requested; say
            // so once at startup instead of leaving burst-drop behavior
            // unexplained (PR #12 review).
            if effective < RECV_BUFFER_SIZE {
                warn!(
                    requested = RECV_BUFFER_SIZE,
                    effective = effective,
                    event = "discovery_rcvbuf_clamped",
                    "Kernel clamped the UDP receive buffer below the requested \
                     size (net.core.rmem_max); burst broadcasts may drop earlier"
                );
            }
        }

        socket
            .set_nonblocking(true)
            .map_err(|e| Error::DiscoveryError(format!("Failed to set nonblocking: {}", e)))?;

        socket
            .bind(&addr.into())
            .map_err(|e| Error::DiscoveryError(format!("Failed to bind UDP socket: {}", e)))?;

        let std_socket: std::net::UdpSocket = socket.into();
        let socket = UdpSocket::from_std(std_socket)
            .map_err(|e| Error::DiscoveryError(format!("Failed to convert UDP socket: {}", e)))?;

        socket
            .set_broadcast(true)
            .map_err(|e| Error::DiscoveryError(format!("Failed to enable broadcast: {}", e)))?;

        info!(
            port = udp_port,
            event = "discovery_service_created",
            "Discovery service initialized"
        );

        Ok(Self {
            socket,
            identity,
            broadcast_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), udp_port),
        })
    }

    /// Broadcast this device's identity
    ///
    /// Sends identity packet to broadcast address. On macOS/FreeBSD, UDP
    /// broadcasts larger than the interface MTU get dropped by the kernel
    /// (`DatagramTooLargeError` upstream) — kde's fallback
    /// (`lanlinkprovider.cpp:259-269`, parity-checklist.md gap 6) strips
    /// both capability lists and resends the smaller identity. See
    /// `is_message_too_large` for why the same retry also matters on
    /// Linux for a sufficiently huge capability list.
    ///
    /// # Returns
    /// * `Ok(())` - Broadcast successful
    /// * `Err(Error)` - Broadcast failed
    pub async fn broadcast(&self) -> Result<()> {
        let packet = self.identity.to_packet()?;
        let bytes = PacketSerializer::serialize(&packet)?;

        if let Err(e) = self.socket.send_to(&bytes, self.broadcast_addr).await {
            if !is_message_too_large(&e) {
                return Err(Error::DiscoveryError(format!(
                    "Failed to send broadcast: {}",
                    e
                )));
            }

            warn!(
                error = %e,
                event = "identity_broadcast_oversized",
                "Identity packet rejected as too large for a UDP broadcast; \
                 retrying with capabilities stripped (kde lanlinkprovider.cpp:259-269)"
            );

            // kde's guard on the RECEIVE side (Task 2.1's
            // apply_capability_update, device/types.rs) only applies an
            // identity's capabilities when BOTH lists are non-empty —
            // that's what makes stripping our OWN capabilities here safe
            // to receive: a peer that already knows our real capabilities
            // keeps them instead of having this smaller retry wipe them.
            let mut stripped = self.identity.clone();
            stripped.incoming_capabilities.clear();
            stripped.outgoing_capabilities.clear();
            let small_packet = stripped.to_packet()?;
            let small_bytes = PacketSerializer::serialize(&small_packet)?;

            self.socket
                .send_to(&small_bytes, self.broadcast_addr)
                .await
                .map_err(|e| {
                    Error::DiscoveryError(format!(
                        "Failed to send broadcast (retry with emptied capabilities): {}",
                        e
                    ))
                })?;
        }

        debug!(
            device_id = %self.identity.device_id,
            device_name = %self.identity.device_name,
            event = "identity_broadcast",
            "Broadcast identity packet"
        );

        Ok(())
    }

    /// Listen for identity packets from other devices
    ///
    /// Blocks until a packet is received or an error occurs.
    ///
    /// # Returns
    /// * `Ok((Identity, SocketAddr))` - Received identity and sender address
    /// * `Err(Error)` - Failed to receive or parse packet
    pub async fn listen(&self) -> Result<(Identity, SocketAddr)> {
        let mut buf = vec![0u8; RECV_BUFFER_SIZE];

        let (len, addr) = self
            .socket
            .recv_from(&mut buf)
            .await
            .map_err(|e| Error::DiscoveryError(format!("Failed to receive packet: {}", e)))?;

        // Android udpPacketReceived (LanLinkProvider.java:215-218): discard
        // packets from non-local (non-private) IPs.
        if !crate::protocol::is_private_address(&addr.ip()) {
            return Err(Error::DiscoveryError(format!(
                "Discarding UDP packet from a non-local IP: {}",
                addr.ip()
            )));
        }

        debug!(
            bytes = len,
            from = %addr,
            event = "packet_received",
            "Received UDP packet"
        );

        // Deserialize packet
        let packet = PacketSerializer::deserialize(&buf[..len])?;

        // Verify it's an identity packet
        if !packet.is_identity() {
            return Err(Error::InvalidPacket(format!(
                "Expected identity packet, got {}",
                packet.packet_type
            )));
        }

        // Parse identity
        let identity = Identity::from_packet(packet)?;

        // Android udpPacketReceived (LanLinkProvider.java:236-240): an
        // identity whose tcpPort falls outside the KDE Connect range is
        // silently dropped.
        let tcp_port = identity.tcp_port.unwrap_or(MIN_PORT);
        if !(MIN_PORT..=MAX_PORT).contains(&tcp_port) {
            return Err(Error::DiscoveryError(
                "TCP port outside of kdeconnect's range".to_string(),
            ));
        }

        // Ignore our own broadcasts
        if identity.device_id == self.identity.device_id {
            debug!(
                device_id = %identity.device_id,
                event = "ignored_own_broadcast",
                "Ignored own broadcast"
            );
            return Err(Error::DiscoveryError("ignored_own".to_string()));
        }

        if crate::protocol::is_split_brain(
            &addr.ip(),
            &self.identity.device_id,
            &identity.device_id,
        ) {
            warn!(
                device_id = %identity.device_id,
                device_name = %identity.device_name,
                from = %addr,
                event = "split_brain_suspected",
                "Another KDE Connect implementation is announcing from THIS host: \
                 two daemons will compete for the same paired phones"
            );
        }

        info!(
            device_id = %identity.device_id,
            device_name = %identity.device_name,
            device_type = %identity.device_type,
            from = %addr,
            event = "device_discovered",
            "Discovered device"
        );

        Ok((identity, addr))
    }

    /// Start listening for devices
    ///
    /// Continuously listens for identity packets and calls the callback
    /// for each discovered device.
    ///
    /// # Arguments
    /// * `callback` - Function to call when a device is discovered
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use rust_connect::protocol::discovery::DiscoveryService;
    /// # use rust_connect::protocol::types::{Identity, DEFAULT_UDP_PORT};
    /// # use rust_connect::device::types::DeviceType;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let identity = Identity::new(
    /// #     "my-device-id".to_string(),
    /// #     "My Device".to_string(),
    /// #     DeviceType::Desktop,
    /// #     vec![],
    /// #     vec![],
    /// # );
    /// # let service = DiscoveryService::new(identity, DEFAULT_UDP_PORT).await?;
    /// // Start listening in background
    /// tokio::spawn(async move {
    ///     service.start_listening(|identity, addr| {
    ///         println!("Discovered: {} at {}", identity.device_name, addr);
    ///     }, tokio_util::sync::CancellationToken::new()).await;
    /// });
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_listening<F>(&self, mut callback: F, shutdown: CancellationToken)
    where
        F: FnMut(Identity, SocketAddr),
    {
        info!(event = "listening_started", "Started listening for devices");

        loop {
            // A4 (2026-09-02 audit): a bare loop here never ended, and every
            // daemon stop paid a 5 s join timeout for it.
            let result = tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(event = "listening_stopped", "Discovery listener stopped on shutdown");
                    return;
                }
                result = self.listen() => result,
            };
            match result {
                Ok((identity, addr)) => {
                    callback(identity, addr);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("ignored_own") {
                        continue;
                    }
                    warn!(
                        error = %e,
                        event = "listen_error",
                        "Error while listening"
                    );
                }
            }
        }
    }

    /// Get this device's identity
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::DeviceType;
    use tokio::time::Duration;

    fn create_test_identity(name: &str) -> Identity {
        Identity::new(
            Identity::generate_device_id(),
            name.to_string(),
            DeviceType::Desktop,
            vec!["kdeconnect.ping".to_string()],
            vec!["kdeconnect.ping".to_string()],
        )
    }

    async fn create_test_service(name: &str) -> Result<DiscoveryService> {
        let identity = create_test_identity(name);

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let socket = UdpSocket::bind(addr)
            .await
            .map_err(|e| Error::DiscoveryError(format!("Failed to bind UDP socket: {}", e)))?;
        socket
            .set_broadcast(true)
            .map_err(|e| Error::DiscoveryError(format!("Failed to enable broadcast: {}", e)))?;

        let broadcast_addr = socket.local_addr().map_err(|e| {
            Error::DiscoveryError(format!("Failed to read test socket address: {}", e))
        })?;

        Ok(DiscoveryService {
            socket,
            identity,
            broadcast_addr,
        })
    }

    #[tokio::test]
    async fn test_discovery_service_creation() {
        let service = create_test_service("Test Device").await;
        assert!(service.is_ok());
    }

    /// Gap 4 (parity-checklist.md § Robustness, vk #997): android sets
    /// SO_RCVBUF to 512 KiB (LanLinkProvider.java:69) so its receive
    /// queue survives a burst of near-simultaneous broadcasts; rust used
    /// to rely on the OS default (`net.core.rmem_default`, ~208 KiB on a
    /// typical Linux host — below android's target). Deterministic:
    /// wraps the already-constructed socket with `socket2::SockRef`
    /// (works on any `AsFd` type, no ownership needed) and reads back
    /// SO_RCVBUF via `getsockopt`, so this doesn't depend on burst timing
    /// or kernel-specific queue-drop behavior the way a live-burst test
    /// would.
    #[tokio::test]
    async fn test_recv_buffer_size_matches_android_target() {
        let identity = create_test_identity("RcvBuf Test");
        let port = find_unused_port().await;
        let service = DiscoveryService::new(identity, port)
            .await
            .expect("DiscoveryService::new must succeed");

        let actual = socket2::SockRef::from(&service.socket)
            .recv_buffer_size()
            .expect("getsockopt(SO_RCVBUF) must succeed");
        // Linux clamps SO_RCVBUF requests to net.core.rmem_max (and
        // getsockopt reports double the clamped value — socket(7)), so on
        // a host with rmem_max < 512 KiB the full target is unreachable
        // no matter what we request. Assert against the honest bound for
        // THIS host: the requested size, or the host ceiling if that is
        // lower (PR #12 review). On an unclamped host this is exactly the
        // 512 KiB assertion; on a clamped host it still proves our
        // request went through rather than silently riding rmem_default.
        let host_ceiling = std::fs::read_to_string("/proc/sys/net/core/rmem_max")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(RECV_BUFFER_SIZE);
        let expected = RECV_BUFFER_SIZE.min(host_ceiling);
        assert!(
            actual >= expected,
            "SO_RCVBUF must reach min(requested {RECV_BUFFER_SIZE}, rmem_max \
             {host_ceiling}) = {expected} bytes (android's 512 KiB target, \
             LanLinkProvider.java:69), got {actual}"
        );
    }

    /// Gap 4's named validation: an oversized live identity injection.
    ///
    /// IMPORTANT finding, verified empirically this session (a Python
    /// `socket.sendto()` past 65507 bytes on loopback fails immediately
    /// with `EMSGSIZE`, confirmed byte-exact at the boundary): IPv4 caps
    /// a single UDP datagram's payload at 65507 bytes (65535 max IP
    /// total length - 20 byte minimum IP header - 8 byte UDP header),
    /// full stop, regardless of ANY receive-buffer setting. The OLD 64
    /// KiB (65536-byte) read buffer was therefore already bigger than
    /// the largest datagram IPv4 can ever deliver — it could not
    /// actually truncate any real identity packet, and this test would
    /// pass on the pre-fix buffer size exactly as it does post-fix (NOT
    /// red-before-green for the byte-count itself; see gap 4 in
    /// plans/task-2.1-report.md for the full finding). What this test
    /// DOES prove, faithfully: the largest datagram IPv4 UDP can ever
    /// carry — the real practical worst case, not a synthetic threshold
    /// — round-trips correctly end-to-end over a live socket, through
    /// the actual production `DiscoveryService::new` construction path
    /// (SO_RCVBUF included), with a real (if synthetic) huge capability
    /// list.
    #[tokio::test]
    async fn test_receives_largest_possible_udp_identity_with_huge_capability_list() {
        const IPV4_MAX_UDP_PAYLOAD: usize = 65_507;

        let listener_identity = create_test_identity("Listener");
        let listener_port = find_unused_port().await;
        let service = DiscoveryService::new(listener_identity, listener_port)
            .await
            .expect("DiscoveryService::new must succeed");
        let listener_addr = service
            .socket
            .local_addr()
            .expect("Value expected to be present");

        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");

        // Pad the capability list until the serialized packet sits as
        // close to the 65507-byte IPv4 ceiling as comfortably achievable.
        // Note this is LESS than the old 65536-byte (64 KiB) buffer, not
        // more — see this test's doc comment above.
        let mut caps = Vec::new();
        let mut sender_identity = create_test_identity("Big Capability List");
        loop {
            caps.push(format!(
                "kdeconnect.synthetic.capability.number.{}",
                caps.len()
            ));
            let candidate = Identity::new(
                sender_identity.device_id.clone(),
                sender_identity.device_name.clone(),
                DeviceType::Desktop,
                caps.clone(),
                vec!["kdeconnect.ping".to_string()],
            );
            let packet_len = crate::protocol::packet::PacketSerializer::serialize(
                &candidate.to_packet().expect("identity to_packet"),
            )
            .expect("serialize")
            .len();
            sender_identity = candidate;
            if packet_len > 65_400 {
                break;
            }
        }

        let wire_bytes = crate::protocol::packet::PacketSerializer::serialize(
            &sender_identity.to_packet().expect("identity to_packet"),
        )
        .expect("serialize");
        // Confirms the loop above actually got close to the ceiling —
        // NOT ">64 KiB (65536 bytes)": that threshold is unreachable by
        // construction, since IPV4_MAX_UDP_PAYLOAD (65507) is itself
        // smaller than 65536. This is the empirical finding stated in
        // this test's doc comment, made mechanical: an assertion of
        // "> 65536" here would never be satisfiable for any real IPv4
        // UDP datagram, on any host.
        assert!(
            wire_bytes.len() > 65_000,
            "test identity must sit close to the IPv4 ceiling to be a meaningful \
             stress case; got {} bytes",
            wire_bytes.len()
        );
        assert!(
            wire_bytes.len() <= IPV4_MAX_UDP_PAYLOAD,
            "test identity must stay within IPv4's own UDP payload ceiling \
             ({IPV4_MAX_UDP_PAYLOAD} bytes); got {} bytes",
            wire_bytes.len()
        );

        sender
            .send_to(&wire_bytes, listener_addr)
            .await
            .expect("send_to must succeed for a datagram under the IPv4 ceiling");

        let (received, _addr) = tokio::time::timeout(Duration::from_secs(2), service.listen())
            .await
            .expect("listen must not time out on a maximal-size identity")
            .expect("listen must accept the oversized identity, not truncate or reject it");

        assert_eq!(received.device_name, "Big Capability List");
        assert_eq!(received.incoming_capabilities, caps);
    }

    #[tokio::test]
    async fn test_broadcast() {
        let service = create_test_service("Test Device")
            .await
            .expect("Value expected to be present");

        // Should not error
        let result = service.broadcast().await;
        assert!(result.is_ok());
    }

    /// Gap 6 (parity-checklist.md § Discovery, vk #998 Task 2.3): the
    /// EMSGSIZE errno predicate, tested directly against constructed
    /// `io::Error`s rather than a live send — deterministic and portable,
    /// no dependency on any host actually hitting the condition.
    #[test]
    fn test_is_message_too_large_matches_linux_and_macos_emsgsize() {
        assert!(is_message_too_large(
            &std::io::Error::from_raw_os_error(90) // Linux EMSGSIZE
        ));
        assert!(is_message_too_large(
            &std::io::Error::from_raw_os_error(40) // macOS/FreeBSD EMSGSIZE
        ));
    }

    /// Non-EMSGSIZE errors must NOT trigger the retry — one send, the
    /// original error surfaced unchanged, exactly as before this gap.
    #[test]
    fn test_is_message_too_large_rejects_other_errors() {
        assert!(!is_message_too_large(&std::io::Error::from_raw_os_error(
            13 // EACCES
        )));
        assert!(!is_message_too_large(&std::io::Error::from_raw_os_error(
            111 // ECONNREFUSED
        )));
        assert!(!is_message_too_large(&std::io::Error::other(
            "generic error with no errno"
        )));
    }

    /// Gap 6 behavioral: a REAL EMSGSIZE, not a mock. IPv4 caps a single
    /// UDP datagram's payload at 65507 bytes full stop (verified
    /// empirically in Task 2.1 — see RECV_BUFFER_SIZE's doc comment and
    /// `test_receives_largest_possible_udp_identity_with_huge_capability_list`
    /// above), regardless of destination or platform — so an identity
    /// built past that ceiling fails `send_to` with a genuine EMSGSIZE on
    /// this host exactly as it would on outpost's BSD kernel, no injection
    /// or mock needed. The retry must fire, strip both capability lists,
    /// and land exactly one (smaller) datagram at the broadcast address —
    /// the oversized send never left the socket, so the capture socket
    /// only ever sees the successful retry.
    #[tokio::test]
    async fn test_broadcast_retries_with_emptied_capabilities_on_oversized_identity() {
        const IPV4_MAX_UDP_PAYLOAD: usize = 65_507;

        let mut caps = Vec::new();
        let mut identity = create_test_identity("Oversized Broadcaster");
        loop {
            caps.push(format!(
                "kdeconnect.synthetic.capability.number.{}",
                caps.len()
            ));
            let candidate = Identity::new(
                identity.device_id.clone(),
                identity.device_name.clone(),
                DeviceType::Desktop,
                caps.clone(),
                caps.clone(),
            );
            let packet_len = crate::protocol::packet::PacketSerializer::serialize(
                &candidate.to_packet().expect("identity to_packet"),
            )
            .expect("serialize")
            .len();
            identity = candidate;
            if packet_len > IPV4_MAX_UDP_PAYLOAD {
                break;
            }
        }
        assert!(
            !identity.incoming_capabilities.is_empty(),
            "sanity: the oversized identity must actually carry capabilities to strip"
        );

        let mut service = create_test_service("placeholder")
            .await
            .expect("Value expected to be present");
        service.identity = identity;

        let capture = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        service.broadcast_addr = capture.local_addr().expect("Value expected to be present");

        service
            .broadcast()
            .await
            .expect("broadcast must retry and succeed, not surface the oversized send's error");

        let mut buf = vec![0u8; RECV_BUFFER_SIZE];
        let (len, _addr) =
            tokio::time::timeout(Duration::from_secs(2), capture.recv_from(&mut buf))
                .await
                .expect("the retried, smaller datagram must arrive")
                .expect("recv_from must succeed");

        let received = crate::protocol::packet::PacketSerializer::deserialize(&buf[..len])
            .expect("retried datagram must deserialize");
        let received_identity =
            Identity::from_packet(received).expect("retried datagram must be a valid identity");
        assert!(
            received_identity.incoming_capabilities.is_empty(),
            "retry must strip incomingCapabilities"
        );
        assert!(
            received_identity.outgoing_capabilities.is_empty(),
            "retry must strip outgoingCapabilities"
        );
        assert_eq!(received_identity.device_name, "Oversized Broadcaster");

        // Exactly one datagram — the oversized send never left the
        // socket, so a regression that fires the retry twice (or the
        // original oversized identity somehow shrinking enough to also
        // go out) would show up here as a second arrival.
        let second =
            tokio::time::timeout(Duration::from_millis(200), capture.recv_from(&mut buf)).await;
        assert!(
            second.is_err(),
            "exactly one datagram must land — the oversized send must never actually go out"
        );
    }

    /// Regression pin: a normally-sized identity must broadcast once, with
    /// its capabilities intact — the retry path must never fire when
    /// nothing is oversized.
    #[tokio::test]
    async fn test_broadcast_normal_identity_unaffected() {
        let service = create_test_service("Normal Broadcaster")
            .await
            .expect("Value expected to be present");

        let capture = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let mut service = service;
        service.broadcast_addr = capture.local_addr().expect("Value expected to be present");

        service.broadcast().await.expect("broadcast must succeed");

        let mut buf = vec![0u8; RECV_BUFFER_SIZE];
        let (len, _addr) =
            tokio::time::timeout(Duration::from_secs(2), capture.recv_from(&mut buf))
                .await
                .expect("the datagram must arrive")
                .expect("recv_from must succeed");
        let received = crate::protocol::packet::PacketSerializer::deserialize(&buf[..len])
            .expect("must deserialize");
        let received_identity = Identity::from_packet(received).expect("must be a valid identity");
        assert_eq!(
            received_identity.incoming_capabilities,
            vec!["kdeconnect.ping".to_string()],
            "an un-oversized identity's capabilities must arrive intact"
        );

        let second =
            tokio::time::timeout(Duration::from_millis(200), capture.recv_from(&mut buf)).await;
        assert!(second.is_err(), "exactly one datagram, no spurious retry");
    }

    #[tokio::test]
    async fn test_identity_getter() {
        let service = create_test_service("Test Device")
            .await
            .expect("Value expected to be present");

        assert_eq!(service.identity().device_name, "Test Device");
    }

    // Integration test: Two services discovering each other
    #[tokio::test]
    async fn test_mutual_discovery() {
        let mut service1 = create_test_service("Device 1")
            .await
            .expect("Value expected to be present");
        let service2 = create_test_service("Device 2")
            .await
            .expect("Value expected to be present");

        service1.broadcast_addr = service2
            .socket
            .local_addr()
            .expect("test socket must have an address");

        // Service 1 broadcasts
        service1
            .broadcast()
            .await
            .expect("Value expected to be present");

        // Service 2 should receive it
        let result = tokio::time::timeout(Duration::from_secs(2), service2.listen()).await;

        // This might timeout in CI environments, so we just check it doesn't panic
        match result {
            Ok(Ok((identity, _addr))) => {
                assert_eq!(identity.device_name, "Device 1");
            }
            Ok(Err(_)) | Err(_) => {
                // Timeout or error is acceptable in test environment
            }
        }
    }

    #[tokio::test]
    async fn test_listen_ignores_own_broadcast() {
        let service = create_test_service("Self Device")
            .await
            .expect("Value expected to be present");
        service
            .broadcast()
            .await
            .expect("Value expected to be present");

        let result = tokio::time::timeout(Duration::from_secs(2), service.listen()).await;
        match result {
            Ok(Err(e)) => {
                assert!(
                    e.to_string().contains("ignored_own"),
                    "Expected own broadcast to be filtered, got: {}",
                    e
                );
            }
            Ok(Ok((identity, _))) => {
                panic!(
                    "Should not have received own broadcast, got identity from: {}",
                    identity.device_name
                );
            }
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn test_new_binds_and_broadcasts_on_configured_port() {
        // settings.udp_port (and the --port CLI flag) flow through here;
        // the bind socket AND broadcast address must use the configured
        // port, not the protocol-default constant.
        let identity = create_test_identity("Port Test");
        let configured_port = find_unused_port().await;

        let service = DiscoveryService::new(identity, configured_port)
            .await
            .expect("Value expected to be present");

        assert_eq!(
            service
                .socket
                .local_addr()
                .expect("Value expected to be present")
                .port(),
            configured_port,
            "DiscoveryService must bind to the configured UDP port"
        );
        assert_eq!(
            service.broadcast_addr.port(),
            configured_port,
            "DiscoveryService must broadcast on the configured UDP port"
        );
    }

    #[tokio::test]
    async fn test_reuseaddr_allows_rapid_rebind() {
        let _identity = create_test_identity("Rebind Test");
        let port = find_unused_port().await;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);

        let socket1 = Socket::new(Domain::IPV4, SockType::DGRAM, Some(Protocol::UDP))
            .expect("Value expected to be present");
        socket1
            .set_reuse_address(true)
            .expect("Value expected to be present");
        socket1
            .set_nonblocking(true)
            .expect("Value expected to be present");
        socket1
            .bind(&addr.into())
            .expect("Value expected to be present");
        drop(socket1);

        let socket2 = Socket::new(Domain::IPV4, SockType::DGRAM, Some(Protocol::UDP))
            .expect("Value expected to be present");
        socket2
            .set_reuse_address(true)
            .expect("Value expected to be present");
        socket2
            .set_nonblocking(true)
            .expect("Value expected to be present");
        let result = socket2.bind(&addr.into());
        assert!(
            result.is_ok(),
            "Should be able to rebind to same port with SO_REUSEADDR"
        );
    }

    async fn find_unused_port() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        socket
            .local_addr()
            .expect("Value expected to be present")
            .port()
    }

    #[tokio::test]
    async fn test_listen_rejects_tcp_port_outside_kdeconnect_range() {
        // Android udpPacketReceived (LanLinkProvider.java:236-240): an
        // identity whose tcpPort is outside 1716-1764 is dropped.
        let service = create_test_service("Listener")
            .await
            .expect("Value expected to be present");
        let listener_addr = service
            .socket
            .local_addr()
            .expect("Value expected to be present");

        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let mut identity = create_test_identity("Bad Port");
        identity.tcp_port = Some(2000);
        let packet = crate::protocol::packet::PacketSerializer::serialize(
            &identity.to_packet().expect("Value expected to be present"),
        )
        .expect("Value expected to be present");
        sender
            .send_to(&packet, listener_addr)
            .await
            .expect("Value expected to be present");

        let result = tokio::time::timeout(Duration::from_secs(2), service.listen()).await;
        match result {
            Ok(Err(e)) => {
                assert!(
                    e.to_string().contains("TCP port outside"),
                    "Expected tcpPort range rejection, got: {}",
                    e
                );
            }
            Ok(Ok((identity, _))) => {
                panic!(
                    "Should not have accepted out-of-range tcpPort, got: {}",
                    identity.device_name
                );
            }
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn test_listen_rejects_malformed_json() {
        let service = create_test_service("Listener")
            .await
            .expect("Value expected to be present");
        let listener_addr = service
            .socket
            .local_addr()
            .expect("Value expected to be present");

        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        sender
            .send_to(b"this is not json at all", listener_addr)
            .await
            .expect("Value expected to be present");

        let result = tokio::time::timeout(Duration::from_secs(2), service.listen()).await;
        match result {
            Ok(Err(_)) => {}
            Ok(Ok((identity, _))) => {
                panic!(
                    "Should not have accepted malformed JSON, got: {}",
                    identity.device_name
                );
            }
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn test_listen_rejects_truncated_json() {
        let service = create_test_service("Listener")
            .await
            .expect("Value expected to be present");
        let listener_addr = service
            .socket
            .local_addr()
            .expect("Value expected to be present");

        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        sender
            .send_to(
                b"{\"id\":1,\"type\":\"kdeconnect.identity\",\"bo",
                listener_addr,
            )
            .await
            .expect("Value expected to be present");

        let result = tokio::time::timeout(Duration::from_secs(2), service.listen()).await;
        match result {
            Ok(Err(_)) => {}
            Ok(Ok((identity, _))) => {
                panic!(
                    "Should not have accepted truncated JSON, got: {}",
                    identity.device_name
                );
            }
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn test_listen_rejects_empty_packet() {
        let service = create_test_service("Listener")
            .await
            .expect("Value expected to be present");
        let listener_addr = service
            .socket
            .local_addr()
            .expect("Value expected to be present");

        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        sender
            .send_to(b"", listener_addr)
            .await
            .expect("Value expected to be present");

        let result = tokio::time::timeout(Duration::from_secs(2), service.listen()).await;
        match result {
            Ok(Err(_)) => {}
            Ok(Ok((identity, _))) => {
                panic!(
                    "Should not have accepted empty packet, got: {}",
                    identity.device_name
                );
            }
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn test_listen_rejects_invalid_utf8() {
        let service = create_test_service("Listener")
            .await
            .expect("Value expected to be present");
        let listener_addr = service
            .socket
            .local_addr()
            .expect("Value expected to be present");

        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        sender
            .send_to(&[0xFF, 0xFE, 0x80, 0x81], listener_addr)
            .await
            .expect("Value expected to be present");

        let result = tokio::time::timeout(Duration::from_secs(2), service.listen()).await;
        match result {
            Ok(Err(_)) => {}
            Ok(Ok((identity, _))) => {
                panic!(
                    "Should not have accepted invalid UTF-8, got: {}",
                    identity.device_name
                );
            }
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn test_listen_survives_error_and_continues() {
        let service = create_test_service("Listener")
            .await
            .expect("Value expected to be present");
        let listener_addr = service
            .socket
            .local_addr()
            .expect("Value expected to be present");

        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        sender
            .send_to(b"garbage data", listener_addr)
            .await
            .expect("Value expected to be present");

        let _ = tokio::time::timeout(Duration::from_secs(1), service.listen()).await;

        let valid_identity = create_test_identity("Valid Sender");
        let valid_packet = crate::protocol::packet::PacketSerializer::serialize(
            &valid_identity
                .to_packet()
                .expect("Value expected to be present"),
        )
        .expect("Value expected to be present");
        sender
            .send_to(&valid_packet, listener_addr)
            .await
            .expect("Value expected to be present");

        let result = tokio::time::timeout(Duration::from_secs(2), service.listen()).await;
        if let Ok(Ok((identity, _))) = result {
            assert_eq!(identity.device_name, "Valid Sender");
        }
    }
}

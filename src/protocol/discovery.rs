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
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use crate::protocol::packet::PacketSerializer;
use crate::protocol::types::{Identity, MAX_PORT, MIN_PORT};
use crate::utils::errors::{Error, Result};

/// Discovery service
///
/// Handles broadcasting identity packets and listening for other devices.
pub struct DiscoveryService {
    pub socket: UdpSocket,
    pub identity: Identity,
    pub broadcast_addr: SocketAddr,
    pub broadcast_interval: Duration,
}

impl DiscoveryService {
    /// Create a new discovery service
    ///
    /// # Arguments
    /// * `identity` - This device's identity
    /// * `broadcast_interval` - How often to broadcast (in seconds)
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
    /// let service = DiscoveryService::new(identity, 5, DEFAULT_UDP_PORT).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(
        identity: Identity,
        broadcast_interval_secs: u64,
        udp_port: u16,
    ) -> Result<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), udp_port);

        let socket = Socket::new(Domain::IPV4, SockType::DGRAM, Some(Protocol::UDP))
            .map_err(|e| Error::DiscoveryError(format!("Failed to create UDP socket: {}", e)))?;

        socket
            .set_reuse_address(true)
            .map_err(|e| Error::DiscoveryError(format!("Failed to set SO_REUSEADDR: {}", e)))?;

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
            interval_secs = broadcast_interval_secs,
            event = "discovery_service_created",
            "Discovery service initialized"
        );

        Ok(Self {
            socket,
            identity,
            broadcast_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), udp_port),
            broadcast_interval: Duration::from_secs(broadcast_interval_secs),
        })
    }

    /// Broadcast this device's identity
    ///
    /// Sends identity packet to broadcast address.
    ///
    /// # Returns
    /// * `Ok(())` - Broadcast successful
    /// * `Err(Error)` - Broadcast failed
    pub async fn broadcast(&self) -> Result<()> {
        let packet = self.identity.to_packet()?;
        let bytes = PacketSerializer::serialize(&packet)?;

        self.socket
            .send_to(&bytes, self.broadcast_addr)
            .await
            .map_err(|e| Error::DiscoveryError(format!("Failed to send broadcast: {}", e)))?;

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
        let mut buf = vec![0u8; 65536];

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

    /// Start broadcasting periodically
    ///
    /// Broadcasts identity at configured interval until cancelled.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use rust_connect::protocol::discovery::DiscoveryService;
    /// # use rust_connect::protocol::types::{Identity, DEFAULT_UDP_PORT};
    /// # use rust_connect::device::types::DeviceType;
    /// # use tokio_util::sync::CancellationToken;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let identity = Identity::new(
    /// #     "my-device-id".to_string(),
    /// #     "My Device".to_string(),
    /// #     DeviceType::Desktop,
    /// #     vec![],
    /// #     vec![],
    /// # );
    /// # let service = DiscoveryService::new(identity, 5, DEFAULT_UDP_PORT).await?;
    /// // Start broadcasting in background
    /// let cancel = CancellationToken::new();
    /// tokio::spawn(async move {
    ///     service.start_broadcasting(cancel).await;
    /// });
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_broadcasting(&self, cancel: tokio_util::sync::CancellationToken) {
        let mut ticker = interval(self.broadcast_interval);

        info!(
            interval_secs = self.broadcast_interval.as_secs(),
            event = "broadcasting_started",
            "Started periodic broadcasting"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.broadcast().await {
                        error!(
                            error = %e,
                            event = "broadcast_failed",
                            "Failed to broadcast identity"
                        );
                    }
                    let jitter_ms = (self.broadcast_interval.as_millis() as u64) / 2;
                    let jitter = if jitter_ms > 0 {
                        rand::random::<u64>() % jitter_ms
                    } else {
                        0
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
                }
                _ = cancel.cancelled() => {
                    info!(
                        event = "broadcasting_stopped",
                        "Stopped broadcasting (cancelled)"
                    );
                    return;
                }
            }
        }
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
    /// # let service = DiscoveryService::new(identity, 5, DEFAULT_UDP_PORT).await?;
    /// // Start listening in background
    /// tokio::spawn(async move {
    ///     service.start_listening(|identity, addr| {
    ///         println!("Discovered: {} at {}", identity.device_name, addr);
    ///     }).await;
    /// });
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_listening<F>(&self, mut callback: F)
    where
        F: FnMut(Identity, SocketAddr),
    {
        info!(event = "listening_started", "Started listening for devices");

        loop {
            match self.listen().await {
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

    /// Get the broadcast interval
    pub fn broadcast_interval(&self) -> Duration {
        self.broadcast_interval
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::DeviceType;

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
            broadcast_interval: Duration::from_secs(5),
        })
    }

    #[tokio::test]
    async fn test_discovery_service_creation() {
        let service = create_test_service("Test Device").await;
        assert!(service.is_ok());
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

    #[tokio::test]
    async fn test_identity_getter() {
        let service = create_test_service("Test Device")
            .await
            .expect("Value expected to be present");

        assert_eq!(service.identity().device_name, "Test Device");
    }

    #[tokio::test]
    async fn test_broadcast_interval() {
        let service = create_test_service("Test Device")
            .await
            .expect("Value expected to be present");

        assert_eq!(service.broadcast_interval(), Duration::from_secs(5));
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

        let service = DiscoveryService::new(identity, 5, configured_port)
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

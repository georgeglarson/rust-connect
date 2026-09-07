//! Unicast our identity to one peer over UDP.
//!
//! The KDE Connect discovery handshake is symmetric: whoever receives an
//! identity over UDP dials the sender's `tcpPort` (kdeconnect-kde
//! `lanlinkprovider.cpp:316-339`; Android `LanLinkProvider.udpPacketReceived`
//! draws no distinction between a broadcast and a unicast). Two callers
//! want exactly that reaction from one specific peer:
//!
//! - the reverse-connection fallback after a failed outbound dial
//!   (`connection/outbound.rs`; kde `connectError`, `lanlinkprovider.cpp:343-354`), and
//! - an mDNS resolve (`service_manager::on_mdns_device_resolved`), which is
//!   how both references answer a resolve (`mdnshdiscovery.cpp:36`, Android
//!   `MdnsDiscovery.onServiceResolved`) — vk #1101.
//!
//! Callers own their log vocabulary: this function reports which stage
//! failed and logs nothing itself.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::protocol::packet::PacketSerializer;
use crate::protocol::types::Identity;
use crate::utils::errors::Error;

/// Which stage of a unicast failed. Every variant is best-effort from the
/// caller's point of view: nothing here retries.
#[derive(Debug)]
pub(crate) enum UnicastError {
    Build(Error),
    Serialize(Error),
    Bind(std::io::Error),
    Send(std::io::Error),
}

impl std::fmt::Display for UnicastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(e) => write!(f, "building the identity packet: {e}"),
            Self::Serialize(e) => write!(f, "serializing the identity packet: {e}"),
            Self::Bind(e) => write!(f, "binding a UDP socket: {e}"),
            Self::Send(e) => write!(f, "sending the datagram: {e}"),
        }
    }
}

/// Send `our_identity` as a UDP identity packet (the broadcast shape, with
/// `tcpPort` present — Android silently drops an identity whose tcpPort is
/// outside 1716-1764, live-proven 2026-07-29) to `peer_ip:udp_port`.
///
/// Binds an ephemeral socket in the peer's address family: a `0.0.0.0`
/// socket cannot `send_to` an IPv6 target (bot round, PR #14). Returns the
/// target on success so the caller can log it.
pub(crate) async fn unicast_identity(
    our_identity: &Identity,
    peer_ip: IpAddr,
    udp_port: u16,
) -> std::result::Result<SocketAddr, UnicastError> {
    let packet = our_identity.to_packet().map_err(UnicastError::Build)?;
    let bytes = PacketSerializer::serialize(&packet).map_err(UnicastError::Serialize)?;
    let bind_addr: SocketAddr = match peer_ip {
        IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = tokio::net::UdpSocket::bind(bind_addr)
        .await
        .map_err(UnicastError::Bind)?;
    let target = SocketAddr::new(peer_ip, udp_port);
    socket
        .send_to(&bytes, target)
        .await
        .map_err(UnicastError::Send)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::DeviceType;

    fn identity() -> Identity {
        let mut id = Identity::new(
            "udp-unicast-test-aaaaaaaaaaaaaaaaa".to_string(),
            "Unicast Test".to_string(),
            DeviceType::Desktop,
            vec![],
            vec![],
        );
        id.tcp_port = Some(1716);
        id
    }

    #[tokio::test]
    async fn test_unicast_identity_delivers_a_udp_shaped_identity() {
        let capture = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind capture");
        let port = capture.local_addr().expect("local_addr").port();

        let target = unicast_identity(&identity(), Ipv4Addr::LOCALHOST.into(), port)
            .await
            .expect("a loopback unicast must succeed");
        assert_eq!(target, SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port));

        let mut buf = vec![0u8; 65536];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            capture.recv_from(&mut buf),
        )
        .await
        .expect("the datagram must arrive")
        .expect("recv_from");
        let packet = PacketSerializer::deserialize(&buf[..len]).expect("must deserialize");
        assert!(packet.is_identity(), "must be an identity packet");
        let received = Identity::from_packet(packet).expect("must be a valid identity");
        assert_eq!(received.device_id, identity().device_id);
        assert_eq!(
            received.tcp_port,
            Some(1716),
            "the UDP identity shape carries tcpPort so the peer can dial us"
        );
    }

    /// The UDP identity shape carries the capability lists (`to_packet`,
    /// not `to_tcp_packet`); the peer's capability gate reads them
    /// (`Device::apply_capability_update`). Pins that the primitive does
    /// not strip them *(cypher, mimo-v25-pro C2)*.
    #[tokio::test]
    async fn test_unicast_identity_carries_capabilities() {
        let capture = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind capture");
        let port = capture.local_addr().expect("local_addr").port();
        let mut id = identity();
        id.incoming_capabilities = vec!["kdeconnect.ping".to_string()];
        id.outgoing_capabilities = vec!["kdeconnect.battery".to_string()];

        unicast_identity(&id, Ipv4Addr::LOCALHOST.into(), port)
            .await
            .expect("unicast");
        let mut buf = vec![0u8; 65536];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            capture.recv_from(&mut buf),
        )
        .await
        .expect("datagram")
        .expect("recv_from");
        let received =
            Identity::from_packet(PacketSerializer::deserialize(&buf[..len]).expect("deserialize"))
                .expect("identity");
        assert_eq!(
            received.incoming_capabilities,
            vec!["kdeconnect.ping".to_string()]
        );
        assert_eq!(
            received.outgoing_capabilities,
            vec!["kdeconnect.battery".to_string()]
        );
    }

    /// The bind follows the peer's address family (bot round, PR #14).
    /// `::1` carries an implicit scope; a link-local `fe80::` target needs
    /// a scope id this primitive does not carry, which is why
    /// `resolved_to_peer` never hands one over (Task 2) *(cypher)*.
    #[tokio::test]
    async fn test_unicast_identity_reaches_an_ipv6_peer() {
        let capture = match tokio::net::UdpSocket::bind("[::1]:0").await {
            Ok(s) => s,
            // Host without IPv6 loopback (some CI containers): the v4 leg
            // above covers the shape; nothing to test here.
            Err(_) => return,
        };
        let port = capture.local_addr().expect("local_addr").port();
        unicast_identity(&identity(), Ipv6Addr::LOCALHOST.into(), port)
            .await
            .expect("a v6 loopback unicast must succeed");
        let mut buf = vec![0u8; 65536];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            capture.recv_from(&mut buf),
        )
        .await
        .expect("the v6 datagram must arrive")
        .expect("recv_from");
    }
}

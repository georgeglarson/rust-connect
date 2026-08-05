//! Protocol layer for KDE Connect
//!
//! This module implements the KDE Connect protocol with strict SRP:
//! - Discovery: UDP broadcast/listen
//! - Connection: TCP/TLS connections
//! - Pairing: Device pairing logic
//! - Packet: Serialization/deserialization
//! - Router: Packet routing to plugins
//! - Crypto: Certificate management

pub mod connection;
pub mod connection_loop;
pub mod crypto;
pub mod discovery;
pub mod keepalive;
pub mod listener;
pub mod mdns_discovery;
pub mod own_identity;
pub mod packet;
pub mod pairing;
pub mod payload_transfer;
#[cfg(any(test, feature = "test-helpers"))]
pub mod replay;
pub mod router;
pub mod transcript;
pub mod types;

// Re-export commonly used types
pub use connection::ConnectionManager;
pub use crypto::CertificateManager;
pub use discovery::DiscoveryService;
pub use packet::PacketSerializer;
pub use pairing::{PairState, PairingHandler};
pub use router::PacketRouter;
pub use types::{
    ConnectionInfo, Identity, Packet, PairingRequest, DEFAULT_TCP_PORT, DEFAULT_UDP_PORT,
    PROTOCOL_VERSION,
};

/// Android `isPrivateAddress` (NetworkHelper.kt): loopback, site-local,
/// link-local, CGNAT (100.64.0.0/10), or IPv6 ULA (fc00::/7). Android
/// refuses KDE Connect traffic from any other address — it is a LAN
/// protocol (LanLinkProvider.java:138-141, 215-218).
pub fn is_private_address(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                // CGNAT 100.64.0.0/10 — not covered by std's is_private.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 0x40)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                // Link-local fe80::/10.
                || (v6.octets()[0] == 0xFE && (v6.octets()[1] & 0xC0) == 0x80)
                // Unique local fc00::/7.
                || (v6.octets()[0] & 0xFE) == 0xFC
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::is_private_address;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("Value expected to be present")
    }

    #[test]
    fn test_is_private_address_matches_android_rules() {
        // Allowed: loopback, site-local, link-local, CGNAT, ULA.
        for allowed in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
        ] {
            assert!(
                is_private_address(&ip(allowed)),
                "{allowed} must be allowed"
            );
        }
        // Refused: public addresses.
        for refused in [
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "11.0.0.1",
            "2001:4860:4860::8888",
            "fe40::1",
        ] {
            assert!(
                !is_private_address(&ip(refused)),
                "{refused} must be refused"
            );
        }
    }
}

//! Find My Phone plugin
//!
//! Single Responsibility: Send kdeconnect.findmyphone.request packets
//! to make the phone ring at max volume for locating it.
//!
//! Wire shape (upstream-verified): the request packet has an EMPTY body.
//! - kdeconnect-kde sends `NetworkPacket np(PACKET_TYPE_FINDMYPHONE_REQUEST)`
//!   with no body fields (plugins/findmyphone/findmyphoneplugin.cpp:17-21).
//! - GSConnect sends `{ type: 'kdeconnect.findmyphone.request', body: {} }`
//!   (src/service/plugins/findmyphone.js:93-98).
//! - The phone only ever RECEIVES this packet; there is no response packet
//!   (kdeconnect-android FindMyPhonePlugin.java:41 declares the packet type,
//!   getOutgoingPacketTypes() returns an empty array).
//!
//! Capability honesty: we advertise the request as OUTGOING only. We do not
//! implement ringing this machine (no sound/dialog), so we must not list the
//! packet as incoming — matching kdeconnect-kde's own declaration
//! (plugins/findmyphone/kdeconnect_findmyphone.json: OutgoingPacketType =
//! ["kdeconnect.findmyphone.request"], SupportedPacketType = []).

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

pub struct FindMyPhonePlugin;

impl Default for FindMyPhonePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl FindMyPhonePlugin {
    pub fn new() -> Self {
        Self
    }

    /// Build the ring-request packet for a device.
    /// Body is empty per upstream (see module docs for citations).
    pub fn ring_request(&self) -> Packet {
        Packet::new(
            "kdeconnect.findmyphone.request".to_string(),
            serde_json::json!({}),
        )
    }
}

#[async_trait::async_trait]
impl Plugin for FindMyPhonePlugin {
    fn name(&self) -> &str {
        "findmyphone"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec![]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.findmyphone.request".to_string()]
    }

    async fn handle_packet(
        &self,
        _device_id: &str,
        _packet: Packet,
    ) -> Result<Option<Vec<Packet>>> {
        // No incoming packets: the phone never answers a findmyphone request
        // (kdeconnect-android FindMyPhonePlugin.getOutgoingPacketTypes() is empty).
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_findmyphone_plugin_name() {
        let plugin = FindMyPhonePlugin::new();
        assert_eq!(plugin.name(), "findmyphone");
    }

    #[tokio::test]
    async fn test_findmyphone_capabilities() {
        let plugin = FindMyPhonePlugin::new();
        // Incoming must stay empty: we cannot ring this machine, so we must
        // not advertise the ability (kdeconnect-kde declares it outgoing-only
        // too — kdeconnect_findmyphone.json).
        assert!(plugin.incoming_capabilities().is_empty());
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.findmyphone.request".to_string()));
    }

    #[tokio::test]
    async fn test_ring_request_wire_shape() {
        // Upstream: kdeconnect-kde plugins/findmyphone/findmyphoneplugin.cpp:17-21
        // sends the request with NO body fields; GSConnect
        // src/service/plugins/findmyphone.js:93-98 sends body: {}.
        let plugin = FindMyPhonePlugin::new();
        let packet = plugin.ring_request();
        assert_eq!(packet.packet_type, "kdeconnect.findmyphone.request");
        assert_eq!(packet.body, serde_json::json!({}));
    }

    #[tokio::test]
    async fn test_handle_packet_noop() {
        let plugin = FindMyPhonePlugin::new();
        let packet = Packet::new(
            "kdeconnect.findmyphone.request".to_string(),
            serde_json::json!({}),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
    }
}

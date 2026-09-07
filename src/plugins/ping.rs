//! Ping plugin
//!
//! Single Responsibility: Handle kdeconnect.ping packets.

use tracing::info;

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;
use super::tool::{Tool, ToolParameter};

pub struct PingPlugin;

impl Default for PingPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl PingPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Plugin for PingPlugin {
    fn name(&self) -> &str {
        "ping"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.ping".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.ping".to_string()]
    }

    async fn handle_packet(&self, _device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        info!(
            packet_id = packet.id,
            event = "ping_received",
            "Received ping"
        );
        Ok(None)
    }

    fn tools(&self) -> Vec<Tool> {
        vec![Tool {
            name: "ping_device".to_string(),
            description: "Send a ping to a device to check connectivity".to_string(),
            capability: "kdeconnect.ping".to_string(),
            endpoint: "/api/v1/ping".to_string(),
            method: "POST".to_string(),
            parameters: vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            available: true,
        }]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_ping_plugin() {
        let plugin = PingPlugin::new();
        assert_eq!(plugin.name(), "ping");
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.ping".to_string()));
    }

    #[tokio::test]
    async fn test_handle_ping() {
        let plugin = PingPlugin::new();
        let packet = Packet::ping();
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_ok());
    }
}

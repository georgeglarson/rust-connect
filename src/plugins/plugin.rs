//! Plugin trait definition
//!
//! Single Responsibility: Define the interface all plugins must implement.

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

#[async_trait::async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;

    fn incoming_capabilities(&self) -> Vec<String>;

    fn outgoing_capabilities(&self) -> Vec<String>;

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>>;

    fn on_connected(&self, device_id: &str) -> Vec<Packet> {
        let _ = device_id;
        vec![]
    }

    fn on_disconnected(&self, device_id: &str) {
        let _ = device_id;
    }
}

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

    /// Runs on the connection task when a device's link drops. Async so a
    /// plugin with blocking cleanup (sftp's fusermount) can bound it with
    /// `spawn_blocking` + a timeout instead of stalling the disconnect.
    async fn on_disconnected(&self, device_id: &str) {
        let _ = device_id;
    }

    /// Whether the plugin's backend is operational. Plugins without a
    /// separable backend (most of them) report `true`; plugins that
    /// detect a session bus / portal / clipboard backend at runtime
    /// (clipboard, mpris, sendnotifications, pausemusic,
    /// screensaver_inhibit) override to report the live state.
    /// Default `true` keeps existing plugins honest-by-default; the
    /// listing surfaces `false` so /api/v1/tools never advertises a
    /// tool the backend can't actually service.
    fn is_backend_available(&self) -> bool {
        true
    }
}

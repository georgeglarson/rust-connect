//! SFTP plugin
//!
//! Single Responsibility: Handle SFTP (SSH Filesystem) browsing of remote device storage.
//!
//! Protocol:
//! - Outgoing: kdeconnect.sftp.request { "startBrowsing": true }
//! - Incoming: kdeconnect.sftp { ip, port, user, password, path, multiPaths, pathNames }
//!
//! The Android device runs an SFTP server. We request browsing, receive the
//! connection credentials (host, port, user, password, paths), keep them per
//! device, and broadcast them as plugin events so the API layer can expose
//! them to clients. Mounting the filesystem is left to the client.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpConnectionInfo {
    pub ip: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub path: String,
    pub multi_paths: Vec<String>,
    pub path_names: Vec<String>,
}

pub struct SftpPlugin {
    connections: Arc<RwLock<HashMap<String, SftpConnectionInfo>>>,
    plugin_events: Arc<PluginEventBroadcaster>,
}

impl Default for SftpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SftpPlugin {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            plugin_events: Arc::new(PluginEventBroadcaster::new(16, "plugin")),
        }
    }

    #[allow(clippy::expect_used)]
    pub fn get_connection(&self, device_id: &str) -> Option<SftpConnectionInfo> {
        let connections = self.connections.read().unwrap_or_else(|e| e.into_inner());
        connections.get(device_id).cloned()
    }

    pub fn with_events(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            plugin_events,
        }
    }

    pub fn request_sftp(&self, _device_id: &str) -> Packet {
        Packet::new(
            "kdeconnect.sftp.request".to_string(),
            serde_json::json!({
                "startBrowsing": true
            }),
        )
    }

    pub fn set_connection(&self, device_id: &str, info: SftpConnectionInfo) {
        if let Ok(mut connections) = self.connections.write() {
            connections.insert(device_id.to_string(), info);
        }
    }
}

#[async_trait::async_trait]
impl Plugin for SftpPlugin {
    fn name(&self) -> &str {
        "sftp"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.sftp".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.sftp.request".to_string()]
    }

    fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut connections) = self.connections.write() {
            connections.remove(device_id);
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type != "kdeconnect.sftp" {
            return Ok(None);
        }

        if let Some(error_msg) = packet.body.get("errorMessage") {
            info!(
                device_id = %device_id,
                error = %error_msg,
                event = "sftp_error",
                "SFTP error from device"
            );
            return Ok(None);
        }

        let ip = packet
            .body
            .get("ip")
            .and_then(|v| v.as_str())
            .unwrap_or("127.0.0.1")
            .to_string();
        let port = packet
            .body
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(1740) as u16;
        let user = packet
            .body
            .get("user")
            .and_then(|v| v.as_str())
            .unwrap_or("kdeconnect")
            .to_string();
        let password = packet
            .body
            .get("password")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let path = packet
            .body
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("/")
            .to_string();

        let multi_paths: Vec<String> = packet
            .body
            .get("multiPaths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let path_names: Vec<String> = packet
            .body
            .get("pathNames")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let info = SftpConnectionInfo {
            ip,
            port,
            user,
            password,
            path,
            multi_paths,
            path_names,
        };

        info!(
            device_id = %device_id,
            ip = %info.ip,
            port = info.port,
            path = %info.path,
            event = "sftp_connected",
            "SFTP connection info received"
        );

        self.set_connection(device_id, info.clone());

        self.plugin_events.broadcast(PluginEvent::SftpUpdate {
            device_id: device_id.to_string(),
            ip: info.ip,
            port: info.port,
            user: info.user,
            path: info.path,
            available: true,
        });

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_sftp_plugin_name() {
        let plugin = SftpPlugin::new();
        assert_eq!(plugin.name(), "sftp");
    }

    #[tokio::test]
    async fn test_sftp_capabilities() {
        let plugin = SftpPlugin::new();
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.sftp".to_string()));
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.sftp.request".to_string()));
    }

    #[tokio::test]
    async fn test_handle_sftp_packet() {
        let plugin = SftpPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": "192.168.1.100",
                "port": 1740,
                "user": "kdeconnect",
                "password": "secretpassword",
                "path": "/storage/emulated/0",
                "multiPaths": ["/storage/emulated/0", "/storage/emulated/0/DCIM"],
                "pathNames": ["Internal Storage", "Camera pictures"]
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());

        let info = plugin
            .get_connection("device1")
            .expect("Value expected to be present");
        assert_eq!(info.ip, "192.168.1.100");
        assert_eq!(info.port, 1740);
        assert_eq!(info.user, "kdeconnect");
        assert_eq!(info.password, "secretpassword");
        assert_eq!(info.path, "/storage/emulated/0");
        assert_eq!(info.multi_paths.len(), 2);
        assert_eq!(info.path_names.len(), 2);
    }

    #[tokio::test]
    async fn test_handle_sftp_error() {
        let plugin = SftpPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "errorMessage": "No storage locations configured"
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert!(plugin.get_connection("device1").is_none());
    }

    #[tokio::test]
    async fn test_request_sftp_packet() {
        let plugin = SftpPlugin::new();
        let packet = plugin.request_sftp("device1");
        assert_eq!(packet.packet_type, "kdeconnect.sftp.request");
        assert_eq!(
            packet.body.get("startBrowsing"),
            Some(&serde_json::json!(true))
        );
    }

    #[tokio::test]
    async fn test_disconnected_clears_connection() {
        let plugin = SftpPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": "192.168.1.100",
                "port": 1740,
                "user": "kdeconnect",
                "password": "secret",
                "path": "/"
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("Value expected to be present");
        assert!(plugin.get_connection("device1").is_some());

        plugin.on_disconnected("device1");
        assert!(plugin.get_connection("device1").is_none());
    }

    #[tokio::test]
    async fn test_handle_sftp_multi_paths_content() {
        let plugin = SftpPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": "192.168.1.100",
                "port": 1740,
                "user": "kdeconnect",
                "password": "secret",
                "path": "/storage/emulated/0",
                "multiPaths": [
                    "/storage/emulated/0",
                    "/storage/emulated/0/DCIM",
                    "/storage/emulated/0/Pictures"
                ],
                "pathNames": ["Internal Storage", "Camera Roll", "Pictures"]
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("Value expected to be present");

        let info = plugin
            .get_connection("device1")
            .expect("Value expected to be present");
        assert_eq!(info.multi_paths.len(), 3);
        assert_eq!(info.multi_paths[0], "/storage/emulated/0");
        assert_eq!(info.multi_paths[2], "/storage/emulated/0/Pictures");
        assert_eq!(info.path_names.len(), 3);
        assert_eq!(info.path_names[1], "Camera Roll");
    }

    #[tokio::test]
    async fn test_handle_sftp_missing_optional_fields() {
        let plugin = SftpPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": "192.168.1.100",
                "port": 1740
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("Value expected to be present");

        let info = plugin
            .get_connection("device1")
            .expect("Value expected to be present");
        assert_eq!(info.ip, "192.168.1.100");
        assert_eq!(info.port, 1740);
        assert_eq!(info.user, "kdeconnect");
        assert_eq!(info.password, "");
        assert_eq!(info.path, "/");
        assert!(info.multi_paths.is_empty());
        assert!(info.path_names.is_empty());
    }
}

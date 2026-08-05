//! Remote Commands plugin
//!
//! Single Responsibility: Receive command list from the connected device and
//! send commands to trigger them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{info, warn};

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::plugins::plugin::Plugin;
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RemoteCommand {
    pub name: String,
    pub command: String,
}

pub struct RemoteCommandsPlugin {
    plugin_events: Arc<PluginEventBroadcaster>,
    commands: RwLock<HashMap<String, HashMap<String, RemoteCommand>>>,
    /// Whether the peer will accept a request to ADD a command. Sent
    /// alongside every command list (kdeconnect-kde
    /// plugins/runcommand/runcommandplugin.cpp:164-165) and read before the
    /// list by kde's own consumer
    /// (plugins/remotecommands/remotecommandsplugin.cpp:29-32).
    can_add_command: RwLock<HashMap<String, bool>>,
}

impl RemoteCommandsPlugin {
    pub fn new(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            plugin_events,
            commands: RwLock::new(HashMap::new()),
            can_add_command: RwLock::new(HashMap::new()),
        }
    }

    pub fn get_commands(&self, device_id: &str) -> Option<HashMap<String, RemoteCommand>> {
        self.commands
            .read()
            .ok()
            .and_then(|c| c.get(device_id).cloned())
    }

    pub fn can_add_command(&self, device_id: &str) -> bool {
        self.can_add_command
            .read()
            .ok()
            .and_then(|c| c.get(device_id).copied())
            .unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl Plugin for RemoteCommandsPlugin {
    fn name(&self) -> &str {
        "remotecommands"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.runcommand".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.runcommand.request".to_string()]
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        // Request the command list from the device when connected.
        let payload = serde_json::json!({
            "requestCommandList": true,
        });
        vec![Packet::new(
            "kdeconnect.runcommand.request".to_string(),
            payload,
        )]
    }

    fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut commands) = self.commands.write() {
            commands.remove(device_id);
        }
        if let Ok(mut flags) = self.can_add_command.write() {
            flags.remove(device_id);
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type != "kdeconnect.runcommand" {
            return Ok(None);
        }
        let Some(command_list_str) = packet.body.get("commandList").and_then(|v| v.as_str()) else {
            return Ok(None);
        };

        // Rides along with every list (runcommandplugin.cpp:164-165); kde
        // reads it first (remotecommandsplugin.cpp:29-32).
        let can_add = packet
            .body
            .get("canAddCommand")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Parse ENTRY BY ENTRY. A whole-map deserialize meant one malformed
        // entry silently dropped every command the peer offered.
        let entries: serde_json::Map<String, serde_json::Value> =
            match serde_json::from_str(command_list_str) {
                Ok(map) => map,
                Err(e) => {
                    warn!(
                        device_id = %device_id,
                        error = %e,
                        event = "remotecommands_list_parse_failed",
                        "commandList is not a JSON object, keeping the previous list"
                    );
                    return Ok(None);
                }
            };

        let mut commands: HashMap<String, RemoteCommand> = HashMap::new();
        for (key, value) in entries {
            match serde_json::from_value::<RemoteCommand>(value) {
                Ok(command) => {
                    commands.insert(key, command);
                }
                Err(e) => {
                    warn!(
                        device_id = %device_id,
                        command_key = %key,
                        error = %e,
                        event = "remotecommands_entry_skipped",
                        "Skipping malformed commandList entry"
                    );
                }
            }
        }

        info!(
            device_id = %device_id,
            commands = commands.len(),
            can_add_command = can_add,
            event = "remotecommands_list",
            "Received remote command list"
        );

        if let Ok(mut cmds) = self.commands.write() {
            cmds.insert(device_id.to_string(), commands.clone());
        }
        if let Ok(mut flags) = self.can_add_command.write() {
            flags.insert(device_id.to_string(), can_add);
        }

        self.plugin_events
            .broadcast(PluginEvent::RemoteCommandsUpdate {
                device_id: device_id.to_string(),
                commands,
            });

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn setup() -> (RemoteCommandsPlugin, Arc<PluginEventBroadcaster>) {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        (RemoteCommandsPlugin::new(broadcaster.clone()), broadcaster)
    }

    fn runcommand_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.runcommand".to_string(), body)
    }

    /// EXACT advertisement shape: `commandList` is a JSON STRING holding a
    /// map of key -> {name, command}, and `canAddCommand` rides along
    /// (kdeconnect-kde plugins/runcommand/runcommandplugin.cpp:164-165;
    /// kdeconnect-android RunCommandPlugin.java parses commandList as a
    /// string too).
    #[tokio::test]
    async fn test_command_list_parsed() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                runcommand_packet(serde_json::json!({
                    "commandList": "{\"abc\":{\"name\":\"Lock\",\"command\":\"loginctl lock-session\"}}",
                    "canAddCommand": true
                })),
            )
            .await
            .unwrap();

        let commands = plugin
            .get_commands("device1")
            .expect("Value expected to be present");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands["abc"].name, "Lock");
        assert_eq!(commands["abc"].command, "loginctl lock-session");
    }

    /// One malformed entry used to fail the whole HashMap deserialize inside
    /// `if let Ok`, silently dropping every command the peer offered.
    #[tokio::test]
    async fn test_malformed_entry_does_not_drop_the_list() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                runcommand_packet(serde_json::json!({
                    "commandList": "{\"a\":{\"name\":\"Kill\",\"command\":\"pkill x\"},\"b\":{\"name\":\"NoCommandKey\"},\"c\":{\"name\":\"List\",\"command\":\"ls\"}}"
                })),
            )
            .await
            .unwrap();

        let commands = plugin
            .get_commands("device1")
            .expect("Value expected to be present");
        assert_eq!(commands.len(), 2);
        assert!(commands.contains_key("a"));
        assert!(commands.contains_key("c"));
        assert!(!commands.contains_key("b"));
    }

    /// kde reads canAddCommand before the list itself
    /// (plugins/remotecommands/remotecommandsplugin.cpp:29-32).
    #[tokio::test]
    async fn test_can_add_command_read() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                runcommand_packet(serde_json::json!({
                    "commandList": "{}",
                    "canAddCommand": true
                })),
            )
            .await
            .unwrap();
        assert!(plugin.can_add_command("device1"));
    }

    #[tokio::test]
    async fn test_can_add_command_defaults_false() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                runcommand_packet(serde_json::json!({ "commandList": "{}" })),
            )
            .await
            .unwrap();
        assert!(!plugin.can_add_command("device1"));
        assert!(!plugin.can_add_command("never-seen"));
    }

    /// A commandList that is not a JSON object at all is dropped whole,
    /// leaving the previous list alone rather than blanking it.
    #[tokio::test]
    async fn test_non_object_command_list_is_dropped() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                runcommand_packet(serde_json::json!({
                    "commandList": "{\"a\":{\"name\":\"Kill\",\"command\":\"pkill x\"}}"
                })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                runcommand_packet(serde_json::json!({ "commandList": "not json at all" })),
            )
            .await
            .unwrap();

        let commands = plugin
            .get_commands("device1")
            .expect("Value expected to be present");
        assert_eq!(commands.len(), 1);
    }

    /// The advertisement is sent on connect (runcommandplugin.cpp:156-159);
    /// we ask for it with requestCommandList (remotecommandsplugin.cpp:37-38).
    #[tokio::test]
    async fn test_on_connected_requests_command_list() {
        let (plugin, _) = setup();
        let packets = plugin.on_connected("device1");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.runcommand.request");
        assert_eq!(
            packets[0]
                .body
                .get("requestCommandList")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_state() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                runcommand_packet(serde_json::json!({
                    "commandList": "{\"a\":{\"name\":\"Kill\",\"command\":\"pkill x\"}}",
                    "canAddCommand": true
                })),
            )
            .await
            .unwrap();
        plugin.on_disconnected("device1");
        assert!(plugin.get_commands("device1").is_none());
        assert!(!plugin.can_add_command("device1"));
    }
}

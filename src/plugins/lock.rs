//! Lock plugin
//!
//! Single Responsibility: Handle kdeconnect.lock packets
//! for remote lock/unlock of the phone screen.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

pub struct LockPlugin {
    /// Last known lock state per device.
    states: Arc<RwLock<HashMap<String, bool>>>,
}

impl Default for LockPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl LockPlugin {
    pub fn new() -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Last known lock state for a device, if any.
    pub async fn is_locked(&self, device_id: &str) -> Option<bool> {
        self.states.read().await.get(device_id).copied()
    }
}

#[async_trait::async_trait]
impl Plugin for LockPlugin {
    fn name(&self) -> &str {
        "lock"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.lock".to_string(),
            "kdeconnect.lock.request".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.lock".to_string(),
            "kdeconnect.lock.request".to_string(),
        ]
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        match packet.packet_type.as_str() {
            "kdeconnect.lock" => {
                let is_locked = packet
                    .body
                    .get("locked")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                self.states
                    .write()
                    .await
                    .insert(device_id.to_string(), is_locked);

                tracing::info!(
                    device_id = %device_id,
                    is_locked = is_locked,
                    event = "lock_update",
                    "Received lock state update"
                );

                Ok(None)
            }
            "kdeconnect.lock.request" => {
                // The peer asks for our last known lock state — answer with a
                // kdeconnect.lock packet (protocol: request/response pair).
                let is_locked = self.is_locked(device_id).await.unwrap_or(false);
                tracing::debug!(
                    device_id = %device_id,
                    is_locked = is_locked,
                    event = "lock_state_requested",
                    "Answering lock state request"
                );
                Ok(Some(vec![Packet::new(
                    "kdeconnect.lock".to_string(),
                    serde_json::json!({ "locked": is_locked }),
                )]))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_lock_plugin_name() {
        let plugin = LockPlugin::new();
        assert_eq!(plugin.name(), "lock");
    }

    #[tokio::test]
    async fn test_lock_capabilities() {
        let plugin = LockPlugin::new();
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.lock".to_string()));
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.lock.request".to_string()));
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.lock".to_string()));
    }

    #[tokio::test]
    async fn test_lock_state_stored() {
        // The rust plugin reads `locked` (a deliberate divergence from
        // kdeconnect-kde's `isLocked`/`lockResult` — see
        // tests/fixtures/upstream-wire/lock/lock_state.json provenance).
        let plugin = LockPlugin::new();
        assert_eq!(plugin.is_locked("device1").await, None);

        let locked_body = serde_json::json!({ "locked": true });
        let unlocked_body = serde_json::json!({ "locked": false });

        let packet = Packet::new("kdeconnect.lock".to_string(), locked_body.clone());
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("handle");
        assert_eq!(plugin.is_locked("device1").await, Some(true));

        let packet = Packet::new("kdeconnect.lock".to_string(), unlocked_body);
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("handle");
        assert_eq!(plugin.is_locked("device1").await, Some(false));

        // The exact wire body the rust plugin accepts is the upstream-derived
        // fixture literal at tests/fixtures/upstream-wire/lock/lock_state.json.
        let fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/lock/lock_state.json"),
            )
            .expect("lock/lock_state.json"),
        )
        .expect("lock/lock_state.json parses");
        let packet = Packet::new("kdeconnect.lock".to_string(), fixture);
        plugin
            .handle_packet("device2", packet)
            .await
            .expect("handle");
        assert_eq!(plugin.is_locked("device2").await, Some(true));
    }

    #[tokio::test]
    async fn test_lock_request_answers_with_stored_state() {
        // Upstream wire literal for the request side:
        // tests/fixtures/upstream-wire/lock/lock_request.json (empty body,
        // cited against kdeconnect-kde lockdeviceplugin.cpp:122).
        let plugin = LockPlugin::new();

        let state_packet = Packet::new(
            "kdeconnect.lock".to_string(),
            serde_json::json!({ "locked": true }),
        );
        plugin
            .handle_packet("device1", state_packet)
            .await
            .expect("handle");

        let request_body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/lock/lock_request.json"),
            )
            .expect("lock/lock_request.json"),
        )
        .expect("lock/lock_request.json parses");
        let request = Packet::new("kdeconnect.lock.request".to_string(), request_body);
        let reply = plugin
            .handle_packet("device1", request)
            .await
            .expect("handle")
            .expect("a lock.request must be answered");
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].packet_type, "kdeconnect.lock");
        assert_eq!(reply[0].body["locked"], true);
    }

    #[tokio::test]
    async fn test_handle_lock_missing_locked_field() {
        let plugin = LockPlugin::new();
        let packet = Packet::new(
            "kdeconnect.lock".to_string(),
            serde_json::json!({ "deviceId": "phone" }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
    }
}

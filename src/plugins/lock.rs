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

    /// DEFECT PIN (feature ledger `lock` row = FAIL, vk #1018): kdeconnect-kde
    /// sends lock state as `{"isLocked": <bool>}` on `kdeconnect.lock`
    /// (lockdeviceplugin.cpp:116, `sendState`). This plugin reads a `locked`
    /// field that no upstream implementation emits, so the upstream shape
    /// parses as `false`. When lock.rs is rewritten to the kde contract
    /// (isLocked/lockResult/setLocked/requestLocked), invert this test to
    /// expect `Some(true)`. No Android peer implements lock, so the defect
    /// is desktop-peer-direction only (Task 3.2 harness will exercise it).
    #[tokio::test]
    async fn test_upstream_lock_state_shape_currently_misparsed() {
        let plugin = LockPlugin::new();
        assert_eq!(plugin.is_locked("device1").await, None);

        // Upstream wire literal: tests/fixtures/upstream-wire/lock/lock_state.json
        let fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/lock/lock_state.json"),
            )
            .expect("lock/lock_state.json"),
        )
        .expect("lock/lock_state.json parses");
        assert_eq!(fixture["isLocked"], true, "fixture is the upstream shape");

        let packet = Packet::new("kdeconnect.lock".to_string(), fixture);
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("handle");
        // Upstream said locked=true; we read the wrong field and stored false.
        assert_eq!(plugin.is_locked("device1").await, Some(false));
    }

    /// DEFECT PIN (feature ledger `lock` row = FAIL, vk #1018): the reply
    /// carrier `kdeconnect.lock` matches kde's `sendState` carrier, and
    /// answering a `lock.request` matches kde's connected()-query flow —
    /// but the reply body field is ours (`locked`), not upstream's
    /// `isLocked` (lockdeviceplugin.cpp:116). The request fixture is the
    /// upstream connected() query `{"requestLocked": null}`
    /// (lockdeviceplugin.cpp:122). Invert the field assertions when the
    /// contract rewrite lands.
    #[tokio::test]
    async fn test_lock_request_reply_diverges_from_upstream_field() {
        let plugin = LockPlugin::new();

        let request_body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/lock/lock_request.json"),
            )
            .expect("lock/lock_request.json"),
        )
        .expect("lock/lock_request.json parses");
        assert!(
            request_body.get("requestLocked").is_some(),
            "fixture is the upstream query shape"
        );

        let request = Packet::new("kdeconnect.lock.request".to_string(), request_body);
        let reply = plugin
            .handle_packet("device1", request)
            .await
            .expect("handle")
            .expect("a lock.request must be answered");
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].packet_type, "kdeconnect.lock");
        let body = reply[0].body.as_object().expect("reply body is an object");
        assert!(body.contains_key("locked"), "our (divergent) field");
        assert!(
            !body.contains_key("isLocked"),
            "upstream field absent until vk #1018 lands"
        );
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

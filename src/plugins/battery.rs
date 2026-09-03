//! Battery plugin
//!
//! Single Responsibility: Handle kdeconnect.battery packets and broadcast battery events.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use tracing::info;

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryInfo {
    pub current_charge: i32,
    pub is_charging: bool,
    pub threshold_event: i32,
}

pub struct BatteryPlugin {
    batteries: Arc<StdRwLock<HashMap<String, BatteryInfo>>>,
    plugin_events: Arc<PluginEventBroadcaster>,
}

impl BatteryPlugin {
    pub fn new(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            batteries: Arc::new(StdRwLock::new(HashMap::new())),
            plugin_events,
        }
    }

    pub fn get_battery(&self, device_id: &str) -> Option<BatteryInfo> {
        let batteries = self.batteries.read().unwrap_or_else(|e| e.into_inner());
        batteries.get(device_id).cloned()
    }
}

#[async_trait::async_trait]
impl Plugin for BatteryPlugin {
    fn name(&self) -> &str {
        "battery"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.battery".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.battery.request".to_string()]
    }

    async fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut batteries) = self.batteries.write() {
            batteries.remove(device_id);
        }
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        vec![Packet::new(
            "kdeconnect.battery.request".to_string(),
            // GSConnect battery.js:364-368 `_requestState` sends
            // {"request": true}. We sent an empty body (vk #1018).
            serde_json::json!({ "request": true }),
        )]
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let info: BatteryInfo = packet.body_as("battery")?;

        info!(
            device_id = %device_id,
            charge = info.current_charge,
            charging = info.is_charging,
            event = "battery_update",
            "Received battery update"
        );

        if let Ok(mut batteries) = self.batteries.write() {
            batteries.insert(device_id.to_string(), info.clone());
        }

        self.plugin_events.broadcast(PluginEvent::Battery {
            device_id: device_id.to_string(),
            current_charge: info.current_charge,
            is_charging: info.is_charging,
        });

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn make_plugin() -> BatteryPlugin {
        BatteryPlugin::new(Arc::new(PluginEventBroadcaster::new(16, "plugin")))
    }

    #[tokio::test]
    async fn test_battery_plugin_capabilities() {
        let plugin = make_plugin();
        assert_eq!(plugin.name(), "battery");
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.battery".to_string()));
    }

    #[tokio::test]
    async fn test_handle_battery_packet_stores_data() {
        let plugin = make_plugin();
        let packet = Packet::new(
            "kdeconnect.battery".to_string(),
            serde_json::json!({
                "currentCharge": 85,
                "isCharging": true,
                "thresholdEvent": 0
            }),
        );

        plugin
            .handle_packet("phone-1", packet)
            .await
            .expect("Value expected to be present");
        let battery = plugin.get_battery("phone-1");
        assert!(battery.is_some());
        let battery = battery.expect("Value expected to be present");
        assert_eq!(battery.current_charge, 85);
        assert!(battery.is_charging);
    }

    #[tokio::test]
    async fn test_get_battery_none_initially() {
        let plugin = make_plugin();
        assert!(plugin.get_battery("test").is_none());
    }

    #[tokio::test]
    async fn test_handle_malformed_battery_packet() {
        let plugin = make_plugin();
        let packet = Packet::new(
            "kdeconnect.battery".to_string(),
            serde_json::json!({"garbage": true}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_outgoing_capabilities() {
        let plugin = make_plugin();
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.battery.request".to_string()));
    }

    #[tokio::test]
    async fn test_on_disconnected_cleans_up() {
        let plugin = make_plugin();
        let packet = Packet::new(
            "kdeconnect.battery".to_string(),
            serde_json::json!({
                "currentCharge": 85,
                "isCharging": true,
                "thresholdEvent": 0
            }),
        );
        plugin
            .handle_packet("phone-1", packet)
            .await
            .expect("Value expected to be present");
        assert!(plugin.get_battery("phone-1").is_some());

        plugin.on_disconnected("phone-1").await;
        assert!(plugin.get_battery("phone-1").is_none());
    }

    #[tokio::test]
    async fn test_on_connected_requests_battery() {
        // Was a DIVERGENCE PIN (vk #1018): we sent an empty body where
        // GSConnect sends {"request": true} (battery.js:364-368
        // `_requestState`). Aligned 2026-08-25; the pin said to invert to
        // assert_eq! when it landed, so this now asserts equality with the
        // upstream fixture rather than difference from it.
        let plugin = make_plugin();
        let packets = plugin.on_connected("test-device");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.battery.request");
        let upstream_body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/battery/request.json"),
            )
            .expect("battery/request.json"),
        )
        .expect("battery/request.json parses");
        assert_eq!(
            upstream_body["request"], true,
            "fixture is the upstream shape"
        );
        assert_eq!(
            packets[0].body, upstream_body,
            "on_connected must send the upstream request shape verbatim"
        );
    }

    #[tokio::test]
    async fn test_battery_broadcasts_event() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = BatteryPlugin::new(broadcaster.clone());

        let packet = Packet::new(
            "kdeconnect.battery".to_string(),
            serde_json::json!({
                "currentCharge": 75,
                "isCharging": false,
                "thresholdEvent": 0
            }),
        );
        plugin
            .handle_packet("phone-1", packet)
            .await
            .expect("Value expected to be present");

        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            PluginEvent::Battery {
                device_id,
                current_charge,
                is_charging,
            } => {
                assert_eq!(device_id, "phone-1");
                assert_eq!(current_charge, 75);
                assert!(!is_charging);
            }
            _ => panic!("Wrong event type"),
        }
    }
}

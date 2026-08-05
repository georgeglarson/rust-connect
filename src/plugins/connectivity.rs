//! Connectivity Report plugin
//!
//! Single Responsibility: Handle kdeconnect.connectivity_report packets
//! for receiving network signal and connectivity information from devices.
//!
//! Protocol (kdeconnect-android ConnectivityReportPlugin.kt):
//! - Incoming: kdeconnect.connectivity_report
//!   Body: { "signalStrengths": { "<subId>": { "signalStrength": 0-4, "networkType": "LTE" } } }

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityReport {
    pub signal_strength: i32,
    pub network_type: String,
}

pub struct ConnectivityPlugin {
    reports: Arc<StdRwLock<HashMap<String, ConnectivityReport>>>,
}

impl Default for ConnectivityPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectivityPlugin {
    pub fn new() -> Self {
        Self {
            reports: Arc::new(StdRwLock::new(HashMap::new())),
        }
    }

    #[allow(clippy::expect_used)]
    pub fn get_report(&self, device_id: &str) -> Option<ConnectivityReport> {
        let reports = self.reports.read().unwrap_or_else(|e| e.into_inner());
        reports.get(device_id).cloned()
    }
}

#[async_trait::async_trait]
impl Plugin for ConnectivityPlugin {
    fn name(&self) -> &str {
        "connectivity"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.connectivity_report".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // Nothing. The report is push-only: the phone sends it when the radio
        // state changes (kdeconnect-android
        // .../connectivityreport/ConnectivityReportPlugin.kt:51-68), and no
        // implementation consumes a `.request` — Android's
        // supportedPacketTypes is emptyArray() (:84) with onPacketReceived
        // returning false (:80-82), and kde's manifest declares
        // "X-KdeConnect-OutgoingPacketType": [] (plugins/connectivity-report/
        // kdeconnect_connectivity_report.json:155). The one mention anywhere
        // is prose in kde's plugins/connectivity-report/README:10.
        vec![]
    }

    fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut reports) = self.reports.write() {
            reports.remove(device_id);
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type != "kdeconnect.connectivity_report" {
            return Ok(None);
        }

        if let Some(signal_strengths) = packet.body.get("signalStrengths") {
            if let Some(obj) = signal_strengths.as_object() {
                for (sub_id, info) in obj {
                    let signal_strength = info
                        .get("signalStrength")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;
                    let network_type = info
                        .get("networkType")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown")
                        .to_string();

                    info!(
                        device_id = %device_id,
                        subscription_id = %sub_id,
                        signal_strength = signal_strength,
                        network_type = %network_type,
                        event = "connectivity_report",
                        "Received connectivity report"
                    );

                    if let Ok(mut reports) = self.reports.write() {
                        reports.insert(
                            device_id.to_string(),
                            ConnectivityReport {
                                signal_strength,
                                network_type,
                            },
                        );
                    }
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_connectivity_plugin_name() {
        let plugin = ConnectivityPlugin::new();
        assert_eq!(plugin.name(), "connectivity");
    }

    #[tokio::test]
    async fn test_connectivity_capabilities() {
        let plugin = ConnectivityPlugin::new();
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.connectivity_report".to_string()));
    }

    #[tokio::test]
    async fn test_handle_connectivity_packet() {
        let plugin = ConnectivityPlugin::new();
        let packet = Packet::new(
            "kdeconnect.connectivity_report".to_string(),
            serde_json::json!({
                "signalStrengths": {
                    "6": {
                        "signalStrength": 4,
                        "networkType": "LTE"
                    }
                }
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
    }

    #[tokio::test]
    async fn test_get_report() {
        let plugin = ConnectivityPlugin::new();
        let packet = Packet::new(
            "kdeconnect.connectivity_report".to_string(),
            serde_json::json!({
                "signalStrengths": {
                    "6": {
                        "signalStrength": 3,
                        "networkType": "LTE"
                    }
                }
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("Value expected to be present");

        let report = plugin.get_report("device1");
        assert!(report.is_some());
        assert_eq!(
            report
                .expect("Value expected to be present")
                .signal_strength,
            3
        );
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_report() {
        let plugin = ConnectivityPlugin::new();
        let packet = Packet::new(
            "kdeconnect.connectivity_report".to_string(),
            serde_json::json!({
                "signalStrengths": {
                    "6": {
                        "signalStrength": 4,
                        "networkType": "LTE"
                    }
                }
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("Value expected to be present");
        assert!(plugin.get_report("device1").is_some());

        plugin.on_disconnected("device1");
        assert!(plugin.get_report("device1").is_none());
    }

    #[tokio::test]
    async fn test_multiple_sub_ids() {
        let plugin = ConnectivityPlugin::new();
        let packet = Packet::new(
            "kdeconnect.connectivity_report".to_string(),
            serde_json::json!({
                "signalStrengths": {
                    "1": { "signalStrength": 2, "networkType": "LTE" },
                    "2": { "signalStrength": 4, "networkType": "5G" },
                    "3": { "signalStrength": 1, "networkType": "WiFi" }
                }
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());

        let report = plugin.get_report("device1");
        assert!(report.is_some());
        assert_eq!(
            report
                .expect("Value expected to be present")
                .signal_strength,
            1
        );
    }

    /// Connect must send NOTHING. `kdeconnect.connectivity_report.request` is
    /// implemented by no upstream: kdeconnect-android
    /// .../connectivityreport/ConnectivityReportPlugin.kt:80-84 returns false
    /// from onPacketReceived and declares `supportedPacketTypes = emptyArray()`;
    /// kdeconnect-kde plugins/connectivity-report/kdeconnect_connectivity_report.json:155
    /// declares `"X-KdeConnect-OutgoingPacketType": []`. The report is
    /// push-only (ConnectivityReportPlugin.kt:51-68).
    #[tokio::test]
    async fn test_on_connected_sends_nothing() {
        let plugin = ConnectivityPlugin::new();
        assert!(plugin.on_connected("test-device").is_empty());
    }

    #[tokio::test]
    async fn test_no_outgoing_capabilities() {
        let plugin = ConnectivityPlugin::new();
        assert!(plugin.outgoing_capabilities().is_empty());
    }
}

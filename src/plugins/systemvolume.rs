//! SystemVolume plugin
//!
//! Single Responsibility: track a DESKTOP peer's audio sinks from
//! `kdeconnect.systemvolume` packets and ask for the list on connect.
//!
//! Role: this is upstream's "remotesystemvolume" CONTROLLER side — consume
//! `kdeconnect.systemvolume`, emit `kdeconnect.systemvolume.request`. It can
//! only ever fire against a kdeconnect-kde or GSConnect desktop peer:
//! kdeconnect-android is a controller too
//! (.../systemvolume/SystemVolumePlugin.kt:91-92), so two controllers never
//! talk to each other. The provider direction (a phone controlling OUR
//! PulseAudio volume) is a separate build and is NOT implemented here.
//!
//! Wire shape, from kdeconnect-kde plugins/systemvolume/systemvolumeplugin-pulse.cpp:
//! - The full state arrives as a `sinkList` ARRAY of sink objects (:90-104),
//!   which upstream consumers CLEAR and rebuild from
//!   (kdeconnect-android .../systemvolume/SystemVolumePlugin.kt:33-42).
//! - Deltas arrive as single-field packets keyed by `name`: `volume` (:71-72),
//!   `muted` (:78-79), `enabled` (:85-86). A delta naming a sink we have
//!   never seen is ignored, matching SystemVolumePlugin.kt:53-55.
//! - `volume` is an INTEGER on an absolute scale whose ceiling is the sink's
//!   `maxVolume` (PulseAudioQt::normalVolume() == 65536, :94; Sink.kt:27 reads
//!   it with getInt). It is NOT a 0.0-1.0 fraction.

use std::collections::HashMap;
use std::sync::RwLock;

use tracing::{info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// One PulseAudio sink as the desktop peer describes it.
///
/// Keys match the object kdeconnect-kde puts in `sinkList`
/// (systemvolumeplugin-pulse.cpp:90-95) and that kdeconnect-android reads in
/// .../systemvolume/Sink.kt:26-31.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SinkState {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Absolute, integer, ceiling is `max_volume` (pulse.cpp:71,94).
    #[serde(default)]
    pub volume: Option<i64>,
    #[serde(default)]
    pub max_volume: Option<i64>,
    #[serde(default)]
    pub muted: Option<bool>,
    /// This sink is the default output (pulse.cpp:85,95; Sink.kt:31).
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub struct SystemVolumePlugin {
    /// device_id -> sink name -> sink state.
    sinks: RwLock<HashMap<String, HashMap<String, SinkState>>>,
}

impl Default for SystemVolumePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemVolumePlugin {
    pub fn new() -> Self {
        Self {
            sinks: RwLock::new(HashMap::new()),
        }
    }

    /// All known sinks for a device, sorted by name so callers get a stable
    /// order out of the map.
    pub fn get_sinks(&self, device_id: &str) -> Vec<SinkState> {
        let guard = self.sinks.read().unwrap_or_else(|e| e.into_inner());
        let mut sinks: Vec<SinkState> = guard
            .get(device_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        sinks.sort_by(|a, b| a.name.cmp(&b.name));
        sinks
    }

    pub fn get_sink(&self, device_id: &str, name: &str) -> Option<SinkState> {
        self.sinks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .and_then(|m| m.get(name))
            .cloned()
    }
}

#[async_trait::async_trait]
impl Plugin for SystemVolumePlugin {
    fn name(&self) -> &str {
        "systemvolume"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.systemvolume".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.systemvolume.request".to_string()]
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        // kdeconnect-kde answers `requestSinks` with the whole sink list
        // (systemvolumeplugin-pulse.cpp:36-37). It also pushes on connect
        // (:107-118), but asking is what covers a peer that was already up.
        vec![Packet::new(
            "kdeconnect.systemvolume.request".to_string(),
            serde_json::json!({ "requestSinks": true }),
        )]
    }

    fn on_disconnected(&self, device_id: &str) {
        self.sinks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type != "kdeconnect.systemvolume" {
            return Ok(None);
        }
        let body = &packet.body;

        // Full state. Upstream clears its map before refilling
        // (SystemVolumePlugin.kt:33-42), so this replaces rather than merges.
        if let Some(list) = body.get("sinkList").and_then(|v| v.as_array()) {
            let mut parsed: HashMap<String, SinkState> = HashMap::new();
            for entry in list {
                match serde_json::from_value::<SinkState>(entry.clone()) {
                    Ok(sink) if !sink.name.is_empty() => {
                        parsed.insert(sink.name.clone(), sink);
                    }
                    Ok(_) => {
                        warn!(
                            device_id = %device_id,
                            event = "systemvolume_sink_unnamed",
                            "Dropping sinkList entry with no name"
                        );
                    }
                    Err(e) => {
                        warn!(
                            device_id = %device_id,
                            error = %e,
                            event = "systemvolume_sink_parse_failed",
                            "Dropping malformed sinkList entry"
                        );
                    }
                }
            }
            info!(
                device_id = %device_id,
                sinks = parsed.len(),
                event = "systemvolume_sink_list",
                "Received sink list"
            );
            self.sinks
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(device_id.to_string(), parsed);
            return Ok(None);
        }

        // Otherwise a per-sink delta keyed by `name` (pulse.cpp:72,79,86).
        let Some(name) = body.get("name").and_then(|v| v.as_str()) else {
            warn!(
                device_id = %device_id,
                event = "systemvolume_update_unkeyed",
                "systemvolume packet has neither sinkList nor name, ignoring"
            );
            return Ok(None);
        };

        let mut guard = self.sinks.write().unwrap_or_else(|e| e.into_inner());
        // Upstream ignores a delta for an unknown sink: SystemVolumePlugin.kt:
        // 53-55 looks the name up and does nothing when it is absent.
        let Some(sink) = guard.get_mut(device_id).and_then(|m| m.get_mut(name)) else {
            warn!(
                device_id = %device_id,
                sink = %name,
                event = "systemvolume_unknown_sink",
                "Update for a sink not in the last sinkList, ignoring"
            );
            return Ok(None);
        };

        if let Some(volume) = body.get("volume").and_then(|v| v.as_i64()) {
            sink.volume = Some(volume);
        }
        if let Some(muted) = body.get("muted").and_then(|v| v.as_bool()) {
            sink.muted = Some(muted);
        }
        if let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) {
            sink.enabled = Some(enabled);
        }

        info!(
            device_id = %device_id,
            sink = %name,
            volume = ?sink.volume,
            muted = ?sink.muted,
            enabled = ?sink.enabled,
            event = "systemvolume_update",
            "Received system volume update"
        );

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn volume_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.systemvolume".to_string(), body)
    }

    /// EXACT sinkList kdeconnect-kde builds from PulseAudio
    /// (plugins/systemvolume/systemvolumeplugin-pulse.cpp:90-104). `volume` is
    /// an int on an absolute scale; `maxVolume` is
    /// PulseAudioQt::normalVolume() == 65536 (:94).
    fn kde_sink_list() -> serde_json::Value {
        serde_json::json!({
            "sinkList": [
                {
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo",
                    "muted": false,
                    "description": "Built-in Audio Analog Stereo",
                    "volume": 45874,
                    "maxVolume": 65536,
                    "enabled": true
                },
                {
                    "name": "alsa_output.usb-Generic_USB_Audio-00.analog-stereo",
                    "muted": true,
                    "description": "USB Audio Analog Stereo",
                    "volume": 65536,
                    "maxVolume": 65536,
                    "enabled": false
                }
            ]
        })
    }

    #[tokio::test]
    async fn test_systemvolume_name_and_capabilities() {
        let plugin = SystemVolumePlugin::new();
        assert_eq!(plugin.name(), "systemvolume");
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.systemvolume".to_string()));
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.systemvolume.request".to_string()));
    }

    #[tokio::test]
    async fn test_sink_list_parsed_per_sink() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", volume_packet(kde_sink_list()))
            .await
            .unwrap();

        let sinks = plugin.get_sinks("device1");
        assert_eq!(sinks.len(), 2);

        let builtin = plugin
            .get_sink("device1", "alsa_output.pci-0000_00_1f.3.analog-stereo")
            .expect("Value expected to be present");
        assert_eq!(builtin.volume, Some(45874));
        assert_eq!(builtin.max_volume, Some(65536));
        assert_eq!(builtin.muted, Some(false));
        assert_eq!(builtin.enabled, Some(true));
        assert_eq!(
            builtin.description.as_deref(),
            Some("Built-in Audio Analog Stereo")
        );

        let usb = plugin
            .get_sink(
                "device1",
                "alsa_output.usb-Generic_USB_Audio-00.analog-stereo",
            )
            .expect("Value expected to be present");
        assert_eq!(usb.volume, Some(65536));
        assert_eq!(usb.muted, Some(true));
        assert_eq!(usb.enabled, Some(false));
    }

    /// A fresh sinkList replaces the whole set, the way upstream clears its
    /// map before refilling (SystemVolumePlugin.kt:33-42, pulse.cpp:63).
    #[tokio::test]
    async fn test_sink_list_replaces_previous_set() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", volume_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                volume_packet(serde_json::json!({
                    "sinkList": [
                        { "name": "only-one", "muted": false, "description": "d",
                          "volume": 100, "maxVolume": 65536, "enabled": true }
                    ]
                })),
            )
            .await
            .unwrap();
        let sinks = plugin.get_sinks("device1");
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "only-one");
    }

    /// Per-sink deltas are keyed by `name` (pulse.cpp:72,79,86) and must not
    /// touch the other sink.
    #[tokio::test]
    async fn test_per_sink_volume_update_keyed_by_name() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", volume_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                volume_packet(serde_json::json!({
                    "volume": 32768,
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo"
                })),
            )
            .await
            .unwrap();

        assert_eq!(
            plugin
                .get_sink("device1", "alsa_output.pci-0000_00_1f.3.analog-stereo")
                .expect("Value expected to be present")
                .volume,
            Some(32768)
        );
        assert_eq!(
            plugin
                .get_sink(
                    "device1",
                    "alsa_output.usb-Generic_USB_Audio-00.analog-stereo"
                )
                .expect("Value expected to be present")
                .volume,
            Some(65536)
        );
    }

    /// mutedChanged and defaultChanged deltas (pulse.cpp:76-88).
    #[tokio::test]
    async fn test_per_sink_muted_and_enabled_updates() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", volume_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                volume_packet(serde_json::json!({
                    "muted": true,
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo"
                })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                volume_packet(serde_json::json!({
                    "enabled": false,
                    "name": "alsa_output.pci-0000_00_1f.3.analog-stereo"
                })),
            )
            .await
            .unwrap();

        let sink = plugin
            .get_sink("device1", "alsa_output.pci-0000_00_1f.3.analog-stereo")
            .expect("Value expected to be present");
        assert_eq!(sink.muted, Some(true));
        assert_eq!(sink.enabled, Some(false));
    }

    /// Upstream ignores a delta for a sink it has never seen in a sinkList
    /// (SystemVolumePlugin.kt:53-55: `sinkMap[name]` is null, nothing happens).
    #[tokio::test]
    async fn test_update_for_unknown_sink_is_ignored() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", volume_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                volume_packet(serde_json::json!({ "volume": 1, "name": "ghost-sink" })),
            )
            .await
            .unwrap();
        assert_eq!(plugin.get_sinks("device1").len(), 2);
        assert!(plugin.get_sink("device1", "ghost-sink").is_none());
    }

    /// Regression: a sinkList packet used to deserialize into
    /// VolumeUpdate { None, None } and blank the stored state.
    #[tokio::test]
    async fn test_sink_list_does_not_blank_state() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", volume_packet(kde_sink_list()))
            .await
            .unwrap();
        assert!(!plugin.get_sinks("device1").is_empty());
    }

    /// A malformed entry is skipped; the good ones survive.
    #[tokio::test]
    async fn test_malformed_sink_entry_skipped() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet(
                "device1",
                volume_packet(serde_json::json!({
                    "sinkList": [
                        { "name": "good", "volume": 100, "maxVolume": 65536,
                          "muted": false, "description": "d", "enabled": true },
                        { "volume": 200 },
                        "not-an-object"
                    ]
                })),
            )
            .await
            .unwrap();
        let sinks = plugin.get_sinks("device1");
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "good");
    }

    /// kde answers `requestSinks` with the full list (pulse.cpp:36-37), so
    /// asking on connect is what makes a peer that missed our arrival reply.
    #[tokio::test]
    async fn test_on_connected_requests_sinks() {
        let plugin = SystemVolumePlugin::new();
        let packets = plugin.on_connected("device1");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.systemvolume.request");
        assert_eq!(
            packets[0]
                .body
                .get("requestSinks")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_sinks() {
        let plugin = SystemVolumePlugin::new();
        plugin
            .handle_packet("device1", volume_packet(kde_sink_list()))
            .await
            .unwrap();
        plugin.on_disconnected("device1");
        assert!(plugin.get_sinks("device1").is_empty());
    }

    /// The integer scale, pinned. `volume` is not a 0.0-1.0 fraction.
    #[tokio::test]
    async fn test_volume_is_an_integer_on_the_max_volume_scale() {
        let sink: SinkState = serde_json::from_value(serde_json::json!({
            "name": "s",
            "muted": false,
            "description": "d",
            "volume": 65536,
            "maxVolume": 65536,
            "enabled": true
        }))
        .expect("Value expected to be present");
        assert_eq!(sink.volume, Some(65536));
        assert_eq!(sink.max_volume, Some(65536));
    }
}

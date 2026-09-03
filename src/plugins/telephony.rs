//! Telephony plugin
//!
//! Single Responsibility: Handle kdeconnect.telephony packets for incoming
//! call and SMS notifications from the remote device.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use tracing::info;

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// One telephony event as the phone sends it.
///
/// Wire shape from kdeconnect-android .../plugins/telephony/TelephonyPlugin.kt:
/// `phoneNumber` (:99), `contactName` (:78, :95), `phoneThumbnail` as a
/// base64 JPEG (:87), and `event` from the vocabulary "ringing" (:105),
/// "talking" (:109), "missedCall" (:129), plus the legacy "sms" that
/// kdeconnect-kde ignores (plugins/telephony/telephonyplugin.cpp:82).
/// kde reads the same keys at telephonyplugin.cpp:22-25.
///
/// There is no `timestamp` on this packet in any upstream implementation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelephonyInfo {
    pub event: String,
    #[serde(default)]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub contact_name: Option<String>,
    #[serde(default)]
    pub phone_thumbnail: Option<String>,
    /// The call ended. Android RESENDS the last event with this set, as a
    /// JSON STRING "true" (TelephonyPlugin.kt:113-116); a v8+ peer may send a
    /// real bool. kde checks it before anything else (telephonyplugin.cpp:74).
    /// Same both-encodings shape pausemusic already handles
    /// (src/plugins/pausemusic.rs:76-82).
    #[serde(default, deserialize_with = "de_is_cancel")]
    pub is_cancel: bool,
}

/// Accepts bool `true` and string `"true"` (case-insensitive); anything else
/// is not a cancel.
pub(crate) fn is_cancel_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn de_is_cancel<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(is_cancel_value(&value))
}

/// Cap on retained call events per device (2026-09-02 audit, B5): each
/// entry may carry a base64 `phoneThumbnail`, and a paired peer could grow
/// the log without bound for the life of a connection. Oldest out.
pub const MAX_CALLS_PER_DEVICE: usize = 100;

pub struct TelephonyPlugin {
    calls: Arc<StdRwLock<HashMap<String, Vec<TelephonyInfo>>>>,
    plugin_events: Arc<PluginEventBroadcaster>,
}

impl TelephonyPlugin {
    pub fn new(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            calls: Arc::new(StdRwLock::new(HashMap::new())),
            plugin_events,
        }
    }

    #[allow(clippy::expect_used)]
    pub fn get_calls(&self, device_id: &str) -> Vec<TelephonyInfo> {
        let calls = self.calls.read().unwrap_or_else(|e| e.into_inner());
        calls.get(device_id).cloned().unwrap_or_default()
    }
}

/// Desktop -> phone "mute the ringing call".
///
/// kdeconnect-kde sends this from a "Mute Call" action it attaches to the
/// ringing notification (`plugins/telephony/telephonyplugin.cpp:66,87-91`);
/// all three upstreams declare it outgoing
/// (`tests/fixtures/upstream-capabilities/*.yaml`).
pub const MUTE_REQUEST_PACKET_TYPE: &str = "kdeconnect.telephony.request_mute";

impl TelephonyPlugin {
    /// The mute-request packet, exactly as kdeconnect-kde builds it.
    ///
    /// The body is NOT empty: upstream sends `{"action": "mute"}`
    /// (`telephonyplugin.cpp:89`). Pure so the wire shape is pinned by a unit
    /// test with no device, no socket and no connection manager.
    pub fn mute_request_packet() -> Packet {
        Packet::new(
            MUTE_REQUEST_PACKET_TYPE.to_string(),
            serde_json::json!({ "action": "mute" }),
        )
    }
}

#[async_trait::async_trait]
impl Plugin for TelephonyPlugin {
    fn name(&self) -> &str {
        "telephony"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.telephony".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec![MUTE_REQUEST_PACKET_TYPE.to_string()]
    }

    async fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut calls) = self.calls.write() {
            calls.remove(device_id);
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let info: TelephonyInfo = packet.body_as("telephony")?;

        // Mask by CHARACTER, not by byte: phoneNumber is peer-controlled and
        // a byte slice panics when the tail splits a multi-byte character.
        let masked_number = info.phone_number.as_ref().map(|n| {
            let count = n.chars().count();
            if count > 4 {
                let tail: String = n.chars().skip(count - 4).collect();
                format!("****{tail}")
            } else {
                n.clone()
            }
        });

        info!(
            device_id = %device_id,
            event_type = %info.event,
            phone_number = ?masked_number,
            is_cancel = info.is_cancel,
            event = "telephony_update",
            "Received telephony update"
        );

        // A cancel RESENDS the previous event (TelephonyPlugin.kt:113-116),
        // so it is the END of the call already in the list, not a new one.
        // kdeconnect-kde returns before creating any notification
        // (telephonyplugin.cpp:74-79). Appending would duplicate the entry.
        if !info.is_cancel {
            if let Ok(mut calls) = self.calls.write() {
                let log = calls.entry(device_id.to_string()).or_default();
                log.push(info.clone());
                if log.len() > MAX_CALLS_PER_DEVICE {
                    let excess = log.len() - MAX_CALLS_PER_DEVICE;
                    log.drain(..excess);
                }
            }
        }

        self.plugin_events.broadcast(PluginEvent::TelephonyUpdate {
            device_id: device_id.to_string(),
            info,
        });

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn setup() -> (TelephonyPlugin, Arc<PluginEventBroadcaster>) {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        (TelephonyPlugin::new(broadcaster.clone()), broadcaster)
    }

    fn telephony_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.telephony".to_string(), body)
    }

    #[test]
    fn test_mute_request_wire_shape_matches_upstream() {
        // kdeconnect-kde plugins/telephony/telephonyplugin.cpp:89 —
        //   NetworkPacket(PACKET_TYPE_TELEPHONY_REQUEST_MUTE,
        //                 {{"action", "mute"}})
        // The body is NOT empty. The lane brief said "likely empty-body,
        // verify, don't assume", and the assumption would have been wrong.
        let packet = TelephonyPlugin::mute_request_packet();
        assert_eq!(packet.packet_type, MUTE_REQUEST_PACKET_TYPE);
        assert_eq!(
            packet.body,
            serde_json::json!({ "action": "mute" }),
            "body must match telephonyplugin.cpp:89 exactly"
        );
    }

    #[tokio::test]
    async fn test_mute_request_is_advertised_as_outgoing() {
        // All three upstreams declare it outgoing
        // (tests/fixtures/upstream-capabilities/*.yaml). Advertising it is
        // what tells the phone we can send it.
        let (plugin, _) = setup();
        assert!(
            plugin
                .outgoing_capabilities()
                .contains(&MUTE_REQUEST_PACKET_TYPE.to_string()),
            "outgoing was {:?}",
            plugin.outgoing_capabilities()
        );
    }

    #[tokio::test]
    async fn test_mute_request_is_not_claimed_as_incoming() {
        // We SEND request_mute; the phone consumes it. Claiming it incoming
        // would advertise a handler that does not exist.
        let (plugin, _) = setup();
        assert!(!plugin
            .incoming_capabilities()
            .contains(&MUTE_REQUEST_PACKET_TYPE.to_string()));
    }

    #[tokio::test]
    async fn test_telephony_plugin_name_and_capabilities() {
        let (plugin, _) = setup();
        assert_eq!(plugin.name(), "telephony");
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.telephony".to_string()));
        // Was `is_empty()` while the plugin was receive-only. The mute leg
        // (vk #1043) makes request_mute the ONE outgoing capability, so pin
        // the exact set rather than loosening the assertion to nothing.
        assert_eq!(
            plugin.outgoing_capabilities(),
            vec![MUTE_REQUEST_PACKET_TYPE.to_string()]
        );
    }

    /// EXACT body Android sends on an incoming call with contacts permission
    /// granted (kdeconnect-android .../telephony/TelephonyPlugin.kt:78,99,105).
    /// Fixture: tests/fixtures/upstream-wire/telephony/ringing.json
    ///   kdeconnect-android@a88f6fa0 TelephonyPlugin.kt:78,95,99,105
    #[tokio::test]
    async fn test_ringing_real_wire_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/telephony/ringing.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read telephony ringing fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let (plugin, _) = setup();
        plugin
            .handle_packet("device1", telephony_packet(upstream_body))
            .await
            .unwrap();
        let calls = plugin.get_calls("device1");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].event, "ringing");
        assert_eq!(calls[0].phone_number.as_deref(), Some("+1234567890"));
        assert_eq!(calls[0].contact_name.as_deref(), Some("John Doe"));
        assert!(!calls[0].is_cancel);
    }

    /// Regression: `number` was this plugin's own invention. A packet
    /// carrying only that key must capture nothing, so nobody reintroduces
    /// it as an "alias" without evidence it is on the wire.
    #[tokio::test]
    async fn test_invented_number_field_captures_nothing() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({
                    "event": "ringing",
                    "number": "+1234567890"
                })),
            )
            .await
            .unwrap();
        let calls = plugin.get_calls("device1");
        assert_eq!(calls.len(), 1);
        assert!(calls[0].phone_number.is_none());
    }

    /// The real event vocabulary: ringing (TelephonyPlugin.kt:105), talking
    /// (:109), missedCall (:129). "incoming" and "missed" were ours.
    #[tokio::test]
    async fn test_real_event_vocabulary_is_recorded() {
        let (plugin, _) = setup();
        for event in ["ringing", "talking", "missedCall"] {
            plugin
                .handle_packet(
                    "device1",
                    telephony_packet(serde_json::json!({
                        "event": event,
                        "phoneNumber": "+1234567890"
                    })),
                )
                .await
                .unwrap();
        }
        let calls = plugin.get_calls("device1");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].event, "missedCall");
    }

    /// EXACT cancel shape: the LAST packet resent with isCancel as a JSON
    /// STRING (TelephonyPlugin.kt:113-116). It ends the call it names, so it
    /// must not append a second call-list entry.
    #[tokio::test]
    async fn test_cancel_string_true_does_not_append_duplicate() {
        let (plugin, _) = setup();
        let ringing = serde_json::json!({
            "event": "ringing",
            "phoneNumber": "+1234567890",
            "contactName": "John Doe"
        });
        plugin
            .handle_packet("device1", telephony_packet(ringing.clone()))
            .await
            .unwrap();

        let mut cancel = ringing;
        cancel["isCancel"] = serde_json::Value::String("true".to_string());
        plugin
            .handle_packet("device1", telephony_packet(cancel))
            .await
            .unwrap();

        assert_eq!(plugin.get_calls("device1").len(), 1);
    }

    /// A v8+ peer may send a real bool; kde's QVariant read accepts either
    /// (telephonyplugin.cpp:74).
    #[tokio::test]
    async fn test_cancel_bool_true_recognized() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "talking" })),
            )
            .await
            .unwrap();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "talking", "isCancel": true })),
            )
            .await
            .unwrap();
        assert_eq!(plugin.get_calls("device1").len(), 1);
    }

    /// The cancel still reaches subscribers so a UI can close its banner,
    /// the way kde closes m_currentCallNotification (telephonyplugin.cpp:74-78).
    #[tokio::test]
    async fn test_cancel_is_broadcast_with_flag_set() {
        let (plugin, broadcaster) = setup();
        let mut rx = broadcaster.subscribe();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({ "event": "ringing", "isCancel": "true" })),
            )
            .await
            .unwrap();
        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            PluginEvent::TelephonyUpdate { device_id, info } => {
                assert_eq!(device_id, "device1");
                assert!(info.is_cancel);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_is_cancel_value_parsing() {
        assert!(is_cancel_value(&serde_json::json!(true)));
        assert!(is_cancel_value(&serde_json::json!("true")));
        assert!(is_cancel_value(&serde_json::json!("TRUE")));
        assert!(!is_cancel_value(&serde_json::json!(false)));
        assert!(!is_cancel_value(&serde_json::json!("false")));
        assert!(!is_cancel_value(&serde_json::json!(1)));
        assert!(!is_cancel_value(&serde_json::json!(null)));
    }

    /// Base64 JPEG of the contact photo (TelephonyPlugin.kt:87,
    /// telephonyplugin.cpp:25).
    #[tokio::test]
    async fn test_phone_thumbnail_captured() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({
                    "event": "ringing",
                    "phoneNumber": "+1234567890",
                    "phoneThumbnail": "/9j/4AAQSkZJRg=="
                })),
            )
            .await
            .unwrap();
        assert_eq!(
            plugin.get_calls("device1")[0].phone_thumbnail.as_deref(),
            Some("/9j/4AAQSkZJRg==")
        );
    }

    /// phoneNumber is peer-controlled. Masking used to slice bytes, which
    /// panics when the last four bytes split a multi-byte character.
    #[tokio::test]
    async fn test_multibyte_phone_number_does_not_panic() {
        let (plugin, _) = setup();
        assert!(plugin
            .handle_packet(
                "device1",
                telephony_packet(serde_json::json!({
                    "event": "ringing",
                    "phoneNumber": "☎☎☎☎☎"
                })),
            )
            .await
            .is_ok());
        assert_eq!(plugin.get_calls("device1").len(), 1);
    }

    /// B5 (2026-09-02 audit): the per-device call log grew without bound
    /// for the life of a connection, each entry carrying a base64
    /// thumbnail. Oldest entries are evicted past `MAX_CALLS_PER_DEVICE`.
    #[tokio::test]
    async fn test_call_log_is_capped_per_device() {
        let (plugin, _broadcaster) = setup();
        for i in 0..(MAX_CALLS_PER_DEVICE + 25) {
            let packet = Packet::new(
                "kdeconnect.telephony".to_string(),
                serde_json::json!({
                    "event": "missedCall",
                    "phoneNumber": format!("+1555{i:07}"),
                    "contactName": format!("caller {i}")
                }),
            );
            plugin
                .handle_packet("phone1", packet)
                .await
                .expect("handle");
        }
        let calls = plugin.get_calls("phone1");
        assert_eq!(calls.len(), MAX_CALLS_PER_DEVICE, "call log must be capped");
        assert_eq!(
            calls.last().and_then(|c| c.contact_name.as_deref()),
            Some(format!("caller {}", MAX_CALLS_PER_DEVICE + 24).as_str()),
            "the newest call must survive eviction"
        );
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_calls() {
        let (plugin, _) = setup();
        plugin
            .handle_packet(
                "device1",
                telephony_packet(
                    serde_json::json!({ "event": "ringing", "phoneNumber": "+1234567890" }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(plugin.get_calls("device1").len(), 1);
        plugin.on_disconnected("device1").await;
        assert!(plugin.get_calls("device1").is_empty());
    }

    #[tokio::test]
    async fn test_telephony_info_defaults() {
        let info: TelephonyInfo = serde_json::from_value(serde_json::json!({ "event": "ringing" }))
            .expect("Value expected to be present");
        assert_eq!(info.event, "ringing");
        assert!(info.phone_number.is_none());
        assert!(info.contact_name.is_none());
        assert!(info.phone_thumbnail.is_none());
        assert!(!info.is_cancel);
    }
}

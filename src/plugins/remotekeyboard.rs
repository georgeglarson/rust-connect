//! Remote Keyboard plugin
//!
//! Single Responsibility: Send keyboard events to the connected device and
//! receive keyboard state and echo events.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::plugins::plugin::Plugin;
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

pub struct RemoteKeyboardPlugin {
    plugin_events: Arc<PluginEventBroadcaster>,
}

/// Echo of a keypress the peer applied.
///
/// kdeconnect-android .../remotekeyboard/RemoteKeyboardPlugin.java:383-395
/// builds it: `key` always, `specialKey`/`shift`/`ctrl`/`alt` only when the
/// request carried them, and `isAck` = true on EVERY echo (:394).
/// kdeconnect-kde plugins/remotekeyboard/remotekeyboardplugin.cpp:64-67 DROPS
/// an echo missing either `isAck` or `key`, and reads `super` at :74 — a
/// modifier Android never sends but a kde peer does (:92).
///
/// Every field is optional or defaulted so a malformed echo still
/// deserializes and can be reported rather than silently swallowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteKeyboardEcho {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub is_ack: Option<bool>,
    #[serde(default)]
    pub special_key: i32,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    /// `super` is a Rust keyword; the wire key is plain "super".
    #[serde(rename = "super", default)]
    pub super_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteKeyboardState {
    pub state: bool,
}

impl RemoteKeyboardPlugin {
    pub fn new(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self { plugin_events }
    }
}

#[async_trait::async_trait]
impl Plugin for RemoteKeyboardPlugin {
    fn name(&self) -> &str {
        "remotekeyboard"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.mousepad.echo".to_string(),
            "kdeconnect.mousepad.keyboardstate".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.mousepad.request".to_string()]
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        match packet.packet_type.as_str() {
            "kdeconnect.mousepad.echo" => {
                match serde_json::from_value::<RemoteKeyboardEcho>(packet.body.clone()) {
                    Ok(echo) => {
                        if echo.key.is_none() || echo.is_ack.is_none() {
                            // kde drops these outright
                            // (remotekeyboardplugin.cpp:64-67). We forward
                            // them with a warning instead, so a peer sending
                            // malformed echoes shows up in the logs rather
                            // than vanishing.
                            warn!(
                                device_id = %device_id,
                                has_key = echo.key.is_some(),
                                has_is_ack = echo.is_ack.is_some(),
                                event = "remotekeyboard_echo_incomplete",
                                "Echo packet missing isAck and/or key"
                            );
                        }
                        self.plugin_events
                            .broadcast(PluginEvent::RemoteKeyboardEcho {
                                device_id: device_id.to_string(),
                                key: echo.key,
                                is_ack: echo.is_ack.unwrap_or(false),
                                special_key: echo.special_key,
                                shift: echo.shift,
                                ctrl: echo.ctrl,
                                alt: echo.alt,
                                super_key: echo.super_key,
                            });
                    }
                    Err(e) => {
                        warn!(
                            device_id = %device_id,
                            error = %e,
                            event = "remotekeyboard_echo_parse_failed",
                            "Malformed mousepad echo packet"
                        );
                    }
                }
            }
            "kdeconnect.mousepad.keyboardstate" => {
                if let Ok(state) =
                    serde_json::from_value::<RemoteKeyboardState>(packet.body.clone())
                {
                    self.plugin_events
                        .broadcast(PluginEvent::RemoteKeyboardState {
                            device_id: device_id.to_string(),
                            state: state.state,
                        });
                }
            }
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn setup() -> (RemoteKeyboardPlugin, Arc<PluginEventBroadcaster>) {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        (RemoteKeyboardPlugin::new(broadcaster.clone()), broadcaster)
    }

    fn echo_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.mousepad.echo".to_string(), body)
    }

    /// EXACT echo Android replies with when the request carried sendAck
    /// (kdeconnect-android .../remotekeyboard/RemoteKeyboardPlugin.java:383-395:
    /// key always, the modifiers only when present, and isAck on every echo
    /// at :394).
    #[tokio::test]
    async fn test_android_echo_wire_shape() {
        let (plugin, broadcaster) = setup();
        let mut rx = broadcaster.subscribe();
        plugin
            .handle_packet(
                "device1",
                echo_packet(serde_json::json!({
                    "key": "a",
                    "specialKey": 0,
                    "shift": false,
                    "ctrl": true,
                    "alt": false,
                    "isAck": true
                })),
            )
            .await
            .unwrap();

        match rx.recv().await.expect("Value expected to be present") {
            PluginEvent::RemoteKeyboardEcho {
                device_id,
                key,
                is_ack,
                ctrl,
                super_key,
                ..
            } => {
                assert_eq!(device_id, "device1");
                assert_eq!(key.as_deref(), Some("a"));
                assert!(is_ack);
                assert!(ctrl);
                assert!(!super_key);
            }
            _ => panic!("Wrong event type"),
        }
    }

    /// kde DROPS an echo missing isAck or key
    /// (kdeconnect-kde plugins/remotekeyboard/remotekeyboardplugin.cpp:64-67).
    /// We surface it instead of swallowing it, so a malformed peer is visible.
    #[tokio::test]
    async fn test_echo_without_key_is_not_swallowed() {
        let (plugin, broadcaster) = setup();
        let mut rx = broadcaster.subscribe();
        plugin
            .handle_packet("device1", echo_packet(serde_json::json!({ "isAck": true })))
            .await
            .unwrap();

        match rx.recv().await.expect("Value expected to be present") {
            PluginEvent::RemoteKeyboardEcho { key, is_ack, .. } => {
                assert!(key.is_none());
                assert!(is_ack);
            }
            _ => panic!("Wrong event type"),
        }
    }

    /// A kde peer sends the `super` modifier (remotekeyboardplugin.cpp:92) and
    /// reads it back at :74. Android never sends it, hence the default.
    #[tokio::test]
    async fn test_kde_super_modifier_parsed() {
        let (plugin, broadcaster) = setup();
        let mut rx = broadcaster.subscribe();
        plugin
            .handle_packet(
                "device1",
                echo_packet(serde_json::json!({
                    "key": "e",
                    "isAck": true,
                    "super": true
                })),
            )
            .await
            .unwrap();

        match rx.recv().await.expect("Value expected to be present") {
            PluginEvent::RemoteKeyboardEcho { super_key, .. } => assert!(super_key),
            _ => panic!("Wrong event type"),
        }
    }

    /// Missing modifiers default to false; specialKey defaults to 0, matching
    /// kde's np.get<...>(key, default) reads at remotekeyboardplugin.cpp:70-74.
    #[tokio::test]
    async fn test_echo_modifier_defaults() {
        let (plugin, broadcaster) = setup();
        let mut rx = broadcaster.subscribe();
        plugin
            .handle_packet(
                "device1",
                echo_packet(serde_json::json!({ "key": "z", "isAck": true })),
            )
            .await
            .unwrap();

        match rx.recv().await.expect("Value expected to be present") {
            PluginEvent::RemoteKeyboardEcho {
                special_key,
                shift,
                ctrl,
                alt,
                super_key,
                ..
            } => {
                assert_eq!(special_key, 0);
                assert!(!shift);
                assert!(!ctrl);
                assert!(!alt);
                assert!(!super_key);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_keyboardstate_still_parsed() {
        let (plugin, broadcaster) = setup();
        let mut rx = broadcaster.subscribe();
        plugin
            .handle_packet(
                "device1",
                Packet::new(
                    "kdeconnect.mousepad.keyboardstate".to_string(),
                    serde_json::json!({ "state": true }),
                ),
            )
            .await
            .unwrap();

        match rx.recv().await.expect("Value expected to be present") {
            PluginEvent::RemoteKeyboardState { state, .. } => assert!(state),
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_remotekeyboard_capabilities() {
        let (plugin, _) = setup();
        assert_eq!(plugin.name(), "remotekeyboard");
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.mousepad.echo".to_string()));
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.mousepad.request".to_string()));
    }
}

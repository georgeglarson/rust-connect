//! Plugin events
//!
//! Single Responsibility: Define plugin-level event types and provide a broadcaster.

use serde::{Deserialize, Serialize};

use crate::device::events::Broadcaster;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginEvent {
    Notification {
        device_id: String,
        id: String,
        app_name: String,
        title: String,
        text: String,
        ticker: Option<String>,
        is_cancel: bool,
        silent: bool,
        actions: Option<Vec<String>>,
        conversation: Option<serde_json::Value>,
        group_name: Option<String>,
        reply_id: Option<String>,
    },
    Battery {
        device_id: String,
        current_charge: i32,
        is_charging: bool,
    },
    MprisUpdate {
        device_id: String,
        info: crate::plugins::mpris::MprisInfo,
    },
    TelephonyUpdate {
        device_id: String,
        info: crate::plugins::telephony::TelephonyInfo,
    },
    ClipboardUpdate {
        device_id: String,
        content: String,
    },
    SftpUpdate {
        device_id: String,
        ip: String,
        port: u16,
        user: String,
        path: String,
        available: bool,
        /// True when the device's filesystem is currently mounted at
        /// `mount_point`. Updated on every mount/unmount/rotation so SSE
        /// listeners can drive a live indicator.
        mounted: bool,
        /// Server-determined path; `None` when nothing is mounted.
        mount_point: Option<String>,
    },
    RemoteKeyboardEcho {
        device_id: String,
        /// Absent when the peer sent a malformed echo. kdeconnect-kde drops
        /// those (plugins/remotekeyboard/remotekeyboardplugin.cpp:64-67); we
        /// surface them so the malformation is visible.
        key: Option<String>,
        /// Android sets this on every echo
        /// (.../remotekeyboard/RemoteKeyboardPlugin.java:394).
        is_ack: bool,
        special_key: i32,
        shift: bool,
        ctrl: bool,
        alt: bool,
        /// Wire key is "super" (remotekeyboardplugin.cpp:74 reads it,
        /// :92 sends it). Android never sends it.
        super_key: bool,
    },
    RemoteKeyboardState {
        device_id: String,
        state: bool,
    },
    RemoteCommandsUpdate {
        device_id: String,
        commands: std::collections::HashMap<String, crate::plugins::remotecommands::RemoteCommand>,
    },
    ShareText {
        device_id: String,
        text: String,
        path: String,
    },
    ShareUrl {
        device_id: String,
        url: String,
        opened: bool,
    },
    ShareProgress {
        device_id: String,
        number_of_files: u32,
        total_payload_size: u64,
    },
    /// Local sink state refresh from the PulseAudio/PipeWire backend.
    /// `device_id` is "local" — these are the sinks WE expose, not a
    /// peer's. Mirrors the broadcast shape of other local-only events
    /// (clipboard-desktop-update).
    SystemVolumeUpdate {
        sinks: Vec<crate::plugins::systemvolume::SinkState>,
    },
}

/// Plugin event broadcaster (type alias for backward compatibility).
pub type PluginEventBroadcaster = Broadcaster<PluginEvent>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn make_broadcaster() -> PluginEventBroadcaster {
        PluginEventBroadcaster::new(16, "plugin")
    }

    #[tokio::test]
    async fn test_broadcast_notification() {
        let broadcaster = make_broadcaster();
        let mut rx = broadcaster.subscribe();

        broadcaster.broadcast(PluginEvent::Notification {
            device_id: "phone-1".to_string(),
            id: "notif-123".to_string(),
            app_name: "WhatsApp".to_string(),
            title: "John".to_string(),
            text: "Hey".to_string(),
            ticker: None,
            is_cancel: false,
            silent: false,
            actions: None,
            conversation: None,
            group_name: None,
            reply_id: None,
        });

        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            PluginEvent::Notification { app_name, .. } => {
                assert_eq!(app_name, "WhatsApp");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_broadcast_battery() {
        let broadcaster = make_broadcaster();
        let mut rx = broadcaster.subscribe();

        broadcaster.broadcast(PluginEvent::Battery {
            device_id: "phone-1".to_string(),
            current_charge: 85,
            is_charging: true,
        });

        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            PluginEvent::Battery { current_charge, .. } => {
                assert_eq!(current_charge, 85);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_serialization_roundtrip() {
        let event = PluginEvent::Notification {
            device_id: "phone-1".to_string(),
            id: "notif-123".to_string(),
            app_name: "WhatsApp".to_string(),
            title: "John".to_string(),
            text: "Hey".to_string(),
            ticker: Some("New message".to_string()),
            is_cancel: false,
            silent: false,
            actions: Some(vec!["Reply".to_string()]),
            conversation: None,
            group_name: None,
            reply_id: Some("reply-123".to_string()),
        };
        let json = serde_json::to_string(&event).expect("Serialization of known types cannot fail");
        let parsed: PluginEvent =
            serde_json::from_str(&json).expect("Value expected to be present");
        match parsed {
            PluginEvent::Notification { app_name, text, .. } => {
                assert_eq!(app_name, "WhatsApp");
                assert_eq!(text, "Hey");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_no_subscribers_doesnt_panic() {
        let broadcaster = make_broadcaster();
        broadcaster.broadcast(PluginEvent::Battery {
            device_id: "x".to_string(),
            current_charge: 50,
            is_charging: false,
        });
    }

    #[tokio::test]
    async fn test_broadcast_share_text() {
        let broadcaster = make_broadcaster();
        let mut rx = broadcaster.subscribe();

        broadcaster.broadcast(PluginEvent::ShareText {
            device_id: "phone-1".to_string(),
            text: "some shared paragraph".to_string(),
            path: "/home/u/Downloads/kdeconnect-0a1b2c3d.txt".to_string(),
        });

        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            PluginEvent::ShareText { text, path, .. } => {
                assert_eq!(text, "some shared paragraph");
                assert!(path.ends_with(".txt"));
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_share_events_serialize_with_type_tag() {
        let url_event = PluginEvent::ShareUrl {
            device_id: "phone-1".to_string(),
            url: "https://kde.org/".to_string(),
            opened: true,
        };
        let json =
            serde_json::to_string(&url_event).expect("Serialization of known types cannot fail");
        assert!(json.contains("\"type\":\"ShareUrl\""), "{json}");
        assert!(json.contains("\"opened\":true"), "{json}");

        let progress = PluginEvent::ShareProgress {
            device_id: "phone-1".to_string(),
            number_of_files: 3,
            total_payload_size: 123_456,
        };
        let json =
            serde_json::to_string(&progress).expect("Serialization of known types cannot fail");
        assert!(json.contains("\"type\":\"ShareProgress\""), "{json}");
        assert!(json.contains("\"number_of_files\":3"), "{json}");
    }

    #[tokio::test]
    async fn test_broadcast_mpris_update() {
        let broadcaster = make_broadcaster();
        let mut rx = broadcaster.subscribe();

        let mpris_info = crate::plugins::mpris::MprisInfo {
            player: Some("vlc".to_string()),
            player_list: None,
            title: Some("Test Song".to_string()),
            artist: Some("Test Artist".to_string()),
            album: None,
            length: Some(240000),
            position: Some(60000),
            can_play: true,
            can_go_next: false,
            can_go_previous: false,
            is_playing: true,
            volume: Some(0.8),
            can_seek: Some(true),
            loop_status: None,
            shuffle: None,
        };

        broadcaster.broadcast(PluginEvent::MprisUpdate {
            device_id: "phone-1".to_string(),
            info: mpris_info.clone(),
        });

        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            PluginEvent::MprisUpdate { device_id, info } => {
                assert_eq!(device_id, "phone-1");
                assert_eq!(info.player.as_deref(), Some("vlc"));
                assert_eq!(info.title, Some("Test Song".to_string()));
                assert_eq!(info.volume, Some(0.8));
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_broadcast_telephony_update() {
        let broadcaster = make_broadcaster();
        let mut rx = broadcaster.subscribe();

        let telephony_info = crate::plugins::telephony::TelephonyInfo {
            event: "ringing".to_string(),
            phone_number: Some("+1234567890".to_string()),
            contact_name: Some("John".to_string()),
            phone_thumbnail: None,
            is_cancel: false,
        };

        broadcaster.broadcast(PluginEvent::TelephonyUpdate {
            device_id: "phone-1".to_string(),
            info: telephony_info,
        });

        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            PluginEvent::TelephonyUpdate { device_id, info } => {
                assert_eq!(device_id, "phone-1");
                assert_eq!(info.phone_number, Some("+1234567890".to_string()));
                assert_eq!(info.contact_name, Some("John".to_string()));
            }
            _ => panic!("Wrong event type"),
        }
    }
}

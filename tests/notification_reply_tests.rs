//! Reply-handle capture from the notification wire format.
//!
//! This file previously fed the plugin a `replyUuid` field and asserted we
//! captured it. Android has never sent that field. The test agreed with the
//! implementation's own misreading, passed from the day it was written, and
//! made a defect invisible: `replyId` was null for every notification the
//! daemon ever received, so notification reply could not work at all.
//!
//! The real field is `requestReplyId` — `NotificationsPlugin.kt:261`,
//! `np["requestReplyId"] = rn.id` — and the phone reads the same key back off
//! `kdeconnect.notification.reply` at :534-535. `rn.id` is a handle the phone
//! mints per repliable notification and keys its `pendingIntents` map by; it is
//! not the notification id.
//!
//! Fixtures here are copied from that upstream source, not from our own structs.

use anyhow::Context;
use rust_connect::plugins::events::{PluginEvent, PluginEventBroadcaster};
use rust_connect::plugins::notification::NotificationPlugin;
use rust_connect::plugins::plugin::Plugin;
use rust_connect::protocol::types::Packet;
use std::sync::Arc;

#[tokio::test]
async fn test_reply_id_captured_from_request_reply_id() -> anyhow::Result<()> {
    // Fixture: tests/fixtures/upstream-wire/notification/reply_id_request.json
    //   kdeconnect-android@a88f6fa0 NotificationsPlugin.kt:251-262
    // The fixture carries the upstream wire literal: a notification with
    // `requestReplyId` set — the field kdeconnect-android actually writes.
    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/upstream-wire/notification/reply_id_request.json");
    let fixture_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&fixture_path).expect("read reply-id fixture"),
    )
    .expect("parse fixture")["body"]
        .clone();
    let packet = Packet::new("kdeconnect.notification".to_string(), fixture_body);

    let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
    let mut rx = broadcaster.subscribe();
    let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

    assert!(plugin.handle_packet("device-1", packet).await.is_ok());

    let event = rx.recv().await.context("Value expected to be present")?;
    match event {
        PluginEvent::Notification { reply_id, .. } => {
            assert_eq!(reply_id, Some("uuid-1234".to_string()));
        }
        _ => panic!("Wrong event type"),
    }

    let history = plugin.get_history(None, 1);
    assert_eq!(history[0].reply_id, Some("uuid-1234".to_string()));
    Ok(())
}

/// `replyUuid` is not a KDE Connect field. If a future change makes the plugin
/// accept it again, that is a regression back to the original defect: the
/// implementation would once more be reading a name of its own invention.
/// Fixture: tests/fixtures/upstream-wire/notification/reply_uuid_negative.json
///   — the invented field is recorded here for the record even though no
///   upstream peer ever sends it.
#[tokio::test]
async fn test_invented_reply_uuid_field_is_not_accepted() {
    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/upstream-wire/notification/reply_uuid_negative.json");
    let fixture_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&fixture_path).expect("read reply-uuid fixture"),
    )
    .expect("parse fixture")["body"]
        .clone();
    let packet = Packet::new("kdeconnect.notification".to_string(), fixture_body);

    let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
    let plugin = NotificationPlugin::new_without_desktop(broadcaster);

    assert!(plugin.handle_packet("device-1", packet).await.is_ok());

    let history = plugin.get_history(None, 1);
    assert_eq!(
        history[0].reply_id, None,
        "replyUuid is not a wire field; accepting it would restore the original bug"
    );
}

//! SMS plugin
//!
//! Single Responsibility: Handle kdeconnect.sms packets.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use tracing::{info, trace};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmsAddress {
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SmsMessage {
    #[serde(rename = "messageID")]
    pub message_id: Option<i64>,
    #[serde(rename = "threadID", default)]
    pub thread_id: Option<i64>,
    #[serde(default)]
    pub addresses: Vec<SmsAddress>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub date: Option<i64>,
    #[serde(rename = "type", default)]
    pub message_type: Option<i32>,
    #[serde(default)]
    pub read: Option<i32>,
    #[serde(rename = "uID")]
    pub uid: Option<i64>,
    #[serde(default)]
    pub event: Option<i32>,
    #[serde(rename = "subscriptionID")]
    pub subscription_id: Option<i32>,
    #[serde(rename = "attachments")]
    pub attachments: Option<Vec<serde_json::Value>>,
}

type ThreadKey = (String, i64);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmsThread {
    pub thread_id: i64,
    pub read_count: usize,
    pub addresses: Vec<SmsAddress>,
    pub name: Option<String>,
    pub snippet: Option<String>,
}

type SmsThreadStore = HashMap<ThreadKey, Vec<SmsMessage>>;

/// Caps on peer-fed SMS state (2026-09-02 audit, B5): thread ids are
/// peer-controlled, so both the thread count per device and the messages
/// per thread are bounded, oldest out.
pub const MAX_SMS_THREADS_PER_DEVICE: usize = 200;
pub const MAX_SMS_MESSAGES_PER_THREAD: usize = 500;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmsMessagePayload {
    #[serde(default)]
    pub version: Option<i32>,
    #[serde(default)]
    pub messages: Vec<SmsMessage>,
}

pub struct SmsPlugin {
    threads: Arc<StdRwLock<SmsThreadStore>>,
}

impl Default for SmsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SmsPlugin {
    pub fn new() -> Self {
        Self {
            threads: Arc::new(StdRwLock::new(HashMap::new())),
        }
    }

    #[allow(clippy::expect_used)]
    pub fn get_threads(&self, device_id: &str) -> Vec<SmsThread> {
        let threads = self.threads.read().unwrap_or_else(|e| e.into_inner());
        threads
            .iter()
            .filter(|((did, _), _)| did == device_id)
            .map(|((_, thread_id), msgs)| {
                let read_count = msgs.len();
                let addresses = msgs
                    .first()
                    .map(|m| m.addresses.clone())
                    .unwrap_or_default();
                let snippet = msgs.last().and_then(|m| m.body.clone());
                SmsThread {
                    thread_id: *thread_id,
                    read_count,
                    addresses,
                    name: None,
                    snippet,
                }
            })
            .collect()
    }

    #[allow(clippy::expect_used)]
    pub fn get_thread(&self, device_id: &str, thread_id: i64) -> Vec<SmsMessage> {
        let threads = self.threads.read().unwrap_or_else(|e| e.into_inner());
        threads
            .get(&(device_id.to_string(), thread_id))
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Plugin for SmsPlugin {
    fn name(&self) -> &str {
        "sms"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.sms.messages".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.sms.request".to_string(),
            "kdeconnect.sms.request_conversations".to_string(),
        ]
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        vec![Packet::new(
            "kdeconnect.sms.request_conversations".to_string(),
            serde_json::json!({}),
        )]
    }

    async fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut threads) = self.threads.write() {
            threads.retain(|(did, _), _| did != device_id);
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let messages: Vec<SmsMessage> = match packet.body_as("messages payload") {
            Ok(msgs) => msgs,
            Err(_) => {
                let payload: SmsMessagePayload = packet.body_as("messages payload")?;
                payload.messages
            }
        };

        info!(
            device_id = %device_id,
            count = messages.len(),
            event = "sms_batch_received",
            "Received SMS message batch"
        );

        if let Ok(mut threads) = self.threads.write() {
            for msg in messages {
                trace!(
                    thread_id = msg.thread_id.unwrap_or(0),
                    event = "sms_received",
                    "Received individual SMS message"
                );
                let key = (device_id.to_string(), msg.thread_id.unwrap_or(0));
                if !threads.contains_key(&key) {
                    // New thread: evict this device's oldest thread (by its
                    // newest message date) once the per-device cap is hit.
                    let device_threads = threads.iter().filter(|(k, _)| k.0 == device_id).count();
                    if device_threads >= MAX_SMS_THREADS_PER_DEVICE {
                        if let Some(oldest) = threads
                            .iter()
                            .filter(|(k, _)| k.0 == device_id)
                            .min_by_key(|(_, msgs)| {
                                msgs.iter().filter_map(|m| m.date).max().unwrap_or(0)
                            })
                            .map(|(k, _)| k.clone())
                        {
                            threads.remove(&oldest);
                        }
                    }
                }
                let thread = threads.entry(key).or_default();
                thread.push(msg);
                if thread.len() > MAX_SMS_MESSAGES_PER_THREAD {
                    let excess = thread.len() - MAX_SMS_MESSAGES_PER_THREAD;
                    thread.drain(..excess);
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
    async fn test_sms_plugin() {
        let plugin = SmsPlugin::new();
        assert_eq!(plugin.name(), "sms");
    }

    #[tokio::test]
    async fn test_handle_sms_messages_stores_data() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": 1,
                "threadID": 100,
                "addresses": [{"address": "+1234567890"}],
                "body": "Hello",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0
            }]}),
        );
        plugin
            .handle_packet("test", packet)
            .await
            .expect("Value expected to be present");

        let thread = plugin.get_thread("test", 100);
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].body.as_deref(), Some("Hello"));
    }

    #[tokio::test]
    async fn test_empty_threads() {
        let plugin = SmsPlugin::new();
        assert!(plugin.get_threads("test").is_empty());
        assert!(plugin.get_thread("test", 999).is_empty());
    }

    #[tokio::test]
    async fn test_handle_malformed_sms_packet() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": "not an array"}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_sms_missing_fields() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{"body": "incomplete"}]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sms_outgoing_capabilities() {
        let plugin = SmsPlugin::new();
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.sms.request".to_string()));
    }

    fn sms_packet(thread_id: i64, message_id: i64) -> Packet {
        Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": message_id,
                "threadID": thread_id,
                "addresses": [{"address": "+1234567890"}],
                "body": format!("m{message_id}"),
                "date": 1700000000000i64 + message_id,
                "type": 1,
                "read": 0
            }]}),
        )
    }

    /// B5 (2026-09-02 audit): thread ids are peer-controlled and the
    /// store grew without bound for the life of a connection. Thread count
    /// per device and messages per thread are both capped, oldest out.
    #[tokio::test]
    async fn test_sms_threads_per_device_are_capped() {
        let plugin = SmsPlugin::new();
        for t in 0..(MAX_SMS_THREADS_PER_DEVICE as i64 + 25) {
            plugin
                .handle_packet("phone1", sms_packet(t, t))
                .await
                .expect("handle");
        }
        let threads = plugin.get_threads("phone1");
        assert_eq!(
            threads.len(),
            MAX_SMS_THREADS_PER_DEVICE,
            "thread count must be capped"
        );
        assert!(
            !plugin
                .get_thread("phone1", MAX_SMS_THREADS_PER_DEVICE as i64 + 24)
                .is_empty(),
            "the newest thread must survive eviction"
        );
    }

    #[tokio::test]
    async fn test_sms_messages_per_thread_are_capped() {
        let plugin = SmsPlugin::new();
        for m in 0..(MAX_SMS_MESSAGES_PER_THREAD as i64 + 25) {
            plugin
                .handle_packet("phone1", sms_packet(7, m))
                .await
                .expect("handle");
        }
        let msgs = plugin.get_thread("phone1", 7);
        assert_eq!(
            msgs.len(),
            MAX_SMS_MESSAGES_PER_THREAD,
            "messages per thread must be capped"
        );
        assert_eq!(
            msgs.last().and_then(|m| m.body.as_deref()),
            Some(format!("m{}", MAX_SMS_MESSAGES_PER_THREAD as i64 + 24).as_str()),
            "the newest message must survive eviction"
        );
    }

    #[tokio::test]
    async fn test_on_disconnected_cleans_up() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": 1,
                "threadID": 100,
                "addresses": [{"address": "+1234567890"}],
                "body": "Hello",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0
            }]}),
        );
        plugin
            .handle_packet("phone-1", packet)
            .await
            .expect("Value expected to be present");
        assert!(!plugin.get_threads("phone-1").is_empty());

        plugin.on_disconnected("phone-1").await;
        assert!(plugin.get_threads("phone-1").is_empty());
    }

    #[tokio::test]
    async fn test_on_disconnected_keeps_other_devices() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": 1,
                "threadID": 100,
                "addresses": [{"address": "+1234567890"}],
                "body": "Hello",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0
            }]}),
        );
        plugin
            .handle_packet("phone-1", packet.clone())
            .await
            .expect("Value expected to be present");
        plugin
            .handle_packet("phone-2", packet)
            .await
            .expect("Value expected to be present");

        plugin.on_disconnected("phone-1").await;
        assert!(plugin.get_threads("phone-1").is_empty());
        assert!(!plugin.get_threads("phone-2").is_empty());
    }

    // =====================================================================
    // TESTS FROM PROTOCOL REFERENCE (kdeconnect-android SMSHelper.kt)
    // These tests use the ACTUAL format from the Android implementation
    // =====================================================================

    #[tokio::test]
    async fn test_handle_sms_with_addresses_array_from_android() {
        // Upstream wire literal — see fixture provenance
        // tests/fixtures/upstream-wire/provenance.yaml: sms/message_batch.json
        // cited against kdeconnect-android SMSHelper.kt:911-933
        // (Message.toJSONObject() emits the {addresses, body, date, type,
        // read, threadID, uID, event, subscriptionID} shape).
        let plugin = SmsPlugin::new();

        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/sms/message_batch.json"),
            )
            .expect("sms/message_batch.json"),
        )
        .expect("sms/message_batch.json parses");

        let packet = Packet::new("kdeconnect.sms.messages".to_string(), body);
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle addresses array from Android"
        );

        let thread = plugin.get_thread("test", 100);
        assert_eq!(thread.len(), 1, "Should have stored the message");
        assert_eq!(thread[0].body.as_deref(), Some("Hello from Android"));
    }

    #[tokio::test]
    async fn test_handle_sms_with_read_as_int() {
        // kdeconnect-android sends "read" as int, not bool
        // See SMSHelper.kt line 921: json.put(READ, read) where read is Int
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [{"address": "+1234567890"}],
                "body": "Test message",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0,  // int from Android, not bool
                "threadID": 100,
                "uID": 1,
                "event": 0
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle read as int from Android"
        );
    }

    #[tokio::test]
    async fn test_handle_sms_multiple_addresses() {
        // Group messages can have multiple addresses in the array
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [
                    {"address": "+1234567890"},
                    {"address": "+0987654321"}
                ],
                "body": "Group message",
                "date": 1700000000000i64,
                "type": 2,
                "read": 1,
                "threadID": 200,
                "uID": 2,
                "event": 0
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle multiple addresses"
        );

        let thread = plugin.get_thread("test", 200);
        assert_eq!(thread.len(), 1);
    }

    #[tokio::test]
    async fn test_handle_sms_with_event_flags() {
        // Android sends event flags - should not cause parse error
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [{"address": "+1234567890"}],
                "body": "Incoming SMS",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0,
                "threadID": 300,
                "uID": 3,
                "event": 1,  // EVENT_SINGLE_PART_SMS
                "subscriptionID": 1
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_ok(), "SMS plugin should handle event flags");
    }

    #[tokio::test]
    async fn test_handle_sms_without_optional_fields() {
        // Minimum required fields from Android's toJSONObject
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [{"address": "+1234567890"}],
                "body": "Minimal message",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0,
                "threadID": 400,
                "uID": 4,
                "event": 0
                // subscriptionID is optional in some cases
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle minimal message format"
        );
    }
}

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
    /// Our own lock state, as `kdeconnect.lock` carries it.
    ///
    /// Upstream's `sendState` sends `m_localLocked` (lockdeviceplugin.cpp:116).
    /// rust-connect has no session-lock backend, so our local state is
    /// definitively "not locked" — reporting anything else would be a claim we
    /// cannot back.
    fn state_packet(&self) -> Packet {
        Packet::new(
            "kdeconnect.lock".to_string(),
            serde_json::json!({ "isLocked": false }),
        )
    }

    /// The PEER's last reported lock state (not ours).
    pub async fn is_locked(&self, device_id: &str) -> Option<bool> {
        self.states.read().await.get(device_id).copied()
    }
}

#[async_trait::async_trait]
impl Plugin for LockPlugin {
    /// B4 (2026-09-02 audit): a device's last lock state must not outlive
    /// its connection or its pairing. PR #40 review: the original
    /// `try_write` silently skipped the removal while a reader held
    /// `states`; await the write lock instead so the removal always lands.
    async fn on_disconnected(&self, device_id: &str) {
        self.states.write().await.remove(device_id);
    }

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
        // kdeconnect-kde's lockdevice plugin is permissive about WHICH of its
        // two packet types carries which field — receivePacket() tests for
        // requestLocked / lockResult / setLocked on both
        // (lockdeviceplugin.cpp:77-111). Mirror that rather than binding a
        // field to one carrier and misparsing a peer that chose the other.
        match packet.packet_type.as_str() {
            "kdeconnect.lock" | "kdeconnect.lock.request" => {}
            _ => return Ok(None),
        }

        let mut replies = Vec::new();

        // Peer reporting ITS lock state. Upstream field is `isLocked`
        // (lockdeviceplugin.cpp:116, sendState). The old code read `locked`,
        // which no upstream emits, so every upstream packet parsed as false.
        if let Some(is_locked) = packet.body.get("isLocked").and_then(|v| v.as_bool()) {
            self.states
                .write()
                .await
                .insert(device_id.to_string(), is_locked);
            tracing::info!(
                device_id = %device_id, is_locked = is_locked,
                event = "lock_update", "Received lock state update"
            );
        }

        // Result of a setLocked WE sent (lockdeviceplugin.cpp:82-95). Upstream
        // raises a desktop notification; we have no notification surface for
        // it, so log at the level that makes a failed remote lock findable.
        if let Some(ok) = packet.body.get("lockResult").and_then(|v| v.as_bool()) {
            if ok {
                tracing::info!(device_id = %device_id, event = "remote_lock_result",
                               "Remote lock succeeded");
            } else {
                tracing::warn!(device_id = %device_id, event = "remote_lock_result",
                               "Remote lock FAILED on the peer");
            }
        }

        // Peer commanding US to lock/unlock (lockdeviceplugin.cpp:98-110).
        // rust-connect has no session-lock backend, so this cannot be honoured.
        // Upstream answers a lock attempt with lockResult; answering `false` is
        // both contract-faithful and true. Silence would leave the peer
        // waiting on a reply its own code expects.
        if let Some(set_locked) = packet.body.get("setLocked").and_then(|v| v.as_bool()) {
            if set_locked {
                tracing::warn!(
                    device_id = %device_id, event = "lock_unsupported",
                    "Peer asked us to lock; no session-lock backend, replying lockResult=false"
                );
                replies.push(Packet::new(
                    "kdeconnect.lock".to_string(),
                    serde_json::json!({ "lockResult": false }),
                ));
            }
            replies.push(self.state_packet());
        }

        // Peer asking for OUR state (lockdeviceplugin.cpp:77-79 -> sendState).
        // The old code answered with the PEER's last reported state, i.e. it
        // echoed their own value back at them. sendState sends m_localLocked.
        if packet.body.get("requestLocked").is_some() {
            tracing::debug!(device_id = %device_id, event = "lock_state_requested",
                            "Answering lock state request");
            replies.push(self.state_packet());
        }

        Ok(if replies.is_empty() {
            None
        } else {
            Some(replies)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    /// B4 (2026-09-02 audit): the plugin had no disconnect handler, so a
    /// device's last lock state outlived its connection and its pairing.
    #[tokio::test]
    async fn test_on_disconnected_forgets_the_device_state() {
        let plugin = LockPlugin::new();
        plugin
            .handle_packet(
                "phone1",
                Packet::new(
                    "kdeconnect.lock".to_string(),
                    serde_json::json!({ "isLocked": true }),
                ),
            )
            .await
            .expect("handle");
        assert_eq!(plugin.is_locked("phone1").await, Some(true));
        plugin.on_disconnected("phone1").await;
        assert_eq!(plugin.is_locked("phone1").await, None);
    }

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
    async fn test_request_answers_with_our_state_not_the_peers_echo() {
        // The subtler half of vk #1018. sendState sends m_localLocked
        // (lockdeviceplugin.cpp:116) — OUR state. The old reply used the
        // peer's last reported value, echoing their own number back at them,
        // which reads as correct in a one-device test and is wrong on the wire.
        let plugin = LockPlugin::new();
        plugin
            .handle_packet(
                "device1",
                Packet::new(
                    "kdeconnect.lock".to_string(),
                    serde_json::json!({"isLocked": true}),
                ),
            )
            .await
            .expect("handle");
        assert_eq!(
            plugin.is_locked("device1").await,
            Some(true),
            "peer state stored"
        );

        let reply = plugin
            .handle_packet(
                "device1",
                Packet::new(
                    "kdeconnect.lock.request".to_string(),
                    serde_json::json!({"requestLocked": serde_json::Value::Null}),
                ),
            )
            .await
            .expect("handle")
            .expect("a requestLocked must be answered");
        assert_eq!(
            reply[0].body["isLocked"], false,
            "must report OUR state (no lock backend => false), not the peer's true"
        );
    }

    #[tokio::test]
    async fn test_set_locked_is_answered_honestly_not_silently() {
        // We have no session-lock backend. Upstream's setLocked path expects a
        // lockResult (lockdeviceplugin.cpp:98-107); silence would leave the
        // peer waiting on a reply its own code handles.
        let plugin = LockPlugin::new();
        let reply = plugin
            .handle_packet(
                "device1",
                Packet::new(
                    "kdeconnect.lock.request".to_string(),
                    serde_json::json!({"setLocked": true}),
                ),
            )
            .await
            .expect("handle")
            .expect("setLocked must be answered");
        assert_eq!(reply[0].body["lockResult"], false, "we cannot lock; say so");
        assert_eq!(
            reply[1].body["isLocked"], false,
            "and follow with state, as kde does"
        );
    }

    #[tokio::test]
    async fn test_lock_result_from_peer_is_accepted_without_a_reply() {
        let plugin = LockPlugin::new();
        let out = plugin
            .handle_packet(
                "device1",
                Packet::new(
                    "kdeconnect.lock".to_string(),
                    serde_json::json!({"lockResult": true}),
                ),
            )
            .await
            .expect("handle");
        assert!(out.is_none(), "a result is terminal, not a question");
    }

    /// Was a DEFECT PIN (vk #1018): this plugin read a `locked` field no
    /// upstream emits, so the upstream shape parsed as `false`. Inverted on
    /// 2026-08-25 when the contract rewrite landed, exactly as the pin said to.
    #[tokio::test]
    async fn test_upstream_lock_state_shape_parses() {
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
        // Upstream said isLocked=true, and we now read it.
        assert_eq!(plugin.is_locked("device1").await, Some(true));
    }

    /// Was a DEFECT PIN (vk #1018): the reply body field was ours (`locked`),
    /// not upstream's `isLocked`. Inverted on 2026-08-25 with the rewrite.
    #[tokio::test]
    async fn test_lock_request_reply_uses_the_upstream_field() {
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
        assert!(
            body.contains_key("isLocked"),
            "reply must use upstream's field (lockdeviceplugin.cpp:116)"
        );
        assert!(!body.contains_key("locked"), "the divergent field is gone");
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

    /// PR #40 review (coderabbit): the B4 handler used `try_write` and
    /// silently skipped the removal while a reader held `states`
    /// (`handle_packet` / `is_locked`), leaving stale lock state after
    /// disconnect. The trait is async now — await the write lock instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_disconnected_waits_for_readers_instead_of_skipping_removal() {
        let plugin = Arc::new(LockPlugin::new());
        plugin
            .handle_packet(
                "phone1",
                Packet::new(
                    "kdeconnect.lock".to_string(),
                    serde_json::json!({ "isLocked": true }),
                ),
            )
            .await
            .expect("handle");
        assert_eq!(plugin.is_locked("phone1").await, Some(true));

        let held = plugin.states.read().await;
        let p = plugin.clone();
        let task = tokio::spawn(async move { p.on_disconnected("phone1").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !task.is_finished(),
            "on_disconnected must wait for the reader, not skip the removal"
        );
        drop(held);
        task.await
            .expect("on_disconnected completes once the reader releases");
        assert_eq!(plugin.is_locked("phone1").await, None);
    }
}

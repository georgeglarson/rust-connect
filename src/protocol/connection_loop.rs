//! Packet receive loop for established device connections
//!
//! Single Responsibility: Read packets from an established TLS connection,
//! handle pairing protocol, and route plugin packets.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, trace, warn};

use crate::app::AppState;
use crate::device::types::{DeviceEvent, DeviceId, DeviceState};
use crate::protocol::pairing::PairState;
use crate::protocol::types::Packet;
use tokio_util::sync::CancellationToken;

pub enum LoopResult {
    Shutdown,
    Disconnected,
}

fn pair_decision(packet: &Packet) -> Option<bool> {
    packet.body.get("pair").and_then(|value| value.as_bool())
}

fn pair_timestamp_is_fresh(timestamp: i64, now: i64) -> bool {
    timestamp.abs_diff(now) <= 1800
}

pub async fn run_packet_loop(
    state: Arc<AppState>,
    conn_cancel: CancellationToken,
    device_id: &DeviceId,
    generation: u64,
    idle_timeout_secs: u64,
) -> LoopResult {
    let cm = &state.connection_manager;
    let router = &state.packet_router;
    let pairing = &state.pairing_handler;
    let plugin_registry = &state.plugin_registry;
    let lifecycle = &state.lifecycle;

    let idle_check_interval = if idle_timeout_secs > 0 {
        Some(Duration::from_secs(30))
    } else {
        None
    };

    // Parity gap 1 (docs/parity-checklist.md): an unpaired peer's non-pair
    // packets are dropped, but the peer must be TOLD — kde replies
    // pair:false (device.cpp:391-394), Android auto-unpairs. Once per
    // unpaired stretch on this link (reset by any pair-packet activity),
    // not once per packet.
    let mut unpair_notified = false;

    loop {
        tokio::select! {
            result = cm.recv_packet_current(device_id, generation) => {
                match result {
                    Ok(packet) => {
                        trace!(
                            device_id = %device_id,
                            packet_type = %packet.packet_type,
                            event = "packet_loop_received",
                            "Packet loop received packet"
                        );

                        if packet.is_pair() {
                            // Pair-packet activity starts a new pairing
                            // stretch: the next unpaired stretch gets its
                            // own single pair:false notification.
                            unpair_notified = false;
                            let Some(accepted) = pair_decision(&packet) else {
                                warn!(
                                    device_id = %device_id,
                                    packet_body = ?packet.body,
                                    event = "malformed_pair_packet",
                                    "Ignoring pair packet without a boolean pair value"
                                );
                                continue;
                            };

                            debug!(
                                device_id = %device_id,
                                pair_value = accepted,
                                packet_body = ?packet.body,
                                event = "pair_packet_debug",
                                "Pair packet contents"
                            );

                            let pair_ts = packet.body.get("timestamp").and_then(|v| v.as_i64());
                            // Android (PairingHandler.kt) validates the 1800s
                            // window only on incoming pair REQUESTS from v8+
                            // peers; pair=false is processed regardless of
                            // timestamp — a clock-skewed peer's unpair must
                            // still cut the pairing relationship.

                            let pair_state = pairing.pair_state(device_id).await;
                            if accepted {
                                if pair_state == PairState::Requested {
                                    // Stage the peer cert for accept_pairing
                                    // to persist (verify-before-write: this
                                    // staging is the only source). Safe to
                                    // stage here: accept_pairing runs no
                                    // sweep — unlike receive_pair_request,
                                    // whose cleanup_expired would reap a cert
                                    // staged before it, so
                                    // the else branch stages via
                                    // receive_pair_request_with_cert instead.
                                    if let Some(cert_der) = cm.get_peer_certificate(device_id).await {
                                        pairing.set_pending_peer_cert(device_id, cert_der).await;
                                    }
                                    // We initiated; this pair=true is the peer's
                                    // accept. Android: pairingDone, no reply.
                                    if let Err(e) = pairing.accept_pairing(device_id).await {
                                        warn!(device_id = %device_id, error = %e, event = "pair_accept_failed", "Failed to complete pairing");
                                        continue;
                                    }
                                    info!(
                                        device_id = %device_id,
                                        event = "pair_accepted",
                                        "Pairing accepted by remote device"
                                    );

                                    // Spawn the advertisement off the loop: it
                                    // now waits briefly for a live link, so
                                    // awaiting it inline would stop this loop
                                    // from reading further packets for the
                                    // device during the wait. Same shape as the
                                    // connect-time path in the listener.
                                    let state_clone = state.clone();
                                    let device_id_clone = device_id.to_string();
                                    tokio::spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_millis(500))
                                            .await;
                                        state_clone
                                            .send_plugin_init_packets(&device_id_clone)
                                            .await;
                                    });
                                } else {
                                    // NotPaired / RequestedByPeer / Paired.
                                    // Paired: Android semantics — a pair request
                                    // while paired unpairs BOTH sides and starts
                                    // over (handled inside receive_pair_request).
                                    // Never answer pair:true with pair:true —
                                    // that unpairs Android.
                                    //
                                    // The v8 timestamp is required only here,
                                    // and only from protocol >= 8 peers
                                    // (PairingHandler.kt:71-83): the ACCEPT path
                                    // above takes whatever upstream
                                    // acceptPairing() sends — {"pair": true}
                                    // with no timestamp — and a v7 peer never
                                    // sends one at all. Unknown version fails
                                    // closed (require the timestamp).
                                    let protocol_version = state
                                        .registry
                                        .get(device_id)
                                        .await
                                        .map(|d| d.protocol_version)
                                        .unwrap_or(8);
                                    if protocol_version >= 8 {
                                        let Some(ts) = pair_ts else {
                                            warn!(
                                                device_id = %device_id,
                                                event = "pair_timestamp_missing",
                                                "Pair request from v8+ peer missing timestamp, rejecting"
                                            );
                                            continue;
                                        };
                                        let now = chrono::Utc::now().timestamp();
                                        if !pair_timestamp_is_fresh(ts, now) {
                                            warn!(
                                                device_id = %device_id,
                                                packet_timestamp = ts,
                                                local_timestamp = now,
                                                drift_secs = ts.abs_diff(now),
                                                event = "pair_timestamp_rejected",
                                                "Pair packet timestamp outside 30-minute window, rejecting"
                                            );
                                            continue;
                                        }
                                    }
                                    let cert_der = cm.get_peer_certificate(device_id).await;
                                    if let Err(e) = pairing.receive_pair_request_with_cert(device_id, pair_ts, cert_der).await {
                                        warn!(device_id = %device_id, error = %e, event = "pair_request_failed", "Failed to process pair request");
                                        continue;
                                    }
                                    if pairing.is_paired(device_id).await {
                                        // Mutual initiation auto-accepted: the
                                        // peer is waiting for our accept.
                                        let accept_pkt = Packet::pair_response(true);
                                        if let Err(e) = cm.send_packet(device_id, &accept_pkt).await {
                                            // Android acceptPairing
                                            // (PairingHandler.kt:181-185): the
                                            // pairing completes on send success
                                            // — a failed send must not leave us
                                            // paired while the peer isn't.
                                            warn!(device_id = %device_id, error = %e, "Failed to send pair accept, rolling back pairing");
                                            let _ = pairing.unpair(device_id).await;
                                        } else {
                                            info!(
                                                device_id = %device_id,
                                                event = "pair_mutual_accepted",
                                                "Mutual pairing auto-accepted and confirmed"
                                            );
                                            // late-pairing plugin init: pairing completed on
                                            // an already-established (and at
                                            // connect-time UNPAIRED) link, so
                                            // the orchestrator/listener
                                            // connect-time notify never fired
                                            // — plugins get their init
                                            // packets here or never.
                                            //
                                            // Spawned off the loop for the same
                                            // reason as the pair-accepted arm
                                            // above: the advertisement path now
                                            // waits briefly for a live link.
                                            let state_clone = state.clone();
                                            let device_id_clone = device_id.to_string();
                                            tokio::spawn(async move {
                                                tokio::time::sleep(
                                                    std::time::Duration::from_millis(500),
                                                )
                                                .await;
                                                state_clone
                                                    .send_plugin_init_packets(&device_id_clone)
                                                    .await;
                                            });
                                        }
                                    } else {
                                        info!(
                                            device_id = %device_id,
                                            event = "pair_request_received",
                                            "Pairing request received, awaiting user acceptance"
                                        );
                                        // Surface the pending request so the UI
                                        // refreshes inside the request's short
                                        // window instead of waiting for the next
                                        // poll. The event carries no key: clients
                                        // re-read the device over the authenticated
                                        // API, which is the single path the key
                                        // travels.
                                        let device_name = state
                                            .registry
                                            .get(device_id)
                                            .await
                                            .map(|d| d.name)
                                            .unwrap_or_else(|_| device_id.clone());
                                        state.broadcaster.broadcast(DeviceEvent::PairRequested {
                                            device_id: device_id.clone(),
                                            device_name,
                                        });
                                    }
                                }
                            } else {
                                match pair_state {
                                    PairState::Paired => {
                                        warn!(
                                            device_id = %device_id,
                                            event = "pair_rejected_unpair",
                                            "Remote device unpaired, clearing pair state and disconnecting"
                                        );
                                        if let Err(e) = pairing.unpair(device_id).await {
                                            debug!(device_id = %device_id, error = %e, "Failed to unpair after rejection");
                                        }
                                        return LoopResult::Disconnected;
                                    }
                                    // Requested: we started pairing and got
                                    // rejected. RequestedByPeer: they started,
                                    // then cancelled. Android drops the link on
                                    // both — staying connected would leave a
                                    // live link to a device that just cut the
                                    // pairing relationship.
                                    PairState::Requested | PairState::RequestedByPeer => {
                                        if let Err(e) = pairing.reject_pairing(device_id).await {
                                            debug!(device_id = %device_id, error = %e, "Failed to clear pending pair request after rejection");
                                        }
                                        info!(
                                            device_id = %device_id,
                                            event = "pair_rejected",
                                            "Pairing rejected by remote device, disconnecting"
                                        );
                                        return LoopResult::Disconnected;
                                    }
                                    // Android PairingHandler.kt:100: an unpair
                                    // for a device we never paired is ignored.
                                    PairState::NotPaired => {
                                        debug!(
                                            device_id = %device_id,
                                            event = "unpair_ignored",
                                            "Ignoring unpair request for already unpaired device"
                                        );
                                    }
                                }
                            }
                            continue;
                        }
                        if !pairing.is_paired(device_id).await {
                            debug!(
                                device_id = %device_id,
                                packet_type = %packet.packet_type,
                                event = "unpaired_packet_dropped",
                                "Dropping non-pair packet from unpaired device"
                            );
                            // Tell the peer, once (see unpair_notified
                            // above): without this the phone keeps the link
                            // and keeps believing it is paired — the unpair
                            // desync the references avoid.
                            if !unpair_notified {
                                unpair_notified = true;
                                let reject = Packet::pair_response(false);
                                if let Err(e) = cm.send_packet(device_id, &reject).await {
                                    warn!(
                                        device_id = %device_id,
                                        error = %e,
                                        event = "unpair_notify_failed",
                                        "Failed to tell an unpaired peer pair:false"
                                    );
                                } else {
                                    info!(
                                        device_id = %device_id,
                                        event = "unpaired_peer_notified",
                                        "Told an unpaired peer pair:false"
                                    );
                                }
                            }
                            continue;
                        }
                        match router.route(device_id, packet).await {
                            Ok(Some(packets)) => {
                                for p in packets {
                                    if let Err(e) = cm.send_packet(device_id, &p).await {
                                        warn!(
                                            error = %e,
                                            device_id = %device_id,
                                            packet_type = %p.packet_type,
                                            event = "plugin_reply_failed",
                                            "Failed to send plugin reply packet"
                                        );
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    device_id = %device_id,
                                    event = "route_failed",
                                    "Failed to route packet"
                                );
                            }
                        }
                    }
                    Err(crate::utils::errors::Error::ConnectionTimeout(_)) => {
                        debug!(
                            device_id = %device_id,
                            event = "recv_timeout",
                            "recv_packet timeout, keepalive will handle liveness check"
                        );
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            device_id = %device_id,
                            event = "device_disconnected",
                            "Device disconnected"
                        );
                        // Lifecycle + plugin notification only while OUR
                        // generation owns the link: disconnect returns false
                        // when a same-cert replacement holds the slot, and a
                        // stale loop must not mark the live replacement
                        // Disconnected (the F-4 stale-loop race).
                        match cm.disconnect(device_id, generation).await {
                            Ok(true) => {
                                lifecycle.try_transition(device_id, DeviceState::Disconnected).await;
                                plugin_registry.notify_disconnected(device_id).await;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                warn!(device_id = %device_id, error = %e, event = "disconnect_failed", "Failed to disconnect");
                            }
                        }
                        return LoopResult::Disconnected;
                    }
                }
            }
            _ = conn_cancel.cancelled() => {
                info!(
                    device_id = %device_id,
                    event = "connection_replaced",
                    "Connection replaced by new incoming connection"
                );
                // Generation-scoped cleanup: after a same-cert replacement
                // the NEW connection owns the cancel-token slot and the
                // lifecycle. Removing the token here would strip the
                // replacement's freshly-registered token; marking
                // Disconnected would clobber the live link's state.
                if cm.remove_cancel_token_if_current(device_id, generation).await {
                    lifecycle.try_transition(device_id, DeviceState::Disconnected).await;
                }
                return LoopResult::Disconnected;
            }
            _ = state.shutdown.cancelled() => {
                info!(
                    device_id = %device_id,
                    event = "connection_loop_shutdown",
                    "Shutting down connection loop"
                );
                match cm.disconnect(device_id, generation).await {
                    Ok(true) => {
                        lifecycle.try_transition(device_id, DeviceState::Disconnected).await;
                        plugin_registry.notify_disconnected(device_id).await;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(device_id = %device_id, error = %e, event = "shutdown_disconnect_failed", "Failed to disconnect on shutdown");
                    }
                }
                return LoopResult::Shutdown;
            }
            _ = async {
                if let Some(interval) = idle_check_interval {
                    loop {
                        tokio::time::sleep(interval).await;
                        if let Some(info) = cm.get_connection_info(device_id).await {
                            let threshold = chrono::Duration::seconds(idle_timeout_secs as i64);
                            if info.is_idle(threshold) {
                                info!(
                                    device_id = %device_id,
                                    idle_secs = (chrono::Utc::now() - info.last_activity).num_seconds(),
                                    event = "idle_timeout",
                                    "Connection idle timeout, disconnecting"
                                );
                                match cm.disconnect(device_id, generation).await {
                                    Ok(true) => {
                                        lifecycle.try_transition(device_id, DeviceState::Disconnected).await;
                                        plugin_registry.notify_disconnected(device_id).await;
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        warn!(device_id = %device_id, error = %e, event = "idle_disconnect_failed", "Failed to disconnect on idle");
                                    }
                                }
                                return LoopResult::Disconnected;
                            }
                        }
                    }
                }
                futures::future::pending().await
            } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! Packet-shape helpers only. The 1800s staleness window itself is
    //! covered by driving the REAL loop in tests/protocol_integration.rs
    //! (`test_connection_loop_stale_pair_timestamp_ignored_fresh_accepted`,
    //! `test_connection_loop_future_pair_timestamp_ignored_fresh_stored`,
    //! `test_connection_loop_pair_true_without_timestamp_rejected`) —
    //! the arithmetic-only tests that used to live here asserted `(ts -
    //! now).abs()` against a packet body and never touched the loop.
    use super::{pair_decision, pair_timestamp_is_fresh};
    use crate::protocol::types::Packet;

    #[test]
    fn malformed_pair_values_are_not_rejections() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "pair": "false" }),
            serde_json::json!({ "pair": 0 }),
            serde_json::json!({ "pair": null }),
        ] {
            let packet = Packet::new("kdeconnect.pair".to_string(), body);
            assert_eq!(pair_decision(&packet), None);
        }
    }

    #[test]
    fn pair_timestamp_distance_is_overflow_safe() {
        assert!(pair_timestamp_is_fresh(100, 1900));
        assert!(!pair_timestamp_is_fresh(100, 1901));
        assert!(!pair_timestamp_is_fresh(i64::MIN, i64::MAX));
        assert!(!pair_timestamp_is_fresh(i64::MAX, i64::MIN));
    }

    #[test]
    fn test_pair_packet_with_valid_timestamp_is_accepted() {
        let now = chrono::Utc::now().timestamp();
        let packet = Packet::new(
            "kdeconnect.pair".to_string(),
            serde_json::json!({ "pair": true, "timestamp": now }),
        );
        assert!(packet.is_pair());
        assert!(packet
            .body
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .is_some());
    }

    #[test]
    fn test_pair_packet_without_timestamp_carries_no_timestamp() {
        // Upstream acceptPairing() sends exactly this shape — {"pair": true}
        // with no timestamp — and the loop's accept path must complete on it.
        let packet = Packet::new(
            "kdeconnect.pair".to_string(),
            serde_json::json!({ "pair": true }),
        );
        assert!(packet.is_pair());
        assert!(packet.body.get("timestamp").is_none());
    }
}

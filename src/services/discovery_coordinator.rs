//! Discovery coordinator (Task 2.2, vk #994)
//!
//! Single Responsibility: react to a debounced `NetworkChanged` event
//! (`network_watcher.rs`) by re-announcing — UDP broadcast once + mDNS
//! reannounce — matching both reference implementations, which do the
//! same on start AND on network change
//! (kdeconnect-kde `lanlinkprovider.cpp:149,192`; kdeconnect-android
//! `LanLinkProvider.java:567,572-584`).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::protocol::mdns_discovery::MdnsDiscoveryService;
use crate::protocol::types::Identity;
use crate::protocol::DiscoveryService;

use super::network_watcher;

/// Spawns the network-change watcher and the task that reacts to it. The
/// returned handle covers the reactor task only — the watcher's own
/// sub-tasks (if-watch, suspend watchdog) are spawned internally by
/// `network_watcher::watch_network_changes` and share the same
/// `shutdown` token.
pub fn spawn_network_change_reactor(
    discovery: Arc<DiscoveryService>,
    mdns: Option<Arc<MdnsDiscoveryService>>,
    identity: Identity,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let watcher_shutdown = shutdown.clone();
    tokio::spawn(network_watcher::watch_network_changes(tx, watcher_shutdown));

    let reactor_shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = reactor_shutdown.cancelled() => return,
                event = rx.recv() => {
                    if event.is_none() {
                        return;
                    }
                    react_to_network_change(&discovery, mdns.as_deref(), &identity).await;
                }
            }
        }
    })
}

/// One re-announce cycle: UDP broadcast once, then mDNS reannounce if a
/// backend is running. Both are independent best-effort legs — a failure
/// in one must not skip the other, matching how every other backend-
/// bearing subsystem in this codebase degrades (log and continue, never
/// fatal).
async fn react_to_network_change(
    discovery: &DiscoveryService,
    mdns: Option<&MdnsDiscoveryService>,
    identity: &Identity,
) {
    info!(
        event = "network_change_reannounce",
        "Network change detected; re-announcing identity"
    );

    if let Err(e) = discovery.broadcast().await {
        warn!(
            error = %e,
            event = "network_change_broadcast_failed",
            "Failed to broadcast identity after a network change"
        );
    }

    if let Some(mdns) = mdns {
        if let Err(e) = mdns.reannounce(identity) {
            warn!(
                error = %e,
                event = "network_change_mdns_reannounce_failed",
                "Failed to re-announce mDNS service after a network change"
            );
        }
    }
}

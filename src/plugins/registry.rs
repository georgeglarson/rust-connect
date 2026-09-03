//! Plugin registry
//!
//! Single Responsibility: Store and look up plugins by capability.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{debug, error};

use crate::protocol::types::Packet;
use crate::utils::errors::{Error, Result};

use super::plugin::Plugin;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
}

pub struct PluginRegistry {
    plugins: Arc<RwLock<HashMap<String, Arc<dyn Plugin>>>>,
    capability_index: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
            capability_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(&self, plugin: Arc<dyn Plugin>) {
        let name = plugin.name().to_string();
        let capabilities = plugin.incoming_capabilities();

        let old_capabilities = {
            let mut plugins = self.plugins.write().await;
            debug!(plugin = %name, event = "plugin_registered", "Registered plugin");
            plugins
                .insert(name.clone(), plugin)
                .map(|old| old.incoming_capabilities())
        };

        {
            let mut index = self.capability_index.write().await;
            // Replacing a plugin under the same name: strip its OLD
            // capabilities first — otherwise the index would route packet
            // types the replacement doesn't declare.
            if let Some(old_capabilities) = old_capabilities {
                for cap in old_capabilities {
                    if let Some(names) = index.get_mut(&cap) {
                        names.retain(|n| n != &name);
                        if names.is_empty() {
                            index.remove(&cap);
                        }
                    }
                }
            }
            for cap in capabilities {
                debug!(
                    plugin = %name,
                    capability = %cap,
                    event = "capability_registered",
                    "Registered capability"
                );
                // 1:N — more than one plugin may consume a packet type
                // (telephony + pausemusic both take kdeconnect.telephony);
                // a plain insert would silently clobber the first.
                let names = index.entry(cap).or_default();
                if !names.contains(&name) {
                    names.push(name.clone());
                }
            }
        }
    }

    pub async fn unregister(&self, name: &str) -> bool {
        let mut plugins = self.plugins.write().await;
        if let Some(plugin) = plugins.remove(name) {
            let caps = plugin.incoming_capabilities();
            drop(plugins);

            let mut index = self.capability_index.write().await;
            for cap in caps {
                // Remove only THIS plugin from the capability's list;
                // dropping the whole key would strip other consumers.
                if let Some(names) = index.get_mut(&cap) {
                    names.retain(|n| n != name);
                    if names.is_empty() {
                        index.remove(&cap);
                    }
                }
            }
            debug!(plugin = %name, event = "plugin_unregistered", "Unregistered plugin");
            true
        } else {
            false
        }
    }

    /// The first plugin registered for a capability. Kept for callers that
    /// expect one; new code that needs every consumer should use
    /// [`Self::get_all_by_capability`]. NOTE: pre-1:N this was LAST-wins
    /// (a plain `HashMap::insert` clobbered the previous registration);
    /// the flip to first-wins is deliberate — registration order is the
    /// only stable order.
    /// Index names that no longer resolve (unregister's two-lock window)
    /// are skipped, same tolerance as `get_all_by_capability`.
    pub async fn get_by_capability(&self, capability: &str) -> Option<Arc<dyn Plugin>> {
        let index = self.capability_index.read().await;
        let names = index.get(capability)?;
        let plugins = self.plugins.read().await;
        names.iter().find_map(|n| plugins.get(n).cloned())
    }

    /// Every plugin registered for a capability, in registration order.
    pub async fn get_all_by_capability(&self, capability: &str) -> Vec<Arc<dyn Plugin>> {
        let index = self.capability_index.read().await;
        let Some(names) = index.get(capability) else {
            return Vec::new();
        };
        let plugins = self.plugins.read().await;
        names
            .iter()
            .filter_map(|n| plugins.get(n).cloned())
            .collect()
    }

    pub async fn get(&self, name: &str) -> Option<Arc<dyn Plugin>> {
        let plugins = self.plugins.read().await;
        plugins.get(name).cloned()
    }

    pub async fn list(&self) -> Vec<String> {
        let plugins = self.plugins.read().await;
        plugins.keys().cloned().collect()
    }

    pub async fn list_with_capabilities(&self) -> Vec<PluginInfo> {
        let plugins = self.plugins.read().await;
        plugins
            .values()
            .map(|p| PluginInfo {
                name: p.name().to_string(),
                incoming_capabilities: p.incoming_capabilities(),
                outgoing_capabilities: p.outgoing_capabilities(),
            })
            .collect()
    }

    pub async fn handle_packet(
        &self,
        device_id: &str,
        packet_type: &str,
        packet: Packet,
    ) -> Result<Option<Vec<Packet>>> {
        let plugins = self.get_all_by_capability(packet_type).await;
        if plugins.is_empty() {
            return Err(Error::NoPluginForPacketType(packet_type.to_string()));
        }

        // 1:N fan-out, mirroring the PacketRouter: every consumer of a
        // shared capability (telephony + pausemusic on kdeconnect.telephony)
        // gets the packet; responses merge; a failing plugin is logged and
        // doesn't starve its siblings, but when NO handler succeeded the
        // first error surfaces (an Ok(None) sibling still counts as
        // processed). NOT the hot path — production dispatch goes through
        // PacketRouter (app.rs init_plugins) — so this keeps a plain loop
        // and pays one packet clone per plugin for clarity (the
        // router's last-handler clone optimization is not worth
        // duplicating here).
        let mut all_responses = Vec::new();
        let mut first_error: Option<Error> = None;
        let mut any_ok = false;
        for plugin in plugins {
            match plugin.handle_packet(device_id, packet.clone()).await {
                Ok(Some(mut responses)) => {
                    any_ok = true;
                    all_responses.append(&mut responses);
                }
                Ok(None) => {
                    any_ok = true;
                }
                Err(e) => {
                    error!(
                        device_id = %device_id,
                        plugin = plugin.name(),
                        error = %e,
                        event = "plugin_error",
                        "Plugin returned error handling packet"
                    );
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }
        if !all_responses.is_empty() {
            Ok(Some(all_responses))
        } else if !any_ok {
            match first_error {
                Some(e) => Err(e),
                None => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    pub async fn notify_connected(&self, device_id: &str) -> Vec<Packet> {
        let plugins = self.plugins.read().await;
        let mut packets = Vec::new();
        for (name, plugin) in plugins.iter() {
            let mut p = plugin.on_connected(device_id);
            debug!(plugin = %name, device_id = %device_id, event = "plugin_connected", "Notified plugin of connection");
            packets.append(&mut p);
        }
        packets
    }

    pub async fn notify_disconnected(&self, device_id: &str) {
        // Snapshot the plugin Arcs and release the read guard BEFORE
        // awaiting: plugin cleanup (sftp unmounts, up to `mount_timeout`
        // each) can hold the loop for seconds, and keeping the guard
        // across the awaits blocks every register/unregister for the
        // whole wait (PR #40 review).
        let plugins: Vec<(String, Arc<dyn Plugin>)> = self
            .plugins
            .read()
            .await
            .iter()
            .map(|(name, plugin)| (name.clone(), Arc::clone(plugin)))
            .collect();
        for (name, plugin) in &plugins {
            plugin.on_disconnected(device_id).await;
            debug!(plugin = %name, device_id = %device_id, event = "plugin_disconnected", "Notified plugin of disconnection");
        }
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    struct MockPlugin {
        name: String,
        incoming: Vec<String>,
    }

    impl MockPlugin {
        fn new(name: &str, incoming: Vec<&str>) -> Self {
            Self {
                name: name.to_string(),
                incoming: incoming.into_iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Plugin for MockPlugin {
        fn name(&self) -> &str {
            &self.name
        }

        fn incoming_capabilities(&self) -> Vec<String> {
            self.incoming.clone()
        }

        fn outgoing_capabilities(&self) -> Vec<String> {
            vec![]
        }

        async fn handle_packet(
            &self,
            _device_id: &str,
            _packet: Packet,
        ) -> Result<Option<Vec<Packet>>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn test_register_and_lookup() {
        let registry = PluginRegistry::new();
        let plugin = Arc::new(MockPlugin::new("ping", vec!["kdeconnect.ping"]));
        registry.register(plugin).await;

        assert!(registry
            .get_by_capability("kdeconnect.ping")
            .await
            .is_some());
        assert!(registry
            .get_by_capability("kdeconnect.unknown")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_list() {
        let registry = PluginRegistry::new();
        registry
            .register(Arc::new(MockPlugin::new("a", vec![])))
            .await;
        registry
            .register(Arc::new(MockPlugin::new("b", vec![])))
            .await;

        let mut names = registry.list().await;
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn test_unregister() {
        let registry = PluginRegistry::new();
        registry
            .register(Arc::new(MockPlugin::new("ping", vec!["kdeconnect.ping"])))
            .await;

        assert!(registry.unregister("ping").await);
        assert!(!registry.unregister("ping").await);
        assert!(registry
            .get_by_capability("kdeconnect.ping")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_replaced_plugin_loses_old_capabilities() {
        // Re-registering the same name with different capabilities must not
        // leave the replacement reachable under its predecessor's types.
        let registry = PluginRegistry::new();
        registry
            .register(Arc::new(MockPlugin::new("plug", vec!["kdeconnect.old"])))
            .await;
        registry
            .register(Arc::new(MockPlugin::new("plug", vec!["kdeconnect.new"])))
            .await;

        assert!(registry
            .get_all_by_capability("kdeconnect.old")
            .await
            .is_empty());
        assert_eq!(
            registry.get_all_by_capability("kdeconnect.new").await.len(),
            1
        );
        assert!(registry
            .handle_packet("d", "kdeconnect.old", Packet::ping())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_capability_index_supports_multiple_consumers() {
        // telephony + pausemusic both consume kdeconnect.telephony: the
        // index must keep both, and unregistering one must not strip the
        // other.
        let registry = PluginRegistry::new();
        registry
            .register(Arc::new(MockPlugin::new(
                "telephony",
                vec!["kdeconnect.telephony"],
            )))
            .await;
        registry
            .register(Arc::new(MockPlugin::new(
                "pausemusic",
                vec!["kdeconnect.telephony"],
            )))
            .await;

        let all = registry.get_all_by_capability("kdeconnect.telephony").await;
        assert_eq!(all.len(), 2);

        assert!(registry.unregister("telephony").await);
        let all = registry.get_all_by_capability("kdeconnect.telephony").await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name(), "pausemusic");

        assert!(registry.unregister("pausemusic").await);
        assert!(registry
            .get_all_by_capability("kdeconnect.telephony")
            .await
            .is_empty());
        assert!(registry
            .get_by_capability("kdeconnect.telephony")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_handle_packet() {
        let registry = PluginRegistry::new();
        registry
            .register(Arc::new(MockPlugin::new("ping", vec!["kdeconnect.ping"])))
            .await;

        let packet = Packet::ping();
        let result = registry
            .handle_packet("test", "kdeconnect.ping", packet)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_packet_unknown() {
        let registry = PluginRegistry::new();
        let packet = Packet::ping();
        let result = registry
            .handle_packet("test", "kdeconnect.unknown", packet)
            .await;
        assert!(result.is_err());
    }

    struct NotifyMockPlugin {
        name: String,
        connected_devices: Arc<std::sync::Mutex<Vec<String>>>,
        disconnected_devices: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl NotifyMockPlugin {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                connected_devices: Arc::new(std::sync::Mutex::new(Vec::new())),
                disconnected_devices: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl Plugin for NotifyMockPlugin {
        fn name(&self) -> &str {
            &self.name
        }

        fn incoming_capabilities(&self) -> Vec<String> {
            vec![]
        }

        fn outgoing_capabilities(&self) -> Vec<String> {
            vec![]
        }

        async fn handle_packet(
            &self,
            _device_id: &str,
            _packet: Packet,
        ) -> Result<Option<Vec<Packet>>> {
            Ok(None)
        }

        fn on_connected(&self, device_id: &str) -> Vec<Packet> {
            self.connected_devices
                .lock()
                .expect("Value expected to be present")
                .push(device_id.to_string());
            vec![]
        }

        async fn on_disconnected(&self, device_id: &str) {
            self.disconnected_devices
                .lock()
                .expect("Value expected to be present")
                .push(device_id.to_string());
        }
    }

    #[tokio::test]
    async fn test_notify_connected() {
        let registry = PluginRegistry::new();
        let plugin_a = Arc::new(NotifyMockPlugin::new("a"));
        let plugin_b = Arc::new(NotifyMockPlugin::new("b"));
        registry.register(plugin_a.clone()).await;
        registry.register(plugin_b.clone()).await;

        let packets = registry.notify_connected("dev-1").await;
        assert!(packets.is_empty());

        let a_devices = plugin_a
            .connected_devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let b_devices = plugin_b
            .connected_devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(a_devices, vec!["dev-1"]);
        assert_eq!(b_devices, vec!["dev-1"]);
    }

    #[tokio::test]
    async fn test_notify_disconnected() {
        let registry = PluginRegistry::new();
        let plugin = Arc::new(NotifyMockPlugin::new("test"));
        registry.register(plugin.clone()).await;

        registry.notify_disconnected("dev-1").await;
        registry.notify_disconnected("dev-2").await;

        let disconnected = plugin
            .disconnected_devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(disconnected, vec!["dev-1", "dev-2"]);
    }

    /// PR #40 review (coderabbit + cubic-dev): the notify loop held the
    /// `plugins` read guard across every `on_disconnected` await. The sftp
    /// cleanup alone can take up to `mount_timeout` per device, so
    /// `register`/`unregister` blocked for the whole cleanup. The guard
    /// must be released before the awaits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notify_disconnected_does_not_block_registration_on_slow_cleanup() {
        struct SlowCleanupPlugin {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }

        #[async_trait::async_trait]
        impl Plugin for SlowCleanupPlugin {
            fn name(&self) -> &str {
                "slow-cleanup"
            }

            fn incoming_capabilities(&self) -> Vec<String> {
                vec![]
            }

            fn outgoing_capabilities(&self) -> Vec<String> {
                vec![]
            }

            async fn handle_packet(
                &self,
                _device_id: &str,
                _packet: Packet,
            ) -> Result<Option<Vec<Packet>>> {
                Ok(None)
            }

            async fn on_disconnected(&self, _device_id: &str) {
                // notify_one, not notify_waiters: the counterpart can fire
                // while this task is between this notify and registering
                // the release waiter below; notify_one stores a permit
                // instead of losing the wake (hung the full suite once).
                self.entered.notify_one();
                self.release.notified().await;
            }
        }

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let registry = Arc::new(PluginRegistry::new());
        registry
            .register(Arc::new(SlowCleanupPlugin {
                entered: entered.clone(),
                release: release.clone(),
            }))
            .await;

        // Register the waiter BEFORE spawning so the notify can't be lost.
        let entered_fut = entered.notified();
        tokio::pin!(entered_fut);
        entered_fut.as_mut().enable();

        let r = registry.clone();
        let notify = tokio::spawn(async move { r.notify_disconnected("dev-1").await });
        entered_fut.await; // cleanup is now mid-await inside notify_disconnected

        // Registration must not wait for the cleanup to finish.
        let r = registry.clone();
        let registered = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            r.register(Arc::new(MockPlugin::new("late", vec![]))),
        )
        .await;
        assert!(
            registered.is_ok(),
            "register blocked while notify_disconnected awaited a slow plugin"
        );

        release.notify_one();
        notify.await.expect("notify_disconnected completes");
    }
}

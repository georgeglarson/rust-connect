//! Device registry for CRUD operations on devices
//!
//! Single Responsibility: Store and retrieve devices.
//! Thread-safe via RwLock. Optional disk persistence.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;

use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::device::types::{Device, DeviceId, DeviceState};
use crate::utils::errors::{Error, Result};

/// Unpaired, unconnected records untouched for longer than this are dropped
/// on startup.
///
/// `devices.json` otherwise grows monotonically. A phone that reinstalls
/// announces a fresh device id, and the record for the old id keeps a
/// `Disconnected` state — which `Device::is_paired` reports as paired, so
/// `save_to_disk` writes it back forever. Three records for one physical
/// handset is the observed shape.
const STALE_DEVICE_TTL_DAYS: i64 = 30;

pub struct DeviceRegistry {
    devices: Arc<RwLock<HashMap<DeviceId, Device>>>,
    persist_path: Option<PathBuf>,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            persist_path: None,
        }
    }

    pub fn with_persistence(persist_path: PathBuf) -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            persist_path: Some(persist_path),
        }
    }

    pub async fn upsert_device(&self, device: Device) -> Result<()> {
        let should_save = {
            let mut devices = self.devices.write().await;
            if let Some(existing) = devices.get_mut(&device.id) {
                // A known device re-announcing itself carries a CURRENT identity.
                // Refresh what the identity owns and keep what the lifecycle owns.
                // Only bumping last_seen froze name, protocol version and
                // capabilities at first-discovery values forever: observed
                // 2026-07-30, a phone first seen in April still reported empty
                // capability lists while connected and paired.
                existing.name = device.name;
                existing.device_type = device.device_type;
                existing.protocol_version = device.protocol_version;
                existing.incoming_capabilities = device.incoming_capabilities;
                existing.outgoing_capabilities = device.outgoing_capabilities;
                // Discovery re-announces carry no address of their own; don't
                // erase one we already learned.
                if device.address.is_some() {
                    existing.address = device.address;
                }
                // state, state_since, paired_at, certificate_fingerprint,
                // verification_key and discovered_at belong to the lifecycle and
                // to history. Copying the incoming record wholesale would unpair
                // the device on every discovery packet.
                existing.update_last_seen();
            } else {
                info!(device_id = %device.id, event = "device_upserted", "Adding new device via upsert");
                devices.insert(device.id.clone(), device);
            }
            self.persist_path.is_some()
        };

        if should_save {
            self.save_to_disk().await.ok();
        }
        Ok(())
    }

    pub async fn add(&self, device: Device) -> Result<()> {
        let should_save = {
            let mut devices = self.devices.write().await;
            if devices.contains_key(&device.id) {
                return Err(Error::DeviceAlreadyExists(device.id));
            }
            info!(device_id = %device.id, event = "device_added", "Adding device to registry");
            devices.insert(device.id.clone(), device);
            self.persist_path.is_some()
        };

        if should_save {
            self.save_to_disk().await.ok();
        }
        Ok(())
    }

    pub async fn get(&self, id: &DeviceId) -> Result<Device> {
        let devices = self.devices.read().await;
        devices
            .get(id)
            .cloned()
            .ok_or_else(|| Error::DeviceNotFound(id.clone()))
    }

    pub async fn list(&self) -> Vec<Device> {
        let devices = self.devices.read().await;
        devices.values().cloned().collect()
    }

    pub async fn update(&self, device: Device) -> Result<()> {
        let mut devices = self.devices.write().await;
        if !devices.contains_key(&device.id) {
            return Err(Error::DeviceNotFound(device.id));
        }
        debug!(device_id = %device.id, "Updating device in registry");
        devices.insert(device.id.clone(), device);
        Ok(())
    }

    pub async fn remove(&self, id: &DeviceId) -> Result<Device> {
        let (device, should_save) = {
            let mut devices = self.devices.write().await;
            let device = devices
                .remove(id)
                .ok_or_else(|| Error::DeviceNotFound(id.clone()))?;
            info!(device_id = %id, event = "device_removed", "Device removed from registry");
            (device, self.persist_path.is_some())
        };

        if should_save {
            self.save_to_disk().await.ok();
        }
        Ok(device)
    }

    pub async fn contains(&self, id: &DeviceId) -> bool {
        let devices = self.devices.read().await;
        devices.contains_key(id)
    }

    pub async fn count(&self) -> usize {
        let devices = self.devices.read().await;
        devices.len()
    }

    pub async fn list_by_state(&self, state: DeviceState) -> Vec<Device> {
        let devices = self.devices.read().await;
        devices
            .values()
            .filter(|d| d.state == state)
            .cloned()
            .collect()
    }

    pub async fn save_to_disk(&self) -> Result<()> {
        let path = match &self.persist_path {
            Some(p) => p.clone(),
            None => return Err(Error::ConfigError("No persist path configured".to_string())),
        };

        let devices = self.devices.read().await;
        let paired_only: std::collections::HashMap<&DeviceId, &Device> =
            devices.iter().filter(|(_, d)| d.is_paired()).collect();
        let json = serde_json::to_string_pretty(&paired_only)?;
        drop(devices);

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let temp_path = path.with_extension("tmp");
        tokio::fs::write(&temp_path, &json).await?;
        tokio::fs::rename(&temp_path, &path).await?;

        info!(path = %path.display(), "Saved device registry to disk");
        Ok(())
    }

    pub async fn load_from_disk(&self) -> Result<()> {
        let path = match &self.persist_path {
            Some(p) => p.clone(),
            None => return Err(Error::ConfigError("No persist path configured".to_string())),
        };

        if !path.exists() {
            debug!("No persisted registry found, starting fresh");
            return Ok(());
        }

        let data = tokio::fs::read_to_string(&path).await?;
        let loaded: HashMap<DeviceId, Device> = serde_json::from_str(&data)?;

        let mut devices = self.devices.write().await;
        let count = loaded.len();
        *devices = loaded;

        info!(count, path = %path.display(), "Loaded device registry from disk");
        Ok(())
    }

    /// Drop records the pairing store no longer knows, that are not
    /// connected, and that have not been seen for `STALE_DEVICE_TTL_DAYS`.
    ///
    /// `paired_ids` comes from `PairingHandler::paired_devices` and is the
    /// authority. `Device::is_paired` is NOT usable here: it reads the
    /// record's own `state`, which stays `Disconnected` long after an
    /// unpair, and that is exactly how the phantom records survive.
    ///
    /// Returns the ids that were removed.
    pub async fn prune_stale_devices(&self, paired_ids: &HashSet<DeviceId>) -> Vec<DeviceId> {
        let cutoff = Utc::now() - chrono::Duration::days(STALE_DEVICE_TTL_DAYS);
        let (pruned, should_save) = {
            let mut devices = self.devices.write().await;
            let mut pruned = Vec::new();
            devices.retain(|id, device| {
                let keep =
                    paired_ids.contains(id) || device.is_connected() || device.last_seen >= cutoff;
                if !keep {
                    pruned.push(id.clone());
                }
                keep
            });
            (pruned, self.persist_path.is_some())
        };

        for id in &pruned {
            info!(
                device_id = %id,
                event = "device_pruned",
                "Pruned a stale unpaired device record"
            );
        }

        if should_save && !pruned.is_empty() {
            self.save_to_disk().await.ok();
        }

        pruned
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::DeviceType;

    fn test_device(id: &str) -> Device {
        Device::new(
            id.to_string(),
            format!("Device {}", id),
            DeviceType::Phone,
            7,
        )
    }

    #[tokio::test]
    async fn test_add_and_get() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let device = test_device("dev-1");
        registry
            .add(device)
            .await
            .expect("Value expected to be present");

        let got = registry
            .get(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(got.id, "dev-1");
        assert_eq!(got.name, "Device dev-1");
        Ok(())
    }

    #[tokio::test]
    async fn test_add_duplicate_fails() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");
        let result = registry.add(test_device("dev-1")).await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_get_nonexistent() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let result = registry.get(&"nope".to_string()).await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_list() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        registry
            .add(test_device("a"))
            .await
            .expect("Value expected to be present");
        registry
            .add(test_device("b"))
            .await
            .expect("Value expected to be present");
        let list = registry.list().await;
        assert_eq!(list.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_update() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");

        let mut device = registry
            .get(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        device.name = "Updated".to_string();
        registry
            .update(device)
            .await
            .expect("Value expected to be present");

        let got = registry
            .get(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(got.name, "Updated");
        Ok(())
    }

    #[tokio::test]
    async fn test_update_nonexistent() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let result = registry.update(test_device("nope")).await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_remove() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");
        let removed = registry
            .remove(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(removed.id, "dev-1");
        assert!(!registry.contains(&"dev-1".to_string()).await);
        Ok(())
    }

    #[tokio::test]
    async fn test_remove_nonexistent() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let result = registry.remove(&"nope".to_string()).await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_list_by_state() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();

        let mut d1 = test_device("a");
        d1.state = DeviceState::Discovered;
        let mut d2 = test_device("b");
        d2.state = DeviceState::Connected;

        registry
            .add(d1)
            .await
            .expect("Value expected to be present");
        registry
            .add(d2)
            .await
            .expect("Value expected to be present");

        let discovered = registry.list_by_state(DeviceState::Discovered).await;
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, "a");
        Ok(())
    }

    #[tokio::test]
    async fn test_persistence_roundtrip() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("devices.json");

        let registry = DeviceRegistry::with_persistence(path.clone());
        let mut d1 = test_device("dev-1");
        d1.state = DeviceState::Paired;
        d1.paired_at = Some(chrono::Utc::now());
        let mut d2 = test_device("dev-2");
        d2.state = DeviceState::Paired;
        d2.paired_at = Some(chrono::Utc::now());
        registry
            .add(d1)
            .await
            .expect("Value expected to be present");
        registry
            .add(d2)
            .await
            .expect("Value expected to be present");
        registry
            .save_to_disk()
            .await
            .expect("Value expected to be present");

        let registry2 = DeviceRegistry::with_persistence(path.clone());
        registry2
            .load_from_disk()
            .await
            .expect("Value expected to be present");

        assert_eq!(registry2.count().await, 2);
        assert!(registry2.contains(&"dev-1".to_string()).await);
        assert!(registry2.contains(&"dev-2".to_string()).await);
        Ok(())
    }

    #[tokio::test]
    async fn test_load_missing_file() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("nonexistent.json");

        let registry = DeviceRegistry::with_persistence(path);
        registry
            .load_from_disk()
            .await
            .expect("Value expected to be present");
        assert_eq!(registry.count().await, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_save_to_disk_without_persistence_path() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let result = registry.save_to_disk().await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_load_from_disk_without_persistence_path() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let result = registry.load_from_disk().await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_list_empty_registry() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        assert!(registry.list().await.is_empty());
        assert_eq!(registry.count().await, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_list_by_state_empty() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let result = registry.list_by_state(DeviceState::Connected).await;
        assert!(result.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_contains_false() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        assert!(!registry.contains(&"nobody".to_string()).await);
        Ok(())
    }

    #[tokio::test]
    async fn test_concurrent_add_and_read() -> anyhow::Result<()> {
        let registry = Arc::new(DeviceRegistry::new());
        let mut handles = Vec::new();

        for i in 0..20 {
            let reg = registry.clone();
            handles.push(tokio::spawn(async move {
                let device = Device::new(
                    format!("concurrent-{}", i),
                    format!("Device {}", i),
                    DeviceType::Phone,
                    7,
                );
                reg.add(device).await.expect("Value expected to be present");
            }));
        }

        for handle in handles {
            handle.await.expect("Value expected to be present");
        }

        assert_eq!(registry.count().await, 20);

        for i in 0..20 {
            assert!(registry.contains(&format!("concurrent-{}", i)).await);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_concurrent_reads() -> anyhow::Result<()> {
        let registry = Arc::new(DeviceRegistry::new());
        registry
            .add(test_device("shared"))
            .await
            .expect("Value expected to be present");

        let mut handles = Vec::new();
        for _ in 0..10 {
            let reg = registry.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    let _ = reg
                        .get(&"shared".to_string())
                        .await
                        .expect("Value expected to be present");
                    let _ = reg.contains(&"shared".to_string()).await;
                }
            }));
        }

        for handle in handles {
            handle.await.expect("Value expected to be present");
        }

        assert_eq!(registry.count().await, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_add_remove_add_same_device() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        registry
            .add(test_device("reusable"))
            .await
            .expect("Value expected to be present");
        registry
            .remove(&"reusable".to_string())
            .await
            .expect("Value expected to be present");
        assert!(!registry.contains(&"reusable".to_string()).await);
        registry
            .add(test_device("reusable"))
            .await
            .expect("Value expected to be present");
        assert!(registry.contains(&"reusable".to_string()).await);
        Ok(())
    }

    #[tokio::test]
    async fn test_persistence_corrupt_json_fails() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new()?;
        let path = temp.path().join("bad.json");
        tokio::fs::write(&path, "not valid json {{{").await?;

        let registry = DeviceRegistry::with_persistence(path);
        let result = registry.load_from_disk().await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_update_preserves_all_fields() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let original = test_device("dev-1");
        registry
            .add(original)
            .await
            .expect("Value expected to be present");

        let mut updated = registry
            .get(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        updated.name = "New Name".to_string();
        registry
            .update(updated)
            .await
            .expect("Value expected to be present");

        let loaded = registry
            .get(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(loaded.name, "New Name");
        assert_eq!(loaded.device_type, DeviceType::Phone);
        assert_eq!(loaded.protocol_version, 7);
        Ok(())
    }

    #[tokio::test]
    async fn test_prune_drops_only_stale_unpaired_records() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();

        let mut paired_old = test_device("paired-old");
        paired_old.state = DeviceState::Disconnected;
        paired_old.last_seen = Utc::now() - chrono::Duration::days(400);

        let mut unpaired_old = test_device("unpaired-old");
        unpaired_old.state = DeviceState::Disconnected;
        unpaired_old.last_seen = Utc::now() - chrono::Duration::days(400);

        let mut unpaired_recent = test_device("unpaired-recent");
        unpaired_recent.state = DeviceState::Disconnected;
        unpaired_recent.last_seen = Utc::now() - chrono::Duration::days(1);

        for device in [paired_old, unpaired_old, unpaired_recent] {
            registry.add(device).await.expect("add device");
        }

        // The pairing store is the authority, not the record's own state.
        let paired: HashSet<DeviceId> = ["paired-old".to_string()].into_iter().collect();
        let pruned = registry.prune_stale_devices(&paired).await;

        assert_eq!(pruned, vec!["unpaired-old".to_string()]);
        assert!(registry.contains(&"paired-old".to_string()).await);
        assert!(registry.contains(&"unpaired-recent".to_string()).await);
        assert!(!registry.contains(&"unpaired-old".to_string()).await);
        Ok(())
    }

    #[tokio::test]
    async fn test_prune_keeps_connected_device_even_when_unpaired_and_old() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let mut connected = test_device("connected-old");
        connected.state = DeviceState::Connected;
        connected.last_seen = Utc::now() - chrono::Duration::days(400);
        registry.add(connected).await.expect("add device");

        let pruned = registry.prune_stale_devices(&HashSet::new()).await;

        assert!(
            pruned.is_empty(),
            "a connected device is never pruned, whatever the pairing store says"
        );
        assert!(registry.contains(&"connected-old".to_string()).await);
        Ok(())
    }
}

#[cfg(test)]
mod device_record_accuracy_tests {
    use super::*;
    use crate::device::types::{Device, DeviceType};

    fn dev(id: &str) -> Device {
        Device::new(
            id.to_string(),
            "Galaxy A15 5G".to_string(),
            DeviceType::Phone,
            8,
        )
    }

    /// A device we already know re-announces itself on every discovery. Its
    /// identity-derived fields (name, type, protocol version, capabilities) are
    /// whatever the CURRENT identity packet says; keeping the values captured the
    /// first time we ever saw it means a record that can never self-correct. Seen
    /// live 2026-07-30: a phone discovered in April still reported empty
    /// capability lists while connected and paired.
    #[tokio::test]
    async fn test_upsert_refreshes_identity_derived_fields() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        registry
            .add(dev("aaaabbbbccccddddeeeeffff00001111"))
            .await?;

        let mut fresh = dev("aaaabbbbccccddddeeeeffff00001111");
        fresh.name = "Galaxy A15 5G (renamed)".to_string();
        fresh.protocol_version = 9;
        fresh.incoming_capabilities = vec!["kdeconnect.notification".to_string()];
        fresh.outgoing_capabilities = vec!["kdeconnect.battery".to_string()];
        registry.upsert_device(fresh).await?;

        let got = registry
            .get(&"aaaabbbbccccddddeeeeffff00001111".to_string())
            .await?;
        assert_eq!(got.name, "Galaxy A15 5G (renamed)");
        assert_eq!(got.protocol_version, 9);
        assert_eq!(got.incoming_capabilities, vec!["kdeconnect.notification"]);
        assert_eq!(got.outgoing_capabilities, vec!["kdeconnect.battery"]);
        Ok(())
    }

    /// Refreshing identity fields must not clobber the fields the lifecycle owns.
    /// A discovery re-announce carries no pairing state, so copying the incoming
    /// record wholesale would unpair the device in the registry's view.
    #[tokio::test]
    async fn test_upsert_preserves_lifecycle_owned_fields() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let mut known = dev("aaaabbbbccccddddeeeeffff00002222");
        known.state = DeviceState::Paired;
        let paired_at = chrono::Utc::now();
        known.paired_at = Some(paired_at);
        known.certificate_fingerprint = Some("aa:bb:cc".to_string());
        let discovered_at = known.discovered_at;
        registry.add(known).await?;

        registry
            .upsert_device(dev("aaaabbbbccccddddeeeeffff00002222"))
            .await?;

        let got = registry
            .get(&"aaaabbbbccccddddeeeeffff00002222".to_string())
            .await?;
        assert_eq!(got.state, DeviceState::Paired, "discovery must not unpair");
        assert_eq!(got.paired_at, Some(paired_at));
        assert_eq!(got.certificate_fingerprint.as_deref(), Some("aa:bb:cc"));
        assert_eq!(
            got.discovered_at, discovered_at,
            "first-seen time is history"
        );
        Ok(())
    }
}

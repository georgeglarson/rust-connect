//! Device registry for CRUD operations on devices
//!
//! Single Responsibility: Store and retrieve devices.
//! Thread-safe via RwLock. Optional disk persistence.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};

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

/// Cap on unpaired, pre-auth device records held in memory. Mirrors
/// `MAX_TRANSFER_DEVICES` (share.rs:176) for the identical
/// peer-id-keyed-map reason: `lifecycle::ensure_and_transition`
/// unconditionally `add()`s a registry record for ANY device id that
/// completes TCP+TLS+identity, with zero pairing required (TOFU accepts any
/// first-contact cert) — a flood of fresh random ids grew this map and
/// `devices.json` without bound (finding L2-1, Sprint 2 security audit).
/// Truly-paired devices are never counted against this cap and never
/// evicted; see `is_truly_paired`.
const MAX_UNPAIRED_DEVICES: usize = 64;

/// The shape of `PairingHandler`'s `paired` map, shared by `Arc` — see
/// `PairingHandler::paired_handle` and the `paired_ids` field below.
type PairedIds = Arc<RwLock<HashMap<DeviceId, DateTime<Utc>>>>;

pub struct DeviceRegistry {
    devices: Arc<RwLock<HashMap<DeviceId, Device>>>,
    persist_path: Option<PathBuf>,
    /// Shared view of the REAL paired set — the same `Arc` `PairingHandler`
    /// holds internally (`PairingHandler::paired_handle`), not a copy of its
    /// contents. Wired in production by `app.rs` via `with_paired_source`.
    ///
    /// `None` when nothing wired one (older test setups): the unpaired cap
    /// still applies, but `is_truly_paired` degrades to `Device::is_paired()`
    /// (state-based — "reached Connected once," not "completed SAS pairing",
    /// see that method's doc) as a fallback. Documented, accepted
    /// degradation: production always wires the handle.
    paired_ids: Option<PairedIds>,
    /// Serializes `save_to_disk` snapshot+write+rename. Without it,
    /// concurrent callers (e.g. two devices reaching `Paired` back to
    /// back) each write their own snapshot to the SAME `path.with_extension
    /// ("tmp")` and rename over each other — the loser's rename can land
    /// after the winner's, silently discarding whichever snapshot lost the
    /// race, or a reader can open the temp path mid-write. One lock per
    /// registry makes the whole sequence atomic relative to itself.
    save_lock: tokio::sync::Mutex<()>,
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
            paired_ids: None,
            save_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_persistence(persist_path: PathBuf) -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            persist_path: Some(persist_path),
            paired_ids: None,
            save_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Wire the shared view of true pairing (see `paired_ids` field doc).
    /// Consumed at construction, before wrapping in `Arc` — the production
    /// call site is `app.rs`, chained onto `with_persistence`.
    pub fn with_paired_source(mut self, paired_ids: PairedIds) -> Self {
        self.paired_ids = Some(paired_ids);
        self
    }

    /// Whether `device` has completed REAL SAS pairing, not merely reached
    /// `Connected` once (see `Device::is_paired`'s doc for that distinction
    /// — the L2-1 naming collision). Falls back to `Device::is_paired()`
    /// only when no `paired_ids` handle is wired.
    async fn is_truly_paired(&self, device: &Device) -> bool {
        match &self.paired_ids {
            Some(paired) => paired.read().await.contains_key(&device.id),
            None => device.is_paired(),
        }
    }

    /// Enforce `MAX_UNPAIRED_DEVICES` before a NEW record is inserted
    /// (`add()` / the insert branch of `upsert_device()`). Caller must
    /// already hold the write lock on `devices` and `incoming` must not yet
    /// be present in it.
    ///
    /// If `incoming` is itself truly paired, it never counts against the cap
    /// and nothing is evicted for it. Otherwise, if the current unpaired
    /// count is already at the cap, the oldest unpaired record by
    /// `last_seen` is evicted to make room — evict-oldest, not reject: a
    /// reject lets an attacker who fills the cap first lock out a
    /// legitimate new device, whereas LRU eviction keeps the newest
    /// activity and can never displace a truly-paired device (they are
    /// never eviction candidates).
    async fn enforce_unpaired_cap(
        &self,
        devices: &mut HashMap<DeviceId, Device>,
        incoming: &Device,
    ) {
        // Snapshot the shared paired set ONCE (or its absence) instead of an
        // async read per device — the scan below is then pure sync. The
        // `None` fallback keeps the state-based `Device::is_paired()` signal
        // (documented degradation; production always wires the handle).
        let paired_snapshot: Option<std::collections::HashSet<DeviceId>> = match &self.paired_ids {
            Some(paired) => Some(paired.read().await.keys().cloned().collect()),
            None => None,
        };
        let is_paired = |d: &Device| match &paired_snapshot {
            Some(ids) => ids.contains(&d.id),
            None => d.is_paired(),
        };

        if is_paired(incoming) {
            return;
        }

        // Every currently-unpaired record, oldest first. We must drain down
        // to the cap, not evict a single record: a mass unpair (many devices
        // leaving the shared paired map at once) flips their registry records
        // to unpaired all at once, pushing the count well over the cap. A
        // one-per-insert eviction would then pin the registry at that
        // elevated count forever (evict 1, add 1 = net zero), never
        // converging — the invariant is "at most MAX_UNPAIRED_DEVICES
        // unpaired records after an unpaired insert", and only a drain
        // restores it. (PR #15 review: coderabbit MAJOR.)
        let mut unpaired: Vec<(DeviceId, DateTime<Utc>)> = devices
            .values()
            .filter(|d| !is_paired(d))
            .map(|d| (d.id.clone(), d.last_seen))
            .collect();

        // Leave room for the incoming insert: drain to cap-1 so the caller's
        // insert lands exactly at the cap.
        if unpaired.len() < MAX_UNPAIRED_DEVICES {
            return;
        }
        unpaired.sort_by_key(|(_, last_seen)| *last_seen);
        let evict_count = unpaired.len() - MAX_UNPAIRED_DEVICES + 1;
        for (oldest_id, _) in unpaired.into_iter().take(evict_count) {
            if let Some(evicted) = devices.remove(&oldest_id) {
                info!(
                    device_id = %evicted.id,
                    event = "device_evicted_unpaired_cap",
                    "Evicted oldest unpaired device record to enforce MAX_UNPAIRED_DEVICES"
                );
            }
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
                // kde's empty-cap guard lives in ONE place —
                // Device::apply_capability_update (types.rs) — shared with
                // the lifecycle reconnect path. See its doc for the
                // upstream cite and the PR #12 finding that split-site
                // guards invite.
                existing.apply_capability_update(
                    device.incoming_capabilities,
                    device.outgoing_capabilities,
                );
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
                self.enforce_unpaired_cap(&mut devices, &device).await;
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
            self.enforce_unpaired_cap(&mut devices, &device).await;
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

    /// Read-modify-write under ONE write lock (2026-09-02 audit, C2). The
    /// closure sees the current record and may change it or refuse; a
    /// concurrent caller sees the result, never the same stale snapshot.
    /// `get` + `update` as two lock scopes let two callers both validate
    /// against one snapshot, both report success, and one silently lose.
    pub async fn modify<T>(
        &self,
        id: &DeviceId,
        f: impl FnOnce(&mut Device) -> Result<T>,
    ) -> Result<T> {
        let mut devices = self.devices.write().await;
        let device = devices
            .get_mut(id)
            .ok_or_else(|| Error::DeviceNotFound(id.clone()))?;
        // The id is the map key (and `Device.id` is public): a closure that
        // rewrote it would leave the record under the old key with a
        // different id inside. Refuse and restore (PR #39 review).
        // Restore the WHOLE record, not just the id: a closure that changed
        // the id and another field must not half-commit (PR #39 review).
        let original = device.clone();
        let result = f(device);
        if device.id != original.id {
            *device = original;
            return Err(Error::InvalidRequest(
                "DeviceRegistry::modify must not change the device id".to_string(),
            ));
        }
        result
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

        // Serialize the whole snapshot+write+rename sequence. Two
        // concurrent callers otherwise race on the SAME `path.with_extension
        // ("tmp")`: one's `write` can land between the other's `write` and
        // `rename`, or the renames themselves can interleave, silently
        // dropping whichever snapshot lost. Held for the duration of this
        // call (not just the rename) so the file on disk always matches one
        // caller's own snapshot, never an interleaving of two.
        let _save_guard = self.save_lock.lock().await;

        let devices = self.devices.read().await;
        // Gate on TRUE pairing, not `Device::is_paired()` — that method
        // returns true for `Connected`/`Disconnected` too, meaning "reached
        // Connected once," not "completed SAS pairing" (types.rs:253-258).
        // Filtering on it persisted zero-pairing records to devices.json
        // (finding L2-1, Sprint 2 security audit).
        let mut paired_only: std::collections::HashMap<&DeviceId, &Device> = HashMap::new();
        for (id, device) in devices.iter() {
            if self.is_truly_paired(device).await {
                paired_only.insert(id, device);
            }
        }
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

    /// PR #39 review (cubic): `Device.id` is public and is the map key, so a
    /// `modify` closure that rewrote it would leave the record stored under
    /// the old key with a different id inside. The change is refused and
    /// the id restored.
    #[tokio::test]
    async fn test_modify_refuses_to_change_the_device_id() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        registry.add(test_device("dev-1")).await?;
        let result = registry
            .modify(&"dev-1".to_string(), |device| {
                device.id = "dev-2".to_string();
                device.name = "renamed".to_string();
                Ok(())
            })
            .await;
        assert!(result.is_err(), "an id change must be refused");
        let stored = registry.get(&"dev-1".to_string()).await?;
        assert_eq!(stored.id, "dev-1", "the stored id must be restored");
        assert_ne!(
            stored.name, "renamed",
            "a refused modify must not half-commit its other changes"
        );
        Ok(())
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

    /// 50 concurrent `save_to_disk` calls on one registry share the SAME
    /// `path.with_extension("tmp")` temp file. Pre-fix, each call's
    /// write+rename runs unsynchronized: one caller's write can land
    /// between another's write and rename, or renames can interleave, so
    /// `devices.json` can end up truncated, mid-write, or simply missing
    /// the device — a `serde_json::from_str` that errors, or a successful
    /// parse with an empty map, both demonstrate the race. The
    /// `save_lock` fix serializes the whole snapshot+write+rename
    /// sequence so every one of the 50 calls leaves a fully-formed file
    /// containing the one paired device.
    #[tokio::test]
    async fn test_concurrent_save_to_disk_produces_valid_file() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("devices.json");

        let registry = Arc::new(DeviceRegistry::with_persistence(path.clone()));
        let mut d1 = test_device("dev-1");
        d1.state = DeviceState::Paired;
        d1.paired_at = Some(chrono::Utc::now());
        registry
            .add(d1)
            .await
            .expect("Value expected to be present");

        let mut handles = Vec::new();
        for _ in 0..50 {
            let registry = registry.clone();
            handles.push(tokio::spawn(async move {
                registry.save_to_disk().await.expect("save_to_disk failed");
            }));
        }
        for h in handles {
            h.await.expect("save task panicked");
        }

        let data = tokio::fs::read_to_string(&path)
            .await
            .expect("devices.json should exist after 50 concurrent saves");
        let parsed: HashMap<DeviceId, Device> =
            serde_json::from_str(&data).expect("devices.json must parse as valid JSON");
        assert!(
            parsed.contains_key("dev-1"),
            "devices.json must still contain dev-1 after concurrent saves, got: {:?}",
            parsed.keys().collect::<Vec<_>>()
        );
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
            "test phone".to_string(),
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
        fresh.name = "test phone (renamed)".to_string();
        fresh.protocol_version = 9;
        fresh.incoming_capabilities = vec!["kdeconnect.notification".to_string()];
        fresh.outgoing_capabilities = vec!["kdeconnect.battery".to_string()];
        registry.upsert_device(fresh).await?;

        let got = registry
            .get(&"aaaabbbbccccddddeeeeffff00001111".to_string())
            .await?;
        assert_eq!(got.name, "test phone (renamed)");
        assert_eq!(got.protocol_version, 9);
        assert_eq!(got.incoming_capabilities, vec!["kdeconnect.notification"]);
        assert_eq!(got.outgoing_capabilities, vec!["kdeconnect.battery"]);
        Ok(())
    }

    /// Gap 3 (parity-checklist.md § Robustness, vk #997): kde only applies
    /// a capability update when BOTH the new incoming AND outgoing lists
    /// are non-empty (core/device.cpp:319-328). A hand-crafted or buggy
    /// identity carrying an empty capability list must not wipe out
    /// capabilities already learned from a real one. Real peers always
    /// send both (this is hostile-input hardening, adversary class A/B —
    /// reachable via a crafted UDP identity, not via normal operation).
    #[tokio::test]
    async fn test_upsert_empty_capabilities_do_not_clobber_known_ones() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let mut known = dev("aaaabbbbccccddddeeeeffff00003333");
        known.incoming_capabilities = vec!["kdeconnect.notification".to_string()];
        known.outgoing_capabilities = vec!["kdeconnect.battery".to_string()];
        registry.add(known).await?;

        let mut empty_caps = dev("aaaabbbbccccddddeeeeffff00003333");
        empty_caps.name = "test phone (renamed)".to_string();
        empty_caps.incoming_capabilities = vec![];
        empty_caps.outgoing_capabilities = vec![];
        registry.upsert_device(empty_caps).await?;

        let got = registry
            .get(&"aaaabbbbccccddddeeeeffff00003333".to_string())
            .await?;
        // Other identity-derived fields still refresh normally...
        assert_eq!(got.name, "test phone (renamed)");
        // ...but capabilities survive the empty-cap identity.
        assert_eq!(
            got.incoming_capabilities,
            vec!["kdeconnect.notification"],
            "empty incoming capabilities on the new identity must not clobber known ones"
        );
        assert_eq!(
            got.outgoing_capabilities,
            vec!["kdeconnect.battery"],
            "empty outgoing capabilities on the new identity must not clobber known ones"
        );
        Ok(())
    }

    /// The guard is BOTH-empty, not either — kde's condition is `!A.isEmpty()
    /// && !B.isEmpty()`, so an identity with ONE list populated and the
    /// other empty also fails the condition and must not update either
    /// list (not just the empty one) — matching kde's all-or-nothing pair
    /// update, not a per-field one.
    #[tokio::test]
    async fn test_upsert_one_empty_one_populated_still_does_not_update() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let mut known = dev("aaaabbbbccccddddeeeeffff00004444");
        known.incoming_capabilities = vec!["kdeconnect.notification".to_string()];
        known.outgoing_capabilities = vec!["kdeconnect.battery".to_string()];
        registry.add(known).await?;

        let mut half_empty = dev("aaaabbbbccccddddeeeeffff00004444");
        half_empty.incoming_capabilities = vec!["kdeconnect.ping".to_string()];
        half_empty.outgoing_capabilities = vec![]; // outgoing empty
        registry.upsert_device(half_empty).await?;

        let got = registry
            .get(&"aaaabbbbccccddddeeeeffff00004444".to_string())
            .await?;
        assert_eq!(
            got.incoming_capabilities,
            vec!["kdeconnect.notification"],
            "incoming must not update either, even though it was itself non-empty"
        );
        assert_eq!(got.outgoing_capabilities, vec!["kdeconnect.battery"]);
        Ok(())
    }

    /// Both non-empty is the normal case and must still update — this is
    /// the guard's negative-space check, pinning that the fix doesn't
    /// accidentally freeze capabilities forever.
    #[tokio::test]
    async fn test_upsert_both_non_empty_still_updates() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();
        let mut known = dev("aaaabbbbccccddddeeeeffff00005555");
        known.incoming_capabilities = vec!["kdeconnect.notification".to_string()];
        known.outgoing_capabilities = vec!["kdeconnect.battery".to_string()];
        registry.add(known).await?;

        let mut fresh = dev("aaaabbbbccccddddeeeeffff00005555");
        fresh.incoming_capabilities = vec!["kdeconnect.ping".to_string()];
        fresh.outgoing_capabilities = vec!["kdeconnect.sms".to_string()];
        registry.upsert_device(fresh).await?;

        let got = registry
            .get(&"aaaabbbbccccddddeeeeffff00005555".to_string())
            .await?;
        assert_eq!(got.incoming_capabilities, vec!["kdeconnect.ping"]);
        assert_eq!(got.outgoing_capabilities, vec!["kdeconnect.sms"]);
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

/// L2-1 (High, Sprint 2 security audit): unbounded pre-auth device-registry
/// growth. `lifecycle::ensure_and_transition` (lifecycle.rs:120)
/// unconditionally `registry.add()`s any unknown device id that completes
/// TCP+TLS+identity — TOFU accepts any first-contact cert, so a LAN peer
/// reaches a persistable record with ZERO pairing. `save_to_disk` filtered
/// on `Device::is_paired()`, which returns true for `Connected` — a naming
/// collision meaning "reached Connected once," not "completed SAS pairing"
/// (types.rs:253-258). Result: a flood of fresh random device ids grows the
/// in-memory map and `devices.json` without bound, pre-auth.
#[cfg(test)]
mod unpaired_cap_tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::{Device, DeviceType};

    fn unpaired_device(id: &str) -> Device {
        // Discovered is the state `Device::new` produces and the state a
        // device is in at the moment `registry.add()`/`upsert_device()` is
        // called on the real insertion path (lifecycle.rs:110-120) —
        // transition to Connected happens strictly AFTER the insert, via
        // `update()`, which this fix does not touch.
        Device::new(
            id.to_string(),
            format!("Device {}", id),
            DeviceType::Phone,
            8,
        )
    }

    /// Test 1 (the repro): flood 200 distinct unpaired device ids through
    /// `add()`. Must FAIL on the unbounded code (count == 200, all 200
    /// persisted) and PASS after the fix (count <= MAX_UNPAIRED_DEVICES,
    /// zero of them in devices.json).
    #[tokio::test]
    async fn test_flood_of_unpaired_devices_is_capped_and_not_persisted() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let path = temp.path().join("devices.json");
        let registry = DeviceRegistry::with_persistence(path.clone());

        for i in 0..200 {
            registry
                .add(unpaired_device(&format!("flood-{i}")))
                .await
                .expect("add device");
        }

        assert!(
            registry.count().await <= MAX_UNPAIRED_DEVICES,
            "in-memory registry must stay bounded at MAX_UNPAIRED_DEVICES under an unpaired flood, got {}",
            registry.count().await
        );

        let on_disk = tokio::fs::read_to_string(&path).await?;
        let on_disk: std::collections::HashMap<DeviceId, Device> = serde_json::from_str(&on_disk)?;
        assert!(
            on_disk.is_empty(),
            "devices.json must hold ZERO unpaired records, got {}",
            on_disk.len()
        );
        Ok(())
    }

    /// Test 2: a device that completes REAL SAS pairing (present in the
    /// shared `paired_ids` handle — the same Arc `PairingHandler.paired`
    /// holds) must survive a flood of unpaired devices: never evicted, still
    /// present in the registry, still persisted to devices.json.
    #[tokio::test]
    async fn test_truly_paired_device_survives_unpaired_flood() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let path = temp.path().join("devices.json");

        let paired_ids: Arc<RwLock<HashMap<DeviceId, chrono::DateTime<Utc>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        paired_ids
            .write()
            .await
            .insert("real-paired-device".to_string(), Utc::now());

        let registry =
            DeviceRegistry::with_persistence(path.clone()).with_paired_source(paired_ids.clone());

        registry
            .add(unpaired_device("real-paired-device"))
            .await
            .expect("add paired device");

        for i in 0..200 {
            registry
                .add(unpaired_device(&format!("flood-{i}")))
                .await
                .expect("add device");
        }

        assert!(
            registry.contains(&"real-paired-device".to_string()).await,
            "a truly-paired device must never be evicted by the unpaired-cap flood"
        );
        assert_eq!(
            registry.count().await,
            MAX_UNPAIRED_DEVICES + 1,
            "the cap bounds the unpaired flood; the paired device is never a candidate for it"
        );

        let on_disk = tokio::fs::read_to_string(&path).await?;
        let on_disk: std::collections::HashMap<DeviceId, Device> = serde_json::from_str(&on_disk)?;
        assert!(
            on_disk.contains_key("real-paired-device"),
            "the truly-paired device must be persisted to devices.json"
        );
        assert_eq!(
            on_disk.len(),
            1,
            "only the truly-paired device may be persisted, no unpaired flood records"
        );
        Ok(())
    }

    /// Test 3: LRU eviction order. Fill the cap, add one more unpaired
    /// device; the oldest-by-`last_seen` unpaired record is the one dropped
    /// — evict-oldest, not reject, so an attacker who fills the cap first
    /// cannot lock out a legitimate new device.
    #[tokio::test]
    async fn test_eviction_drops_oldest_unpaired_by_last_seen() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();

        for i in 0..MAX_UNPAIRED_DEVICES {
            let mut device = unpaired_device(&format!("cap-{i}"));
            // Spread last_seen deterministically so ordering never races on
            // wall-clock resolution: cap-0 is the oldest, cap-63 the newest.
            device.last_seen =
                Utc::now() - chrono::Duration::seconds((MAX_UNPAIRED_DEVICES - i) as i64);
            registry.add(device).await.expect("add device");
        }
        assert_eq!(registry.count().await, MAX_UNPAIRED_DEVICES);

        registry
            .add(unpaired_device("newcomer"))
            .await
            .expect("add newcomer");

        assert_eq!(
            registry.count().await,
            MAX_UNPAIRED_DEVICES,
            "count stays at the cap: one evicted, one inserted"
        );
        assert!(
            !registry.contains(&"cap-0".to_string()).await,
            "the oldest-by-last_seen unpaired record must be the one evicted"
        );
        for i in 1..MAX_UNPAIRED_DEVICES {
            assert!(
                registry.contains(&format!("cap-{i}")).await,
                "cap-{i} is newer than cap-0 and must survive"
            );
        }
        assert!(registry.contains(&"newcomer".to_string()).await);
        Ok(())
    }

    /// PR #15 review (coderabbit MAJOR): a mass unpair pushes the unpaired
    /// count well over the cap all at once. Eviction must DRAIN back to the
    /// cap on the next unpaired insert, not evict a single record (which
    /// would pin the registry at the elevated count forever). Red before the
    /// drain-loop fix: the count stayed at cap+N.
    #[tokio::test]
    async fn test_mass_unpair_then_insert_drains_back_to_cap() -> anyhow::Result<()> {
        let paired_ids: Arc<RwLock<HashMap<DeviceId, chrono::DateTime<Utc>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let registry = DeviceRegistry::new().with_paired_source(paired_ids.clone());

        // 100 genuinely-paired devices: present in the shared paired map, so
        // they are exempt from the cap and all persist in the registry.
        for i in 0..100 {
            let mut d = unpaired_device(&format!("paired-{i}"));
            d.last_seen = Utc::now() - chrono::Duration::seconds((100 - i) as i64);
            paired_ids.write().await.insert(d.id.clone(), Utc::now());
            registry.add(d).await.expect("add paired device");
        }
        assert_eq!(registry.count().await, 100, "100 paired records, no cap");

        // Mass unpair: all 100 leave the shared paired map at once. Their
        // registry records are now unpaired, count 100 > cap 64.
        paired_ids.write().await.clear();

        // A single unpaired insert must drain the excess down to exactly the
        // cap, not merely evict one.
        registry
            .add(unpaired_device("newcomer"))
            .await
            .expect("add newcomer");
        assert_eq!(
            registry.count().await,
            MAX_UNPAIRED_DEVICES,
            "the insert must drain the post-unpair excess back to the cap"
        );
        assert!(
            registry.contains(&"newcomer".to_string()).await,
            "the newcomer (newest) survives the drain"
        );
        Ok(())
    }

    /// Test 4: the persistence gate. An unpaired `Connected` device (reached
    /// Connected with zero SAS pairing — exactly the L2-1 shape) is NOT
    /// written to devices.json; a truly-paired one IS. This is the
    /// `is_paired()` vs `is_truly_paired()` distinction from types.rs:253-258.
    #[tokio::test]
    async fn test_persistence_gate_uses_true_pairing_not_connected_state() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let path = temp.path().join("devices.json");

        let paired_ids: Arc<RwLock<HashMap<DeviceId, chrono::DateTime<Utc>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let registry =
            DeviceRegistry::with_persistence(path.clone()).with_paired_source(paired_ids.clone());

        let mut connected_unpaired = unpaired_device("connected-not-paired");
        connected_unpaired.state = DeviceState::Connected;
        registry.add(connected_unpaired).await?;

        let mut truly_paired = unpaired_device("truly-paired");
        truly_paired.state = DeviceState::Connected;
        registry.add(truly_paired).await?;
        paired_ids
            .write()
            .await
            .insert("truly-paired".to_string(), Utc::now());
        registry.save_to_disk().await?;

        let on_disk = tokio::fs::read_to_string(&path).await?;
        let on_disk: std::collections::HashMap<DeviceId, Device> = serde_json::from_str(&on_disk)?;
        assert!(
            !on_disk.contains_key("connected-not-paired"),
            "reaching Connected with zero SAS pairing must not persist (the L2-1 bug shape)"
        );
        assert!(
            on_disk.contains_key("truly-paired"),
            "a device present in the shared paired_ids handle must persist"
        );
        Ok(())
    }

    /// Test 5: fallback. With no `paired_ids` handle wired (`None` — the
    /// state older test setups are in), the unpaired cap still bounds
    /// growth on both insert paths (`add()` and `upsert_device()`). The
    /// persistence filter degrades to `Device::is_paired()` in this mode
    /// (documented, accepted: production always wires the handle in
    /// app.rs) — this test covers the cap, not the persistence gate.
    #[tokio::test]
    async fn test_cap_bounds_growth_with_no_paired_handle_wired() -> anyhow::Result<()> {
        let registry = DeviceRegistry::new();

        for i in 0..200 {
            registry
                .upsert_device(unpaired_device(&format!("flood-{i}")))
                .await
                .expect("upsert device");
        }

        assert!(
            registry.count().await <= MAX_UNPAIRED_DEVICES,
            "the cap must bound growth even with no paired_ids handle wired, got {}",
            registry.count().await
        );
        Ok(())
    }
}

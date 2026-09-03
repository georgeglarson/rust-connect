//! Device lifecycle manager
//!
//! Single Responsibility: Enforce valid state transitions and emit events.

use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, info, warn};

use crate::device::events::EventBroadcaster;
use crate::device::registry::DeviceRegistry;
use crate::device::types::{Device, DeviceEvent, DeviceId, DeviceState, DeviceType};
use crate::protocol::types::Identity;
use crate::utils::errors::{Error, Result};

#[derive(Clone)]
pub struct LifecycleManager {
    registry: Arc<DeviceRegistry>,
    broadcaster: Arc<EventBroadcaster>,
}

impl LifecycleManager {
    pub fn new(registry: Arc<DeviceRegistry>, broadcaster: Arc<EventBroadcaster>) -> Self {
        Self {
            registry,
            broadcaster,
        }
    }

    pub async fn transition(&self, device_id: &DeviceId, new_state: DeviceState) -> Result<()> {
        // Validate and mutate under one registry write lock (2026-09-02
        // audit, C2): with `get` then `update` as separate scopes, two
        // concurrent callers both validated against the same snapshot, both
        // broadcast StateChanged, and one caller's write was silently lost.
        let old_state = self
            .registry
            .modify(device_id, |device| {
                let old_state = device.state;
                if old_state == new_state {
                    return Ok(None);
                }
                if !old_state.can_transition_to(&new_state) {
                    warn!(
                        device_id = %device_id,
                        old_state = ?old_state,
                        new_state = ?new_state,
                        "Invalid state transition attempted"
                    );
                    return Err(Error::InvalidStateTransition {
                        from: old_state,
                        to: new_state,
                    });
                }

                device.state = new_state;
                device.state_since = Utc::now();

                // A transition is contact with the device, so the record
                // should say so. last_seen used to advance only on discovery
                // upserts, which left a device that stayed connected drifting
                // hours behind reality.
                device.update_last_seen();

                // Restamp on every entry into Paired, not just the first.
                // `paired_at` reads as "paired since" in the API and
                // `paired.json` records the CURRENT pairing; the previous
                // `is_none()` guard made this field mean "first ever paired",
                // so the two stores disagreed after any re-pair.
                if new_state == DeviceState::Paired {
                    device.paired_at = Some(Utc::now());
                }
                Ok(Some(old_state))
            })
            .await?;
        let Some(old_state) = old_state else {
            return Ok(());
        };

        info!(
            device_id = %device_id,
            old_state = ?old_state,
            new_state = ?new_state,
            event = "device_state_changed",
            "Device state transitioned"
        );

        self.broadcaster.broadcast(DeviceEvent::StateChanged {
            device_id: device_id.clone(),
            old_state,
            new_state,
        });

        Ok(())
    }

    pub async fn get_state(&self, device_id: &DeviceId) -> Result<DeviceState> {
        let device = self.registry.get(device_id).await?;
        Ok(device.state)
    }

    pub async fn get_state_since(
        &self,
        device_id: &DeviceId,
    ) -> Result<chrono::DateTime<chrono::Utc>> {
        let device = self.registry.get(device_id).await?;
        Ok(device.state_since)
    }

    pub async fn remove(&self, device_id: &DeviceId) {
        self.registry.remove(device_id).await.ok();
    }

    pub async fn ensure_and_transition(
        &self,
        device_id: &DeviceId,
        identity: &Identity,
        new_state: DeviceState,
    ) -> Result<()> {
        let auto_created = if !self.registry.contains(device_id).await {
            let device = Device::new(
                device_id.clone(),
                identity.device_name.clone(),
                DeviceType::parse_device_type(&identity.device_type),
                identity.protocol_version,
            )
            .with_capabilities(
                identity.incoming_capabilities.clone(),
                identity.outgoing_capabilities.clone(),
            );
            match self.registry.add(device).await {
                Ok(()) => {
                    info!(
                        device_id = %device_id,
                        event = "device_auto_registered",
                        "Auto-registered device in registry during lifecycle transition"
                    );
                    true
                }
                Err(Error::DeviceAlreadyExists(_)) => {
                    debug!(device_id = %device_id, "Device already registered (race)");
                    false
                }
                Err(e) => return Err(e),
            }
        } else {
            // A device we already know is reconnecting with a CURRENT identity.
            // Adopt its identity-derived fields, or a record created before
            // capabilities were captured keeps its empty lists forever — which is
            // exactly what the live daemon showed after only the UDP-discovery
            // site was fixed. Same field split as DeviceRegistry::upsert_device:
            // the lifecycle owns state and pairing, the identity owns this.
            // Same single-lock shape as `transition` (audit C2).
            let _ = self
                .registry
                .modify(device_id, |existing| {
                    existing.name = identity.device_name.clone();
                    existing.device_type = DeviceType::parse_device_type(&identity.device_type);
                    existing.protocol_version = identity.protocol_version;
                    // Same empty-cap guard as the UDP-discovery site — this
                    // reconnect path bypassed it when the guard lived inline
                    // in upsert_device (PR #12 review finding): a crafted
                    // empty-cap identity arriving over the connection path
                    // could still wipe learned capabilities.
                    existing.apply_capability_update(
                        identity.incoming_capabilities.clone(),
                        identity.outgoing_capabilities.clone(),
                    );
                    existing.update_last_seen();
                    Ok(())
                })
                .await;
            false
        };

        if auto_created && new_state == DeviceState::Disconnected {
            // Discovered→Disconnected is not a valid transition, so walk the
            // freshly-registered device through Connected first. Note there
            // is deliberately NO Paired fast-forward: Discovered→Connected
            // is a valid transition, and stamping an unpaired device through
            // a synthetic Paired state forged `paired_at` and emitted a fake
            // pairing event (found in the P6 review).
            let mut device = self.registry.get(device_id).await?;
            let old = device.state;
            device.state = DeviceState::Connected;
            device.state_since = Utc::now();
            self.registry.update(device).await?;
            info!(
                device_id = %device_id,
                from = ?old,
                to = ?DeviceState::Connected,
                event = "device_fast_forwarded",
                "Fast-forwarded auto-registered device to valid predecessor state"
            );
            self.broadcaster.broadcast(DeviceEvent::StateChanged {
                device_id: device_id.clone(),
                old_state: old,
                new_state: DeviceState::Connected,
            });
        }

        self.transition(device_id, new_state).await
    }

    pub async fn try_transition(&self, device_id: &DeviceId, new_state: DeviceState) {
        match self.transition(device_id, new_state).await {
            Ok(()) => {}
            Err(Error::DeviceNotFound(_)) => {
                warn!(
                    device_id = %device_id,
                    target_state = ?new_state,
                    event = "lifecycle_transition_no_device",
                    "Lifecycle transition skipped: device not in registry"
                );
            }
            Err(Error::InvalidStateTransition { from, to }) => {
                warn!(
                    device_id = %device_id,
                    current_state = ?from,
                    target_state = ?to,
                    event = "lifecycle_transition_invalid",
                    "Lifecycle transition skipped: invalid state transition"
                );
            }
            Err(e) => {
                warn!(
                    device_id = %device_id,
                    target_state = ?new_state,
                    error = %e,
                    event = "lifecycle_transition_error",
                    "Lifecycle transition failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::{Device, DeviceType};

    fn setup() -> (Arc<DeviceRegistry>, Arc<EventBroadcaster>, LifecycleManager) {
        let registry = Arc::new(DeviceRegistry::new());
        let broadcaster = Arc::new(EventBroadcaster::new(16, "device"));
        let lifecycle = LifecycleManager::new(registry.clone(), broadcaster.clone());
        (registry, broadcaster, lifecycle)
    }

    fn test_device(id: &str) -> Device {
        Device::new(
            id.to_string(),
            format!("Device {}", id),
            DeviceType::Phone,
            7,
        )
    }

    #[tokio::test]
    async fn test_valid_transition_discovered_to_pairing() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");

        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Pairing)
            .await
            .expect("Value expected to be present");

        let state = lifecycle
            .get_state(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Pairing);
        Ok(())
    }

    #[tokio::test]
    async fn test_valid_full_lifecycle() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");

        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Pairing)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Paired)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Connected)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Disconnected)
            .await
            .expect("Value expected to be present");

        let state = lifecycle
            .get_state(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Disconnected);
        Ok(())
    }

    #[tokio::test]
    async fn test_discovered_to_connected_allowed() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");

        let result = lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Connected)
            .await;
        assert!(result.is_ok());

        let state = lifecycle
            .get_state(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Connected);
        Ok(())
    }

    #[tokio::test]
    async fn test_connected_to_discovered_is_invalid() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Connected)
            .await
            .expect("Value expected to be present");

        let result = lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Discovered)
            .await;
        assert!(result.is_err());

        let state = lifecycle
            .get_state(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Connected);
        Ok(())
    }

    #[tokio::test]
    async fn test_transition_emits_event() -> anyhow::Result<()> {
        let (registry, broadcaster, lifecycle) = setup();
        let mut rx = broadcaster.subscribe();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");

        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Pairing)
            .await
            .expect("Value expected to be present");

        let event = rx.recv().await.expect("Value expected to be present");
        match event {
            DeviceEvent::StateChanged {
                device_id,
                old_state,
                new_state,
            } => {
                assert_eq!(device_id, "dev-1");
                assert_eq!(old_state, DeviceState::Discovered);
                assert_eq!(new_state, DeviceState::Pairing);
            }
            _ => panic!("Expected StateChanged event"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_pair_requested_event_is_broadcast() {
        let (_registry, broadcaster, _lifecycle) = setup();
        let mut rx = broadcaster.subscribe();

        broadcaster.broadcast(DeviceEvent::PairRequested {
            device_id: "evt-peer-aaaaaaaaaaaaaaaaaaaaaa".to_string(),
            device_name: "test phone".to_string(),
        });

        let event = rx.recv().await.expect("event delivered");
        match event {
            DeviceEvent::PairRequested {
                device_id,
                device_name,
            } => {
                assert_eq!(device_id, "evt-peer-aaaaaaaaaaaaaaaaaaaaaa");
                assert_eq!(device_name, "test phone");
            }
            other => panic!("expected PairRequested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_transition_nonexistent_device() -> anyhow::Result<()> {
        let (_, _, lifecycle) = setup();
        let result = lifecycle
            .transition(&"nope".to_string(), DeviceState::Pairing)
            .await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_paired_at_set_on_pairing() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");

        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Pairing)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Paired)
            .await
            .expect("Value expected to be present");

        let device = registry
            .get(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert!(device.paired_at.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn test_reconnect_flow() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");

        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Pairing)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Paired)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Connected)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Disconnected)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"dev-1".to_string(), DeviceState::Paired)
            .await
            .expect("Value expected to be present");

        let state = lifecycle
            .get_state(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Paired);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_and_transition_creates_device() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        let identity = Identity::new(
            "new-device".to_string(),
            "New Device".to_string(),
            DeviceType::Phone,
            vec![],
            vec![],
        );

        lifecycle
            .ensure_and_transition(&"new-device".to_string(), &identity, DeviceState::Connected)
            .await
            .expect("Value expected to be present");

        assert!(registry.contains(&"new-device".to_string()).await);
        let state = lifecycle
            .get_state(&"new-device".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Connected);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_and_transition_existing_paired_device() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("existing"))
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"existing".to_string(), DeviceState::Pairing)
            .await
            .expect("Value expected to be present");
        lifecycle
            .transition(&"existing".to_string(), DeviceState::Paired)
            .await
            .expect("Value expected to be present");

        let identity = Identity::new(
            "existing".to_string(),
            "Existing".to_string(),
            DeviceType::Phone,
            vec![],
            vec![],
        );

        lifecycle
            .ensure_and_transition(&"existing".to_string(), &identity, DeviceState::Connected)
            .await
            .expect("Value expected to be present");

        let state = lifecycle
            .get_state(&"existing".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Connected);
        Ok(())
    }

    #[tokio::test]
    async fn test_try_transition_nonexistent_device_succeeds() -> anyhow::Result<()> {
        let (_, _, lifecycle) = setup();
        lifecycle
            .try_transition(&"nope".to_string(), DeviceState::Disconnected)
            .await;
        Ok(())
    }

    #[tokio::test]
    async fn test_try_transition_connected_from_disconnected() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        registry
            .add(test_device("dev-1"))
            .await
            .expect("Value expected to be present");
        lifecycle
            .try_transition(&"dev-1".to_string(), DeviceState::Connected)
            .await;
        let state = lifecycle
            .get_state(&"dev-1".to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(state, DeviceState::Connected);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_and_transition_concurrent_calls_succeed() -> anyhow::Result<()> {
        let (registry, _, lifecycle) = setup();
        let identity = Identity::new(
            "race-device".to_string(),
            "Race Device".to_string(),
            DeviceType::Phone,
            vec![],
            vec![],
        );

        let lc1 = lifecycle.clone();
        let lc2 = lifecycle.clone();
        let ident1 = identity.clone();
        let ident2 = identity.clone();
        let device_id = "race-device".to_string();

        let (r1, r2) = tokio::join!(
            lc1.ensure_and_transition(&device_id, &ident1, DeviceState::Connected),
            lc2.ensure_and_transition(&device_id, &ident2, DeviceState::Connected),
        );

        assert!(
            r1.is_ok() || r2.is_ok(),
            "At least one concurrent call must succeed"
        );
        assert!(
            registry.contains(&device_id).await,
            "Device must exist after concurrent calls"
        );
        let state = lifecycle
            .get_state(&device_id)
            .await
            .expect("Value expected to be present");
        assert_eq!(
            state,
            DeviceState::Connected,
            "Device must end up in Connected state"
        );
        Ok(())
    }
}

#[cfg(test)]
mod p6_tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_auto_registered_connect_does_not_forge_paired() -> anyhow::Result<()> {
        // P6: an auto-registered device transitioning straight to Connected
        // must NOT be stamped through a synthetic Paired state — no fake
        // paired_at, and the final state is Connected via the valid
        // Discovered→Connected transition.
        let registry = Arc::new(DeviceRegistry::new());
        let broadcaster = Arc::new(EventBroadcaster::new(16, "device"));
        let lifecycle = LifecycleManager::new(registry.clone(), broadcaster);
        let identity = crate::protocol::types::Identity::new(
            "auto-device-aaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            "Auto".to_string(),
            crate::device::types::DeviceType::Phone,
            vec![],
            vec![],
        );

        lifecycle
            .ensure_and_transition(
                &"auto-device-aaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                &identity,
                DeviceState::Connected,
            )
            .await
            .expect("transition");

        let device = registry
            .get(&"auto-device-aaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
            .expect("device");
        assert_eq!(device.state, DeviceState::Connected);
        assert!(
            device.paired_at.is_none(),
            "paired_at must never be forged by a connection"
        );
        Ok(())
    }
}

#[cfg(test)]
mod record_freshness_tests {
    use super::*;
    use crate::device::types::{Device, DeviceType};

    fn setup() -> (LifecycleManager, Arc<DeviceRegistry>) {
        let registry = Arc::new(DeviceRegistry::new());
        let broadcaster = Arc::new(EventBroadcaster::new(16, "device"));
        let lifecycle = LifecycleManager::new(registry.clone(), broadcaster);
        (lifecycle, registry)
    }

    /// `paired_at` is presented by the API as when the device is paired, and
    /// `paired.json` records the current pairing. Setting it only when it was
    /// previously None made it "first ever paired", so the two stores disagreed:
    /// observed 2026-07-30, the device record said 2026-04-07 while paired.json
    /// said the same day the phone had actually re-paired.
    #[tokio::test]
    async fn test_repairing_updates_paired_at() -> anyhow::Result<()> {
        let (manager, registry) = setup();
        let id = "aaaabbbbccccddddeeeeffff00004444".to_string();
        registry
            .add(Device::new(
                id.clone(),
                "phone".to_string(),
                DeviceType::Phone,
                8,
            ))
            .await?;

        // Discovered -> Pairing -> Paired is the legal first pairing path
        // (Device::can_transition_to).
        manager.transition(&id, DeviceState::Pairing).await?;
        manager.transition(&id, DeviceState::Paired).await?;
        let first = registry
            .get(&id)
            .await?
            .paired_at
            .ok_or_else(|| anyhow::anyhow!("No paired_at"))?;

        // Paired -> Connected -> Disconnected -> Paired is the legal re-pair path.
        manager.transition(&id, DeviceState::Connected).await?;
        manager.transition(&id, DeviceState::Disconnected).await?;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        manager.transition(&id, DeviceState::Paired).await?;

        let second = registry
            .get(&id)
            .await?
            .paired_at
            .ok_or_else(|| anyhow::anyhow!("No paired_at"))?;
        assert!(
            second > first,
            "re-pairing must restamp paired_at (was {first}, now {second})"
        );
        Ok(())
    }

    /// `last_seen` on a device we are actively talking to must not be hours old.
    /// It was only bumped by discovery upserts, so a device that stayed connected
    /// kept drifting: observed 2026-07-30, state_since 21:09 with last_seen 14:48
    /// on a live connection.
    #[tokio::test]
    async fn test_transition_updates_last_seen() -> anyhow::Result<()> {
        let (manager, registry) = setup();
        let id = "aaaabbbbccccddddeeeeffff00005555".to_string();
        let mut device = Device::new(id.clone(), "phone".to_string(), DeviceType::Phone, 8);
        device.last_seen = chrono::Utc::now() - chrono::Duration::hours(6);
        let stale = device.last_seen;
        registry.add(device).await?;

        manager.transition(&id, DeviceState::Connected).await?;

        let got = registry.get(&id).await?;
        assert!(
            got.last_seen > stale,
            "a lifecycle transition is contact; last_seen must advance"
        );
        Ok(())
    }
}

#[cfg(test)]
mod inbound_capability_tests {
    use super::*;
    use crate::device::types::DeviceType;

    fn setup() -> (Arc<DeviceRegistry>, Arc<EventBroadcaster>, LifecycleManager) {
        let registry = Arc::new(DeviceRegistry::new());
        let broadcaster = Arc::new(EventBroadcaster::new(16, "device"));
        let lifecycle = LifecycleManager::new(registry.clone(), broadcaster.clone());
        (registry, broadcaster, lifecycle)
    }

    fn test_device(id: &str) -> Device {
        Device::new(id.to_string(), "phone".to_string(), DeviceType::Phone, 8)
    }

    fn identity(id: &str) -> Identity {
        Identity {
            device_id: id.to_string(),
            device_name: "test phone".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 8,
            incoming_capabilities: vec!["kdeconnect.notification".to_string()],
            outgoing_capabilities: vec!["kdeconnect.battery".to_string()],
            tcp_port: None,
            target_device_id: None,
            target_protocol_version: None,
        }
    }

    /// `ensure_and_transition` is the path a phone takes when it dials US, which
    /// is how an already-paired handset reconnects. It built the device without
    /// the identity's capabilities, so the UDP-discovery fix alone left connected
    /// devices still advertising nothing (observed live 2026-07-30: capability
    /// lists empty on a connected, paired phone after the discovery-site fix).
    #[tokio::test]
    async fn test_auto_registered_device_carries_identity_capabilities() -> anyhow::Result<()> {
        let (registry, _b, manager) = setup();
        let id = "aaaabbbbccccddddeeeeffff00006666".to_string();

        manager
            .ensure_and_transition(&id, &identity(&id), DeviceState::Connected)
            .await?;

        let got = registry.get(&id).await?;
        assert_eq!(got.incoming_capabilities, vec!["kdeconnect.notification"]);
        assert_eq!(got.outgoing_capabilities, vec!["kdeconnect.battery"]);
        Ok(())
    }

    /// A device we already know reconnects with a current identity. Its
    /// capabilities must refresh, or a record created before this fix keeps its
    /// empty lists forever.
    #[tokio::test]
    async fn test_existing_device_capabilities_refresh_on_reconnect() -> anyhow::Result<()> {
        let (registry, _b, manager) = setup();
        let id = "aaaabbbbccccddddeeeeffff00007777".to_string();
        registry.add(test_device(&id)).await?;
        assert!(registry.get(&id).await?.incoming_capabilities.is_empty());

        manager
            .ensure_and_transition(&id, &identity(&id), DeviceState::Connected)
            .await?;

        let got = registry.get(&id).await?;
        assert_eq!(
            got.incoming_capabilities,
            vec!["kdeconnect.notification"],
            "a reconnect carries a current identity; the record must adopt it"
        );
        Ok(())
    }

    /// Gap 3's guard must hold on THIS path too (PR #12 review): the
    /// reconnect leg assigned both capability lists directly, bypassing the
    /// empty-cap guard that upsert_device applies — so a crafted empty-cap
    /// identity arriving over the connection path could still wipe learned
    /// capabilities. Both identity-update paths now route through
    /// Device::apply_capability_update.
    #[tokio::test]
    async fn test_reconnect_empty_capabilities_do_not_clobber_known_ones() -> anyhow::Result<()> {
        let (registry, _b, manager) = setup();
        let id = "aaaabbbbccccddddeeeeffff00008888".to_string();

        // Learn real capabilities first (the normal reconnect above).
        manager
            .ensure_and_transition(&id, &identity(&id), DeviceState::Connected)
            .await?;
        assert!(!registry.get(&id).await?.incoming_capabilities.is_empty());

        // A crafted identity with empty capability lists reconnects.
        let mut hostile = identity(&id);
        hostile.incoming_capabilities = Vec::new();
        hostile.outgoing_capabilities = Vec::new();
        manager
            .ensure_and_transition(&id, &hostile, DeviceState::Connected)
            .await?;

        let got = registry.get(&id).await?;
        assert_eq!(
            got.incoming_capabilities,
            vec!["kdeconnect.notification"],
            "an empty-cap identity must not wipe learned capabilities (kde core/device.cpp:319-328)"
        );
        assert_eq!(got.outgoing_capabilities, vec!["kdeconnect.battery"]);
        Ok(())
    }
}

#[cfg(test)]
mod lane_c_repro_tests {
    //! Lane-C repro (audit-2026-09-02-remaining-brief.md, section C, lead
    //! 2): `LifecycleManager::transition` is a get-then-mutate-then-write
    //! sequence (`registry.get` at :31, `registry.update` at :67, two
    //! separate lock acquisitions) and `DeviceRegistry::update`
    //! (registry.rs :245-251) is a blind insert with no compare-and-swap.
    //! Two concurrent transitions off the SAME starting state race: both
    //! `get()`s can observe the same pre-race state, both independently
    //! validate and broadcast a `StateChanged` event, but only the
    //! LAST `update()` to land actually persists — the other caller's
    //! reported-successful transition is silently discarded.
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::{Device, DeviceType};

    fn setup() -> (Arc<DeviceRegistry>, Arc<EventBroadcaster>, LifecycleManager) {
        let registry = Arc::new(DeviceRegistry::new());
        let broadcaster = Arc::new(EventBroadcaster::new(16, "device"));
        let lifecycle = LifecycleManager::new(registry.clone(), broadcaster.clone());
        (registry, broadcaster, lifecycle)
    }

    /// Multi-thread runtime: the race window between `registry.get` and
    /// `registry.update` is a handful of nanoseconds with no forced yield
    /// in between, so a single-threaded runtime would never preempt one
    /// task mid-transition to let the other interleave. Real OS-thread
    /// parallelism (as `contacts.rs`'s flood tests already use) is what
    /// gives the two lock acquisitions a chance to interleave for real.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_transitions_from_same_state_lose_an_update() {
        const ITERATIONS: usize = 50;
        let mut both_succeeded = 0usize;
        let mut lost_updates = 0usize;

        for i in 0..ITERATIONS {
            let (registry, broadcaster, lifecycle) = setup();
            let id = format!("race-dev-{i:03}-aaaaaaaaaaaaaaaaaaaa");
            registry
                .add(Device::new(
                    id.clone(),
                    "phone".to_string(),
                    DeviceType::Phone,
                    8,
                ))
                .await
                .expect("Value expected to be present");

            // Walk to Paired sequentially (no race here) so both racing
            // calls below start from the SAME valid predecessor state:
            // (Paired, Connected) and (Paired, Pairing) are both valid
            // per DeviceState::can_transition_to.
            lifecycle
                .transition(&id, DeviceState::Pairing)
                .await
                .expect("Value expected to be present");
            lifecycle
                .transition(&id, DeviceState::Paired)
                .await
                .expect("Value expected to be present");

            let mut events = broadcaster.subscribe();

            let lifecycle_a = lifecycle.clone();
            let id_a = id.clone();
            let task_a =
                tokio::spawn(
                    async move { lifecycle_a.transition(&id_a, DeviceState::Connected).await },
                );
            let lifecycle_b = lifecycle.clone();
            let id_b = id.clone();
            let task_b =
                tokio::spawn(
                    async move { lifecycle_b.transition(&id_b, DeviceState::Pairing).await },
                );

            let (res_a, res_b) = tokio::join!(task_a, task_b);
            let res_a = res_a.expect("Value expected to be present");
            let res_b = res_b.expect("Value expected to be present");

            if res_a.is_err() || res_b.is_err() {
                // Serialized naturally this iteration (one call observed
                // the other's committed state and got a legitimately
                // invalid transition) — not the race window we're
                // after, move on.
                continue;
            }
            both_succeeded += 1;

            // Drain every StateChanged event this device produced during
            // the race.
            let mut seen_targets = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let DeviceEvent::StateChanged {
                    device_id,
                    old_state,
                    new_state,
                } = event
                {
                    if device_id == id {
                        seen_targets.push((old_state, new_state));
                    }
                }
            }
            assert_eq!(
                seen_targets.len(),
                2,
                "iteration {i}: both transitions returned Ok(()), both must have broadcast a StateChanged event, got {seen_targets:?}"
            );

            let final_state = registry
                .get(&id)
                .await
                .expect("Value expected to be present")
                .state;

            assert!(
                final_state == DeviceState::Connected || final_state == DeviceState::Pairing,
                "iteration {i}: final state {final_state:?} must be one of the two racing targets"
            );

            // Two Ok(())s are legitimate when the calls serialized: the
            // second one validated against the FIRST one's result (its
            // event's old_state is the first one's new_state). A lost
            // update is when both events claim the same old_state — both
            // read the same snapshot, both "succeeded", and the record can
            // only hold one of them.
            let same_snapshot = seen_targets[0].0 == seen_targets[1].0;
            if same_snapshot {
                lost_updates += 1;
                eprintln!(
                    "iteration {i}: both transition() calls returned Ok(()) from the same snapshot (events: {seen_targets:?}) but the registry only holds {final_state:?} — the other caller's reported-successful transition was lost"
                );
            }
        }

        eprintln!(
            "both-succeeded (raced) iterations: {both_succeeded}/{ITERATIONS}, lost-update iterations: {lost_updates}"
        );
        assert_eq!(
            lost_updates, 0,
            "LifecycleManager::transition's get-then-update TOCTOU lost {lost_updates}/{both_succeeded} racing updates that both reported success \
             (registry.rs update() at :245-251 is a blind insert with no compare-and-swap against the version `transition` originally read)"
        );
    }
}

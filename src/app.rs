//! Application state shared across all components
//!
//! Single Responsibility: Hold and provide access to shared state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::config::settings::AppSettings;
use crate::device::{DeviceRegistry, EventBroadcaster, LifecycleManager};
use crate::plugins::{PluginAccess, PluginEventBroadcaster, PluginRegistry};
use crate::protocol::{CertificateManager, ConnectionManager, PacketRouter, PairingHandler};

pub struct AppState {
    pub settings: AppSettings,
    pub registry: Arc<DeviceRegistry>,
    pub lifecycle: Arc<LifecycleManager>,
    pub broadcaster: Arc<EventBroadcaster>,
    pub cert_manager: Arc<CertificateManager>,
    pub connection_manager: Arc<ConnectionManager>,
    pub pairing_handler: Arc<PairingHandler>,
    pub packet_router: Arc<PacketRouter>,
    pub plugin_registry: Arc<PluginRegistry>,
    pub plugin_events: Arc<PluginEventBroadcaster>,
    pub shutdown: CancellationToken,
    pub started_at: Instant,
    pub plugins: PluginAccess,
}

impl AppState {
    pub fn new(settings: AppSettings) -> crate::utils::Result<Self> {
        Self::new_inner(settings, true)
    }

    /// Build application state without opening desktop input devices.
    pub fn new_without_input(settings: AppSettings) -> crate::utils::Result<Self> {
        Self::new_inner(settings, false)
    }

    fn new_inner(settings: AppSettings, enable_input: bool) -> crate::utils::Result<Self> {
        settings.ensure_dirs_exist()?;

        let cert_manager = Arc::new(CertificateManager::new(settings.cert_dir.clone()));
        cert_manager.init()?;

        let pairing_path = settings.data_dir.join("paired.json");
        // No with_timeout override: the requester/accepter timeouts stay at
        // the Android-conformant 30s/25s defaults (PairingHandler.kt:151/88).
        // The old pairing_timeout_mins config (default 30 MINUTES) let a
        // stale outgoing request outlive the phone's 25s accepter timeout,
        // which broke a live pair round vs a test phone on 2026-07-30: the
        // phone's fresh request was mis-classified as the accept of our
        // expired one, we answered with plugin traffic instead of pair=true,
        // and the unpaired phone correctly rejected with pair=false.
        //
        // The broadcaster is wired into the handler (Task #1042 fix lane
        // E — pairing-event seam) so accept / force-accept emit
        // `DeviceEvent::Paired` and `unpair` emits `DeviceEvent::Unpaired`;
        // capability gates that filter on `is_paired` re-evaluate on those.
        // ONE pairing handler, fully wired (persistence + broadcaster)
        // BEFORE the registry borrows its paired-ids handle. The handle is
        // the inner `Arc` of THIS handler's paired map; a second
        // `PairingHandler::new` allocates a fresh map, and a registry
        // holding the first handler's handle never sees a real pairing —
        // devices.json was written as `{}` on every save for a month
        // (2026-09-02 audit A2, pinned by `tests::test_registry_persists_
        // a_device_the_live_pairing_handler_marks_paired`).
        let broadcaster = Arc::new(EventBroadcaster::new(256, "device"));
        let pairing_handler = Arc::new(
            PairingHandler::new(cert_manager.clone())
                .with_persistence(pairing_path)
                .with_broadcaster(broadcaster.clone()),
        );

        // The registry needs to tell truly-paired devices from pre-auth
        // ones that merely reached Connected (finding L2-1, Sprint 2
        // security audit), so it shares the live handler's paired map.
        let devices_path = settings.data_dir.join("devices.json");
        let registry = Arc::new(
            DeviceRegistry::with_persistence(devices_path)
                .with_paired_source(pairing_handler.paired_handle()),
        );
        let lifecycle = Arc::new(LifecycleManager::new(registry.clone(), broadcaster.clone()));

        let connection_manager = Arc::new(ConnectionManager::new(cert_manager.clone())?);
        let packet_router = Arc::new(PacketRouter::new());
        let plugin_registry = Arc::new(PluginRegistry::new());
        let plugin_events = Arc::new(PluginEventBroadcaster::new(256, "plugin"));
        let shutdown = CancellationToken::new();
        let started_at = Instant::now();

        let plugins = crate::plugins::load_default_plugins(
            plugin_events.clone(),
            cert_manager.clone(),
            connection_manager.clone(),
            pairing_handler.clone(),
            settings.data_dir.clone(),
            enable_input,
            broadcaster.clone(),
        );

        // Populate the runcommand allowlist from the config file at boot.
        // There is intentionally no runtime write path (no REST, no DBus,
        // no signal handler) — the allowlist can only change by editing
        // the config and restarting the daemon. Empty config keeps the
        // prior safe-by-default behavior (every request blocked).
        plugins
            .runcommand
            .register_from_config(&settings.runcommand);

        Ok(Self {
            settings,
            registry,
            lifecycle,
            broadcaster,
            cert_manager,
            connection_manager,
            pairing_handler,
            packet_router,
            plugin_registry,
            plugin_events,
            shutdown,
            started_at,
            plugins,
        })
    }

    pub async fn init_plugins(&self) {
        crate::plugins::load_all(&self.plugin_registry, &self.plugins).await;

        let plugin_registry = self.plugin_registry.clone();
        let packet_router = self.packet_router.clone();
        for plugin_name in self.plugin_registry.list().await {
            if let Some(plugin) = plugin_registry.get(&plugin_name).await {
                for cap in plugin.incoming_capabilities() {
                    let plugin = plugin.clone();
                    packet_router
                        .register(&cap, move |device_id, packet| {
                            let device_id = device_id.to_string();
                            let plugin = plugin.clone();
                            async move { plugin.handle_packet(&device_id, packet).await }
                        })
                        .await;
                }
            }
        }
    }

    pub async fn initialize(&self) -> crate::utils::Result<()> {
        self.init_plugins().await;

        let plugins = self.plugin_registry.list().await;
        tracing::info!(
            plugin_count = plugins.len(),
            plugins = ?plugins,
            event = "plugins_loaded",
            "Plugins loaded and wired to packet router"
        );

        Ok(())
    }

    /// Notify plugins that a device is connected-and-paired and send their
    /// connect-time advertisements (runcommand's command list, …) over the
    /// live link. MUST be called on every path that completes pairing on an
    /// established connection: the connect-time notify in the orchestrator/
    /// listener fires only for devices that were ALREADY paired at connect
    /// time, so a connection that pairs after connecting gets its init
    /// packets only from the pairing-completion path (late-pairing plugin init).
    /// Advertise every plugin's connect-time packets to a device.
    ///
    /// Retries across a brief window rather than firing once. Pairing
    /// completes at the exact moment the phone is most likely to replace the
    /// link — it redials on a fixed cadence and resets its link around
    /// pairing — and a single blind pass loses the whole advertisement set to
    /// a handle that was just evicted. The phone is then paired with no
    /// features until the next connect re-advertises, which reads as a broken
    /// app for up to a full redial cycle.
    ///
    /// Bounded on purpose: callers include the pairing path, which must not
    /// hang. When every pass fails, the connect-time path in the listener
    /// still re-advertises on the next connection — this only closes the gap
    /// until then.
    pub async fn send_plugin_init_packets(&self, device_id: &crate::device::types::DeviceId) {
        const ATTEMPTS: usize = 3;
        const LINK_WAIT: Duration = Duration::from_secs(2);
        const POLL: Duration = Duration::from_millis(250);

        // Skip the whole pass if the device is no longer paired. The
        // re-establish path runs whenever a TCP connection becomes
        // available — including the post-unpair reconnect, where the
        // peer came back up but our pair state is gone. Sending plugin
        // init packets to an unpaired device re-triggers the kdeconnectd
        // Device::privateReceivedPacket unpair loop (device.cpp:391-394)
        // — every non-pair packet from an unpaired device re-emits
        // unpaired() and disk-writes the trust file. M2 finding
        // (vk #991): the M2 test's inter-phase unpair saw a 12+ cycle
        // init-packet / unpair / unpair storm that wedged the kde
        // PairingHandler queue.
        if !self.pairing_handler.is_paired(device_id).await {
            tracing::debug!(
                device_id = %device_id,
                event = "init_packets_skipped_unpaired",
                "Skipping plugin init packets: device is not paired"
            );
            return;
        }

        for attempt in 1..=ATTEMPTS {
            // Wait for a live link before spending the pass. A replacement in
            // flight resolves within a poll or two; a genuinely absent link
            // burns the whole window and we fall through to the next attempt.
            let deadline = tokio::time::Instant::now() + LINK_WAIT;
            while !self.connection_manager.is_connected(device_id).await
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(POLL).await;
            }

            // Re-generate per pass: a fresh link deserves a fresh
            // advertisement set, which is exactly what the connect-time path
            // sends on every connection.
            let packets = self.plugin_registry.notify_connected(device_id).await;
            let mut failed = 0usize;
            for pkt in &packets {
                if let Err(e) = self.connection_manager.send_packet(device_id, pkt).await {
                    failed += 1;
                    tracing::debug!(
                        device_id = %device_id,
                        packet_type = %pkt.packet_type,
                        error = %e,
                        attempt,
                        event = "init_packet_send_retrying",
                        "Init packet send failed; will retry if attempts remain"
                    );
                }
            }

            if failed == 0 {
                return;
            }

            tracing::debug!(
                device_id = %device_id,
                attempt,
                failed,
                total = packets.len(),
                event = "init_packet_pass_incomplete",
                "Plugin init pass did not complete"
            );
        }

        tracing::warn!(
            device_id = %device_id,
            attempts = ATTEMPTS,
            event = "init_packets_undelivered",
            "Could not deliver plugin init packets; the device will get them \
             when it next connects, but may show no features until then"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::{Device, DeviceType};

    /// Regression for the 2026-09-02 audit finding A2: `AppState::new`
    /// built the registry's paired-ids handle from one `PairingHandler`
    /// and then stored a DIFFERENT `PairingHandler` in the state, so the
    /// registry never saw a real pairing and `devices.json` was always
    /// written as `{}`. The registry's own unit tests wire the handle by
    /// hand and could not catch it; only a test through `AppState` can.
    #[tokio::test]
    async fn test_registry_persists_a_device_the_live_pairing_handler_marks_paired() {
        let temp = tempfile::TempDir::new().unwrap();
        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf());
        let state = AppState::new_without_input(settings).unwrap();

        let id = "paired-device-aaaaaaaaaaaaaaaaaaaaa".to_string();
        state
            .pairing_handler
            .paired_handle()
            .write()
            .await
            .insert(id.clone(), chrono::Utc::now());

        let device = Device::new(id.clone(), "Phone".to_string(), DeviceType::Phone, 8);
        state.registry.add(device).await.unwrap();

        let json = std::fs::read_to_string(temp.path().join("devices.json")).unwrap();
        assert!(
            json.contains(&id),
            "devices.json must carry a device the live pairing handler says is paired; got {json}"
        );
    }
}

//! Daemon orchestration
//!
//! Single Responsibility: Coordinate application lifecycle.
//! Delegates bootstrap to bootstrap module, services to service_manager.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::app::AppState;
use crate::bootstrap;
use crate::device::DeviceType;
use crate::protocol::Identity;
use crate::services::service_manager;
use crate::utils::Result;

pub struct Daemon {
    state: Arc<AppState>,
}

impl Daemon {
    pub async fn new() -> Result<Self> {
        Self::new_with_overrides(None, None, None, None, None, false, None).await
    }

    // CLI overrides arrive as a flat list of Options; grouping them into a struct is
    // deferred to the planned config rework, so silence the arity lint here.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_overrides(
        config_path: Option<&str>,
        port: Option<u16>,
        api_port: Option<u16>,
        log_level: Option<&str>,
        device_name: Option<&str>,
        no_api: bool,
        idle_timeout_secs: Option<u64>,
    ) -> Result<Self> {
        let settings = bootstrap::load_config(
            config_path,
            port,
            api_port,
            log_level,
            device_name,
            no_api,
            idle_timeout_secs,
        )?;

        let state = bootstrap::create_state(settings).await?;

        Ok(Self { state })
    }

    pub async fn run(self) -> Result<()> {
        let identity = self.load_identity().await?;
        let shutdown = self.state.shutdown.clone();

        let handles =
            service_manager::start_services(self.state.clone(), identity, shutdown.clone()).await?;

        wait_for_shutdown_signal(&shutdown).await;

        info!(
            event = "daemon_shutting_down",
            "Shutting down Rust Connect daemon"
        );
        shutdown.cancel();

        service_manager::stop_services(handles, &self.state).await;

        // Release every active SFTP mount and drop every stored
        // credential. The startup sweep will pick up anything we miss
        // on the next boot — this is the "clean exit" leg. Runs AFTER
        // stop_services so no new mount/unmount requests can race in.
        self.state.plugins.sftp.cleanup_all().await;
        info!(
            event = "sftp_cleaned_up",
            "Released SFTP mounts and credentials"
        );

        info!(event = "daemon_stopped", "Daemon stopped successfully");
        Ok(())
    }

    async fn load_identity(&self) -> Result<Identity> {
        let own = crate::protocol::own_identity::OwnIdentity::load_or_create(
            &self.state.cert_manager,
            &self.state.settings.data_dir,
            &self.state.settings.device_name,
        )?;
        let device_id = own.device_id.clone();

        self.state
            .connection_manager
            .set_device_identity(&device_id, &self.state.settings.device_name);
        self.state
            .pairing_handler
            .set_own_device_id(device_id.clone())
            .await;

        let mut incoming_caps: Vec<String> = Vec::new();
        let mut outgoing_caps: Vec<String> = Vec::new();
        for plugin_name in self.state.plugin_registry.list().await {
            if let Some(plugin) = self.state.plugin_registry.get(&plugin_name).await {
                incoming_caps.extend(plugin.incoming_capabilities());
                outgoing_caps.extend(plugin.outgoing_capabilities());
            }
        }
        incoming_caps.sort();
        incoming_caps.dedup();
        outgoing_caps.sort();
        outgoing_caps.dedup();

        self.state
            .connection_manager
            .set_capabilities(incoming_caps.clone(), outgoing_caps.clone());

        info!(
            incoming_count = incoming_caps.len(),
            outgoing_count = outgoing_caps.len(),
            event = "capabilities_collected",
            "Collected plugin capabilities for identity advertisement"
        );

        Ok(Identity::new(
            device_id,
            self.state.settings.device_name.clone(),
            DeviceType::Desktop,
            incoming_caps,
            outgoing_caps,
        ))
    }
}

/// Waits for SIGINT, SIGTERM, or external shutdown signal.
async fn wait_for_shutdown_signal(shutdown: &CancellationToken) {
    #[cfg(unix)]
    {
        let ctrl_c = tokio::signal::ctrl_c();
        #[allow(clippy::expect_used)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to create SIGTERM signal");

        tokio::select! {
            _ = ctrl_c => {

                info!(event = "sigint_received", "Received SIGINT (Ctrl+C)");
            }
            _ = sigterm.recv() => {

                info!(event = "sigterm_received", "Received SIGTERM");
            }
            _ = shutdown.cancelled() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {

                info!(event = "sigint_received", "Received SIGINT (Ctrl+C)");
            }
            _ = shutdown.cancelled() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use crate::services::connection_orchestrator;

    #[test]
    fn test_backoff_delay_doubles_each_attempt() {
        use std::time::Duration;
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        let d1 = connection_orchestrator::backoff_delay(1, base, cap);
        assert!(d1.as_secs() >= 1 && d1.as_secs() <= 1);
        let d2 = connection_orchestrator::backoff_delay(2, base, cap);
        assert!(d2.as_millis() >= 2000 && d2.as_millis() <= 3000);
        let d3 = connection_orchestrator::backoff_delay(3, base, cap);
        assert!(d3.as_millis() >= 4000 && d3.as_millis() <= 6000);
        let d4 = connection_orchestrator::backoff_delay(4, base, cap);
        assert!(d4.as_millis() >= 8000 && d4.as_millis() <= 12000);
        let d5 = connection_orchestrator::backoff_delay(5, base, cap);
        assert!(d5.as_millis() >= 16000 && d5.as_millis() <= 24000);
    }

    #[test]
    fn test_backoff_delay_capped_at_max() {
        use std::time::Duration;
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        let d6 = connection_orchestrator::backoff_delay(6, base, cap);
        assert!(d6.as_secs() >= 30 && d6.as_secs() <= 45);
        let d10 = connection_orchestrator::backoff_delay(10, base, cap);
        assert!(d10.as_secs() >= 30 && d10.as_secs() <= 45);
    }

    #[test]
    fn test_backoff_delay_first_attempt_is_base() {
        use std::time::Duration;
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        let d1 = connection_orchestrator::backoff_delay(1, base, cap);
        assert!(d1.as_secs() >= 1 && d1.as_secs() <= 1);
    }
}

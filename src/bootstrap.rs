//! Application bootstrap
//!
//! Single Responsibility: Load configuration, apply overrides, initialize state.

use std::sync::Arc;

use tracing::{info, warn};

use crate::app::AppState;
use crate::config::settings::AppSettings;
use crate::utils::{init_logging_from_env, Result};

/// Loads configuration from file or defaults, applies CLI overrides.
///
/// An explicit `--config` path wins. Without one, the default config path
/// (`~/.config/rust-connect/config.toml`) is loaded when it exists — the
/// shipped systemd unit passes no `--config`, so without this fallback the
/// documented "edit config.toml and restart" flow configures nothing.
pub fn load_config(
    config_path: Option<&str>,
    port: Option<u16>,
    api_port: Option<u16>,
    log_level: Option<&str>,
    device_name: Option<&str>,
    no_api: bool,
    idle_timeout_secs: Option<u64>,
) -> Result<AppSettings> {
    let mut settings = AppSettings::new();

    let default_path = AppSettings::config_path();
    match config_path {
        Some(path) => {
            let path_buf = std::path::PathBuf::from(path);
            if path_buf.exists() {
                let mut loaded = AppSettings::load_from_file(&path_buf)?;
                // A config file without api_keys must not silently disable auth:
                // give it the same persisted/generated-key treatment as the
                // default path.
                loaded.ensure_api_key();
                settings = loaded;
            } else {
                warn!(
                    path = path,
                    event = "config_not_found",
                    "Config file not found, using defaults"
                );
            }
        }
        None if default_path.exists() => {
            let mut loaded = AppSettings::load_from_file(&default_path)?;
            loaded.ensure_api_key();
            settings = loaded;
        }
        None => {}
    }

    if let Some(p) = port {
        settings.tcp_port = p;
        settings.udp_port = p;
    }
    if let Some(p) = api_port {
        settings.api_port = p;
    }
    if let Some(ll) = log_level {
        settings.log_level = ll.to_string();
    }
    if let Some(name) = device_name {
        settings.device_name = name.to_string();
    }
    if no_api {
        settings.api_enabled = false;
    }
    if let Some(timeout) = idle_timeout_secs {
        settings.idle_timeout_secs = timeout;
    }

    Ok(settings)
}

/// Creates and initializes application state from settings.
pub async fn create_state(settings: AppSettings) -> Result<Arc<AppState>> {
    init_logging_from_env(&settings.log_level, settings.log_max_files);

    settings.validate()?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        event = "daemon_starting",
        "Rust Connect daemon starting"
    );

    let state = Arc::new(AppState::new(settings.clone())?);

    // Enable the clipboard plugin's session backend (wl-copy/wl-paste) here,
    // at the production entry point only. AppState::new — which the test
    // suite exercises — deliberately leaves it disabled so tests never touch
    // the developer's live session clipboard. Degrades with a log event when
    // no Wayland session / wl-clipboard is available.
    state.plugins.clipboard.enable_session_backend();

    // Same production-only gate for the MPRIS session D-Bus backend (zbus).
    // Degrades with a log event when no session bus is reachable.
    state.plugins.mpris.enable_session_backend().await;

    // Same gate for pausemusic's session D-Bus pause/resume backend.
    state.plugins.pausemusic.enable_session_backend().await;

    // Gate for sendnotifications D-Bus watcher
    state.plugins.sendnotifications.enable_session_backend();

    // Same gate for screensaver-inhibit's session D-Bus backend.
    state
        .plugins
        .screensaver_inhibit
        .enable_session_backend()
        .await;

    // Same gate for the systemvolume provider's PulseAudio/PipeWire
    // backend (pactl). Degrades with a log event when pactl is missing
    // or the PA daemon is unreachable; the provider side then refuses
    // to advertise (Plugin::is_backend_available() returns false).
    // The registry handle powers the capability-gated peer sync.
    state
        .plugins
        .systemvolume
        .with_device_registry(state.registry.clone());
    state.plugins.systemvolume.enable_session_backend().await;

    info!(
        device_name = %state.settings.device_name,
        tcp_port = state.settings.tcp_port,
        udp_port = state.settings.udp_port,
        api_enabled = state.settings.api_enabled,
        event = "state_initialized",
        "Application state initialized"
    );

    for (i, key) in state.settings.api_keys.iter().enumerate() {
        info!(
            key_index = i,
            key_length = key.len(),
            event = "api_key_configured",
            "API key available - use with X-API-Key header"
        );
    }

    state.initialize().await?;
    load_persisted_data(&state).await;

    // Startup sweep: any SFTP mounts left mounted by a previous crash
    // must be released here, before the daemon accepts new connections.
    // The sweep uses fusermount3 only — it does NOT require sshfs to be
    // installed, so a fresh host that just installed the daemon still
    // gets a clean restart.
    let released = state.plugins.sftp.startup_sweep();
    if !released.is_empty() {
        info!(
            count = released.len(),
            mounts = ?released,
            event = "sftp_startup_sweep",
            "Released stale SFTP mounts from previous run"
        );
    }

    Ok(state)
}

/// Loads persisted device registry and pairing state from disk, then drops
/// device records that no longer correspond to anything.
async fn load_persisted_data(state: &Arc<AppState>) {
    match state.registry.load_from_disk().await {
        Ok(()) => {}
        Err(e) => {
            warn!(error = %e, "Failed to load device registry from disk, starting with empty state");
        }
    }
    match state.pairing_handler.load_from_disk().await {
        Ok(()) => {}
        Err(e) => {
            warn!(error = %e, "Failed to load pairing state from disk, starting with empty state");
        }
    }

    // Prune AFTER the pairing store has loaded — it is the authority for
    // what counts as paired, and an empty one would prune everything.
    let paired_ids: std::collections::HashSet<String> = state
        .pairing_handler
        .paired_devices()
        .await
        .into_iter()
        .collect();
    let pruned = state.registry.prune_stale_devices(&paired_ids).await;
    if !pruned.is_empty() {
        info!(
            count = pruned.len(),
            event = "stale_devices_pruned",
            "Pruned stale device records at startup"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    /// XDG env guard: `dirs::config_dir()`/`dirs::data_dir()` read these at
    /// call time, so tests point them at a tempdir and restore on drop —
    /// parallel tests must never see the override or the real home.
    struct XdgGuard {
        prev_config: Option<std::ffi::OsString>,
        prev_data: Option<std::ffi::OsString>,
    }

    impl XdgGuard {
        fn new(temp: &std::path::Path) -> Self {
            let guard = Self {
                prev_config: std::env::var_os("XDG_CONFIG_HOME"),
                prev_data: std::env::var_os("XDG_DATA_HOME"),
            };
            std::env::set_var("XDG_CONFIG_HOME", temp.join("config"));
            std::env::set_var("XDG_DATA_HOME", temp.join("data"));
            guard
        }
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            match &self.prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match &self.prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    /// Panel P1 (review-20260820T235242Z, codex + grok-46): without
    /// `--config`, the default config path was never read, so a documented
    /// `config.toml` edit configured nothing. One test, three sequential
    /// scenarios — the XDG env override is process-wide, so parallel tests
    /// would race on it.
    #[test]
    fn test_load_config_default_path_fallback() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _xdg = XdgGuard::new(temp.path());
        let cfg_dir = temp.path().join("config").join("rust-connect");

        // (a) No flag, default file present -> the default path is loaded.
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "device_name = \"from-default-path\"\napi_port = 19090\n",
        )
        .expect("write config");
        let settings = load_config(None, None, None, None, None, false, None)
            .expect("load_config should succeed");
        assert_eq!(settings.device_name, "from-default-path");
        assert_eq!(settings.api_port, 19090);

        // (b) An explicit `--config` path still wins over the default path.
        let explicit = temp.path().join("explicit.toml");
        std::fs::write(&explicit, "device_name = \"explicit-path\"\n").expect("write explicit");
        let settings = load_config(
            Some(explicit.to_str().expect("utf8 path")),
            None,
            None,
            None,
            None,
            false,
            None,
        )
        .expect("load_config should succeed");
        assert_eq!(settings.device_name, "explicit-path");

        // (c) No flag and no default file -> plain defaults, no error.
        std::fs::remove_file(cfg_dir.join("config.toml")).expect("remove config");
        let settings = load_config(None, None, None, None, None, false, None)
            .expect("load_config should succeed");
        assert_ne!(settings.device_name, "from-default-path");
        assert_eq!(settings.api_port, 9090);
    }
}

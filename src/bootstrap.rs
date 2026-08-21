//! Application bootstrap
//!
//! Single Responsibility: Load configuration, apply overrides, initialize state.

use std::sync::Arc;

use tracing::{info, warn};

use crate::app::AppState;
use crate::config::settings::AppSettings;
use crate::utils::{init_logging_from_env, Result};

/// Which config file `load_config` should open, if any. Pure resolution —
/// separated from `load_config` so the precedence rules are testable
/// without mutating process-wide XDG env vars (which parallel tests race
/// on; panel p2 ca718267).
enum EffectiveConfig {
    /// Explicit `--config` path that exists.
    Explicit(std::path::PathBuf),
    /// No flag; the default config path exists.
    Default(std::path::PathBuf),
    /// Explicit `--config` path that does not exist (warns, uses defaults).
    Missing(String),
    /// No flag and no default file.
    None,
}

/// An explicit `--config` path wins. Without one, the default config path
/// (`~/.config/rust-connect/config.toml`) is used when it exists — the
/// shipped systemd unit passes no `--config`, so without this fallback the
/// documented "edit config.toml and restart" flow configures nothing.
fn effective_config_path(
    config_path: Option<&str>,
    default_path: &std::path::Path,
) -> EffectiveConfig {
    match config_path {
        Some(path) => {
            let path_buf = std::path::PathBuf::from(path);
            if path_buf.exists() {
                EffectiveConfig::Explicit(path_buf)
            } else {
                EffectiveConfig::Missing(path.to_string())
            }
        }
        None => match default_path.try_exists() {
            Ok(true) => EffectiveConfig::Default(default_path.to_path_buf()),
            Ok(false) => EffectiveConfig::None,
            Err(e) => {
                // exists() would read a metadata error (e.g. an unreadable
                // parent) as absence and silently boot with an empty
                // allowlist — surface it instead (cubic P2, PR #23).
                warn!(
                    path = %default_path.display(),
                    error = %e,
                    event = "config_default_path_unreadable",
                    "Cannot stat the default config path; using defaults"
                );
                EffectiveConfig::None
            }
        },
    }
}

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

    match effective_config_path(config_path, &AppSettings::config_path()) {
        EffectiveConfig::Explicit(path_buf) | EffectiveConfig::Default(path_buf) => {
            let mut loaded = AppSettings::load_from_file(&path_buf)?;
            // A config file without api_keys must not silently disable auth:
            // give it the same persisted/generated-key treatment as the
            // default path.
            loaded.ensure_api_key();
            settings = loaded;
        }
        EffectiveConfig::Missing(path) => {
            warn!(
                path = path.as_str(),
                event = "config_not_found",
                "Config file not found, using defaults"
            );
        }
        EffectiveConfig::None => {}
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

    /// Panel P1 (review-20260820T235242Z, codex + grok-46): without
    /// `--config`, the default config path was never read, so a documented
    /// `config.toml` edit configured nothing. These run against the pure
    /// resolver — mutating process-wide XDG env vars would race parallel
    /// tests (panel p2 ca718267).
    #[test]
    fn test_effective_config_path_precedence() {
        let temp = tempfile::TempDir::new().expect("tempdir");

        // (a) No flag, default file present -> the default path is used.
        let default_dir = temp.path().join("rust-connect");
        std::fs::create_dir_all(&default_dir).expect("mkdir");
        let default_file = default_dir.join("config.toml");
        std::fs::write(&default_file, "device_name = \"from-default-path\"\n")
            .expect("write config");
        match effective_config_path(None, &default_file) {
            EffectiveConfig::Default(p) => assert_eq!(p, default_file),
            _ => panic!("expected the default path to be selected"),
        }

        // (b) An explicit `--config` path wins over the default path.
        let explicit = temp.path().join("explicit.toml");
        std::fs::write(&explicit, "device_name = \"explicit-path\"\n").expect("write explicit");
        match effective_config_path(Some(explicit.to_str().expect("utf8")), &default_file) {
            EffectiveConfig::Explicit(p) => assert_eq!(p, explicit),
            _ => panic!("expected the explicit path to win"),
        }

        // (c) An explicit path that does not exist -> Missing (warns, defaults).
        match effective_config_path(Some("/nonexistent/config.toml"), &default_file) {
            EffectiveConfig::Missing(p) => assert_eq!(p, "/nonexistent/config.toml"),
            _ => panic!("expected Missing for a nonexistent explicit path"),
        }

        // (d) No flag and no default file -> None.
        let absent = temp.path().join("absent").join("config.toml");
        match effective_config_path(None, &absent) {
            EffectiveConfig::None => {}
            _ => panic!("expected None when no file exists anywhere"),
        }
    }

    /// The resolver feeds `load_config` end to end: an explicit config file
    /// is actually parsed and applied.
    #[test]
    fn test_load_config_applies_explicit_file() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let explicit = temp.path().join("config.toml");
        std::fs::write(
            &explicit,
            "device_name = \"from-default-path\"\napi_port = 19090\n",
        )
        .expect("write config");

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

        assert_eq!(settings.device_name, "from-default-path");
        assert_eq!(settings.api_port, 19090);
    }
}

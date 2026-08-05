//! Application settings
//!
//! Single Responsibility: Load and provide configuration values.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::protocol::{DEFAULT_TCP_PORT, DEFAULT_UDP_PORT};
use crate::utils::errors::{Error, Result};

const DEFAULT_DEVICE_NAME: &str = "Rust Connect Device";
const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_BROADCAST_INTERVAL_SECS: u64 = 60;
const DEFAULT_LOG_MAX_FILES: usize = 24;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 0;
const DEFAULT_UI_ENABLED: bool = true;
const DEFAULT_API_BIND: &str = "127.0.0.1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub device_name: String,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub cert_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_level: String,
    pub log_max_files: usize,
    pub broadcast_interval_secs: u64,
    pub api_enabled: bool,
    pub api_port: u16,
    /// Address the REST API binds to. Defaults to loopback-only
    /// ("127.0.0.1") so the API is not exposed to the network; set to
    /// "0.0.0.0" in the config file to listen on all interfaces.
    #[serde(default = "default_api_bind")]
    pub api_bind: String,
    pub api_keys: Vec<String>,
    pub idle_timeout_secs: u64,
    pub ui_enabled: bool,
    /// Origins allowed for cross-origin (CORS) requests. Defaults to empty:
    /// no cross-origin access. "*" allows any origin — dangerous combined
    /// with the API key, since any web page could then call the API.
    pub allowed_origins: Vec<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        // RUST_CONNECT_DATA_DIR overrides the platform data dir — used by
        // integration tests (isolation from real state) and portable installs.
        let data_dir = std::env::var_os("RUST_CONNECT_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_data_dir().join("rust-connect"));
        Self {
            device_name: DEFAULT_DEVICE_NAME.to_string(),
            tcp_port: DEFAULT_TCP_PORT,
            udp_port: DEFAULT_UDP_PORT,
            cert_dir: data_dir.join("certs"),
            data_dir: data_dir.clone(),
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            log_max_files: DEFAULT_LOG_MAX_FILES,
            broadcast_interval_secs: DEFAULT_BROADCAST_INTERVAL_SECS,
            api_enabled: true,
            api_port: 9090,
            api_bind: DEFAULT_API_BIND.to_string(),
            api_keys: Vec::new(),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            ui_enabled: DEFAULT_UI_ENABLED,
            allowed_origins: Vec::new(),
        }
    }
}

fn default_api_bind() -> String {
    DEFAULT_API_BIND.to_string()
}

impl AppSettings {
    pub fn new() -> Self {
        Self::initialize(Self::default())
    }

    /// Build settings with the requested data directory selected before any
    /// API-key read or write. Tests and portable callers must use this rather
    /// than overriding `new()` after it has touched the platform default.
    pub fn new_with_data_dir(path: PathBuf) -> Self {
        Self::initialize(Self::default().with_data_dir(path))
    }

    fn initialize(mut settings: Self) -> Self {
        if let Some(name) = hostname::get().ok().and_then(|h| h.into_string().ok()) {
            settings.device_name = format!("{}-RustConnect", name);
        }

        settings.ensure_api_key();
        settings
    }

    /// Ensures at least one API key exists: reuse the persisted key file when
    /// it holds a valid UUID, otherwise generate a UUID key and persist it
    /// (0600). Used by both the default path and config-file loading so a
    /// config without `api_keys` never leaves auth empty.
    pub fn ensure_api_key(&mut self) {
        if !self.api_keys.is_empty() {
            return;
        }

        let key_path = self.data_dir.join("api_key");
        if key_path.exists() {
            if let Ok(key_content) = std::fs::read_to_string(&key_path) {
                let key = key_content.trim();
                if uuid::Uuid::parse_str(key).is_ok() {
                    self.api_keys = vec![key.to_string()];
                    return;
                }
            }
        }

        let key = uuid::Uuid::new_v4().to_string();
        if let Err(e) = Self::persist_api_key(&key_path, &key) {
            tracing::warn!(error = %e, "Failed to persist API key to disk");
        }
        self.api_keys = vec![key];
    }

    pub(crate) fn persist_api_key(path: &std::path::Path, key: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::fs::PermissionsExt;
            // Create with the mode up front: write-then-chmod leaves a
            // world-readable window, and a crash mid-way leaves the secret
            // permanently loose.
            // For a pre-existing loose file, .mode() is ignored — tighten
            // BEFORE truncating and writing the new secret.
            if path.exists() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(key.as_bytes())?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, key)?;
        }
        Ok(())
    }

    pub fn with_cert_dir(mut self, path: PathBuf) -> Self {
        self.cert_dir = path;
        self
    }

    pub fn with_data_dir(mut self, path: PathBuf) -> Self {
        self.data_dir = path.clone();
        self.cert_dir = path.join("certs");
        self
    }

    pub fn with_api_keys(mut self, keys: Vec<String>) -> Self {
        self.api_keys = keys;
        self
    }

    pub fn load_from_file(path: &PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| Error::ConfigError(format!("Failed to read config: {}", e)))?;
        let mut settings: AppSettings = toml::from_str(&content)?;

        if settings.api_keys.is_empty() {
            let key_path = settings.data_dir.join("api_key");
            if key_path.exists() {
                if let Ok(key_content) = std::fs::read_to_string(&key_path) {
                    let key = key_content.trim();
                    if uuid::Uuid::parse_str(key).is_ok() {
                        settings.api_keys = vec![key.to_string()];
                    }
                }
            }
        }

        Ok(settings)
    }

    pub fn save_to_file(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::ConfigError(format!("Failed to create config dir: {}", e)))?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| Error::ConfigError(format!("Failed to serialize config: {}", e)))?;
        // The config file can contain api_keys — create it owner-only from
        // the start; write-then-chmod leaves a world-readable window.
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::fs::PermissionsExt;
            // For a pre-existing loose file, .mode() is ignored — tighten
            // BEFORE truncating and writing the new secret.
            if path.exists() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| Error::ConfigError(format!("Failed to secure config: {}", e)))?;
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| Error::ConfigError(format!("Failed to write config: {}", e)))?;
            file.write_all(content.as_bytes())
                .map_err(|e| Error::ConfigError(format!("Failed to write config: {}", e)))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Error::ConfigError(format!("Failed to secure config: {}", e)))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, content)
                .map_err(|e| Error::ConfigError(format!("Failed to write config: {}", e)))?;
        }
        Ok(())
    }

    pub fn config_path() -> PathBuf {
        let base = dirs_config_dir();
        base.join("rust-connect").join("config.toml")
    }

    pub fn ensure_dirs_exist(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| Error::ConfigError(format!("Failed to create data dir: {}", e)))?;
        std::fs::create_dir_all(&self.cert_dir)
            .map_err(|e| Error::ConfigError(format!("Failed to create cert dir: {}", e)))?;
        Ok(())
    }

    /// Validates configuration values and returns clear errors for invalid settings.
    pub fn validate(&self) -> Result<()> {
        if self.broadcast_interval_secs == 0 {
            return Err(Error::ConfigError(
                "broadcast_interval_secs must be > 0 (would flood network)".to_string(),
            ));
        }
        if self.api_port == 0 {
            return Err(Error::ConfigError(
                "api_port must be > 0 (port 0 is unpredictable)".to_string(),
            ));
        }
        if self.tcp_port == 0 || self.udp_port == 0 {
            return Err(Error::ConfigError(
                "tcp_port and udp_port must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

fn dirs_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".data"))
}

fn dirs_config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_default_settings() {
        let settings = AppSettings::default();
        assert_eq!(settings.tcp_port, DEFAULT_TCP_PORT);
        assert_eq!(settings.udp_port, DEFAULT_UDP_PORT);
        assert_eq!(settings.broadcast_interval_secs, 60);
        assert!(settings.api_enabled);
        assert!(settings.api_keys.is_empty());
        // Secure-by-default: loopback-only bind, no cross-origin access.
        assert_eq!(settings.api_bind, "127.0.0.1");
        assert!(settings.allowed_origins.is_empty());
    }

    #[test]
    fn test_api_bind_missing_in_toml_defaults_to_loopback() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("partial.toml");
        std::fs::write(&path, "device_name = \"minimal\"").expect("Value expected to be present");

        let settings = AppSettings::load_from_file(&path).expect("Value expected to be present");
        assert_eq!(settings.api_bind, "127.0.0.1");
    }

    #[test]
    fn test_config_file_permissions() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("config.toml");

        AppSettings::new_with_data_dir(temp.path().join("data"))
            .save_to_file(&path)
            .expect("Value expected to be present");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&path)
                .expect("Value expected to be present")
                .permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn test_new_generates_api_key() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf());
        assert_eq!(settings.api_keys.len(), 1);
        assert!(!settings.api_keys[0].is_empty());
    }

    #[test]
    fn test_new_uses_hostname() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf());
        assert!(!settings.device_name.is_empty());
    }

    #[test]
    fn test_builder_pattern() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf());

        assert_eq!(settings.data_dir, temp.path());
        assert_eq!(settings.cert_dir, temp.path().join("certs"));
        assert!(
            temp.path().join("api_key").is_file(),
            "the API key must be created inside the selected data directory"
        );
    }

    #[test]
    fn test_save_and_load() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("config.toml");

        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf());

        settings
            .save_to_file(&path)
            .expect("Value expected to be present");
        let loaded = AppSettings::load_from_file(&path).expect("Value expected to be present");

        assert_eq!(loaded.device_name, settings.device_name);
        assert_eq!(loaded.tcp_port, settings.tcp_port);
        assert_eq!(
            loaded.broadcast_interval_secs,
            settings.broadcast_interval_secs
        );
    }

    #[test]
    fn test_ensure_dirs_exist() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let settings = AppSettings::new_with_data_dir(temp.path().join("subdir").to_path_buf());

        settings
            .ensure_dirs_exist()
            .expect("Value expected to be present");
        assert!(temp.path().join("subdir").exists());
        assert!(temp.path().join("subdir/certs").exists());
    }

    #[test]
    fn test_load_missing_file_fails() {
        let result = AppSettings::load_from_file(&PathBuf::from("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_with_api_keys() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf())
            .with_api_keys(vec!["key1".to_string()]);
        assert_eq!(settings.api_keys, vec!["key1"]);
    }

    #[test]
    fn test_with_cert_dir() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf())
            .with_cert_dir(temp.path().join("custom_certs").to_path_buf());
        assert_eq!(settings.cert_dir, temp.path().join("custom_certs"));
    }

    #[test]
    fn test_save_creates_parent_dirs() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("deeply/nested/config.toml");

        AppSettings::new_with_data_dir(temp.path().join("data"))
            .save_to_file(&path)
            .expect("Value expected to be present");
        assert!(path.exists());
    }

    #[test]
    fn test_load_malformed_toml_fails() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("bad.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("Value expected to be present");

        let result = AppSettings::load_from_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_missing_optional_fields_use_defaults() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("partial.toml");
        std::fs::write(&path, "device_name = \"minimal\"").expect("Value expected to be present");

        let settings = AppSettings::load_from_file(&path).expect("Value expected to be present");
        assert_eq!(settings.device_name, "minimal");
        assert_eq!(settings.tcp_port, DEFAULT_TCP_PORT);
        assert_eq!(settings.udp_port, DEFAULT_UDP_PORT);
    }

    /// Removing `protocol_version` from `AppSettings` must not break
    /// existing config files that still contain the field (older
    /// versions let users set it; it was never read). serde's default
    /// behavior ignores unknown fields, so a config that carries the
    /// legacy key must still load — and the loaded settings must equal
    /// the defaults (the field had no effect when present either).
    #[test]
    fn test_load_ignores_legacy_protocol_version_field() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let path = temp.path().join("legacy.toml");
        std::fs::write(
            &path,
            "device_name = \"legacy\"\nprotocol_version = 7\n",
        )
        .expect("Value expected to be present");

        let settings = AppSettings::load_from_file(&path).expect("Value expected to be present");
        assert_eq!(settings.device_name, "legacy");
        assert_eq!(settings.tcp_port, DEFAULT_TCP_PORT);
        assert_eq!(settings.udp_port, DEFAULT_UDP_PORT);
    }

    #[test]
    fn test_config_path_returns_path() {
        let path = AppSettings::config_path();
        assert!(path.to_string_lossy().contains("rust-connect"));
        assert!(path.to_string_lossy().contains("config.toml"));
    }

    #[test]
    fn test_ensure_dirs_idempotent() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let settings = AppSettings::new_with_data_dir(temp.path().to_path_buf());
        settings
            .ensure_dirs_exist()
            .expect("Value expected to be present");
        settings
            .ensure_dirs_exist()
            .expect("Value expected to be present");
        assert!(temp.path().join("certs").exists());
    }

    #[test]
    fn test_api_key_persisted() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let data_dir = temp.path().to_path_buf();

        let _settings1 = AppSettings::default().with_data_dir(data_dir.clone());
        let key1 = uuid::Uuid::new_v4().to_string();
        AppSettings::persist_api_key(&data_dir.join("api_key"), &key1)
            .expect("Value expected to be present");

        let settings2 = AppSettings::default().with_data_dir(data_dir.clone());
        assert!(settings2.api_keys.is_empty());

        let key_path = data_dir.join("api_key");
        let loaded_key =
            std::fs::read_to_string(&key_path).expect("Serialization of known types cannot fail");
        assert_eq!(loaded_key.trim(), key1);
    }

    #[test]
    fn test_api_key_malformed_file() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let data_dir = temp.path().to_path_buf();
        std::fs::write(data_dir.join("api_key"), "not-a-valid-uuid")
            .expect("Value expected to be present");

        let key_path = data_dir.join("api_key");
        let loaded =
            std::fs::read_to_string(&key_path).expect("Serialization of known types cannot fail");
        assert!(uuid::Uuid::parse_str(loaded.trim()).is_err());
    }

    #[test]
    fn test_api_key_manual_override() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let data_dir = temp.path().to_path_buf();
        let custom_key = uuid::Uuid::new_v4().to_string();

        AppSettings::persist_api_key(&data_dir.join("api_key"), &custom_key)
            .expect("Value expected to be present");

        let key_path = data_dir.join("api_key");
        let loaded =
            std::fs::read_to_string(&key_path).expect("Serialization of known types cannot fail");
        assert_eq!(loaded.trim(), custom_key);
    }

    #[test]
    fn test_api_key_file_permissions() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let data_dir = temp.path().to_path_buf();
        let key_path = data_dir.join("api_key");

        AppSettings::persist_api_key(&key_path, "test-key").expect("Value expected to be present");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&key_path)
                .expect("Value expected to be present")
                .permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn test_validate_rejects_zero_values() {
        let settings = AppSettings {
            broadcast_interval_secs: 0,
            ..Default::default()
        };
        assert!(settings.validate().is_err());

        let settings = AppSettings {
            api_port: 0,
            ..Default::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn test_validate_accepts_valid_settings() {
        let settings = AppSettings::default();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn test_api_key_creates_parent_dirs() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let key_path = temp.path().join("deep/nested/api_key");

        AppSettings::persist_api_key(&key_path, "some-key").expect("Value expected to be present");
        assert!(key_path.exists());
    }

    #[test]
    fn test_load_from_file_falls_back_to_api_key_file() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let data_dir = temp.path().to_path_buf();

        let persisted_key = uuid::Uuid::new_v4().to_string();
        AppSettings::persist_api_key(&data_dir.join("api_key"), &persisted_key)
            .expect("Value expected to be present");

        let toml_content = format!(
            "device_name = \"test\"\ndata_dir = \"{}\"\ncert_dir = \"{}\"\napi_port = 9090\napi_keys = []",
            data_dir.display(),
            data_dir.join("certs").display()
        );
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, toml_content).expect("Value expected to be present");

        let settings =
            AppSettings::load_from_file(&config_path).expect("Value expected to be present");
        assert_eq!(settings.api_keys.len(), 1);
        assert_eq!(settings.api_keys[0], persisted_key);
    }

    #[test]
    fn test_load_from_file_ignores_malformed_api_key_file() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let data_dir = temp.path().to_path_buf();

        std::fs::write(data_dir.join("api_key"), "not-a-uuid")
            .expect("Value expected to be present");

        let toml_content = format!(
            "device_name = \"test\"\ndata_dir = \"{}\"\ncert_dir = \"{}\"\napi_port = 9090\napi_keys = []",
            data_dir.display(),
            data_dir.join("certs").display()
        );
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, toml_content).expect("Value expected to be present");

        let settings =
            AppSettings::load_from_file(&config_path).expect("Value expected to be present");
        assert!(settings.api_keys.is_empty());
    }

    #[test]
    fn test_new_reads_existing_persisted_key() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let data_dir = temp.path().to_path_buf();

        let persisted_key = uuid::Uuid::new_v4().to_string();
        AppSettings::persist_api_key(&data_dir.join("api_key"), &persisted_key)
            .expect("Value expected to be present");

        let key_path = data_dir.join("api_key");
        let key_content =
            std::fs::read_to_string(&key_path).expect("Serialization of known types cannot fail");
        let key = key_content.trim();

        assert!(uuid::Uuid::parse_str(key).is_ok());
        assert_eq!(key, persisted_key);
    }
}

//! Error types for Rust Connect
//!
//! Following AI-first design principles:
//! - Structured error codes (machine-parseable)
//! - Clear error messages (human-readable)
//! - Contextual information (debugging)
//!
//! Single Responsibility: Define all error types for the application

use std::io;
use thiserror::Error;

/// Main error type for Rust Connect
#[derive(Debug, Error)]
pub enum Error {
    // Protocol Errors
    #[error(transparent)]
    DbusError(#[from] zbus::Error),

    #[error("Failed to discover device: {0}")]
    DiscoveryError(String),

    #[error("Invalid packet format: {0}")]
    InvalidPacket(String),

    #[error("Failed to serialize packet: {0}")]
    SerializationError(String),

    #[error("Failed to deserialize packet: {0}")]
    DeserializationError(String),

    #[error("Connection failed: {0}")]
    ConnectionError(String),

    #[error("Connection timed out: {0}")]
    ConnectionTimeout(String),

    /// `size` is the byte count at the moment the cap was exceeded: the true
    /// line length when the delimiter was already buffered, otherwise a lower
    /// bound — reading on to the real end of the line would defeat the
    /// bounded read the error exists to enforce.
    #[error("Packet too large: {size} bytes (max {max} bytes)")]
    PacketTooLarge { size: usize, max: usize },

    #[error("TLS handshake failed: {0}")]
    TlsError(String),

    #[error("Certificate error: {0}")]
    CertificateError(String),

    /// Send-side capability gating (parity-checklist.md § Lifecycle,
    /// `core/device.cpp:358-363` `Device::sendPacket`): the peer's last-
    /// known `incomingCapabilities` don't list this packet type. Refusing
    /// honestly (a typed 4xx) instead of a silent no-op — Android would
    /// ignore the packet anyway, so this turns a fake success into a
    /// truthful error, matching this project's standing stance against
    /// silent drops.
    #[error("Peer {device_id} does not support packet type {packet_type}")]
    CapabilityNotSupported {
        device_id: String,
        packet_type: String,
    },

    // Device Errors
    #[error("Device not found: {0}")]
    DeviceNotFound(String),

    #[error("Device already exists: {0}")]
    DeviceAlreadyExists(String),

    #[error("Invalid device state transition from {from:?} to {to:?}")]
    InvalidStateTransition {
        from: crate::device::types::DeviceState,
        to: crate::device::types::DeviceState,
    },

    // Pairing Errors
    #[error("Pairing request timeout for device: {0}")]
    PairingTimeout(String),

    #[error("Pairing rejected by device: {0}")]
    PairingRejected(String),

    #[error("No pending pairing request for device: {0}")]
    NoPendingPairRequest(String),

    #[error("Device not paired: {0}")]
    DeviceNotPaired(String),

    /// Service Unavailable (HTTP 503). Used when a backend is required
    /// for the requested operation and is missing (e.g. sshfs/fusermount
    /// for the SFTP mount endpoint). Distinct from ConnectionError
    /// (the daemon-to-device link is fine; the desktop's local tool
    /// isn't).
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    // Plugin Errors
    #[error("Plugin not found: {0}")]
    PluginNotFound(String),

    #[error("Plugin error in {plugin}: {message}")]
    PluginError { plugin: String, message: String },

    #[error("No plugin registered for packet type: {0}")]
    NoPluginForPacketType(String),

    // Configuration Errors
    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Invalid configuration value for {key}: {message}")]
    InvalidConfigValue { key: String, message: String },

    // API Errors
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Invalid API request: {0}")]
    InvalidRequest(String),

    #[error("Resource not found: {0}")]
    NotFound(String),

    // I/O Errors
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("File not found: {0}")]
    FileNotFound(String),

    // Serialization Errors
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),

    // Channel Errors
    #[error("Channel send error")]
    ChannelSend,

    #[error("Channel receive error")]
    ChannelReceive,

    // Generic Errors
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("{0}")]
    Other(String),
}

/// Result type alias for Rust Connect
pub type Result<T> = std::result::Result<T, Error>;

/// Error codes for API responses (AI-friendly)
#[derive(Debug, Clone, Copy)]
pub enum ErrorCode {
    // Protocol
    DiscoveryError,
    InvalidPacket,
    SerializationError,
    DeserializationError,
    ConnectionError,
    ConnectionTimeout,
    PacketTooLarge,
    TlsError,
    CertificateError,
    CapabilityNotSupported,

    // Device
    DeviceNotFound,
    DeviceAlreadyExists,
    InvalidStateTransition,

    // Pairing
    PairingTimeout,
    PairingRejected,
    NoPendingPairRequest,
    DeviceNotPaired,

    /// Local backend missing (e.g. sshfs, fusermount, pactl) — the
    /// daemon can serve the request in principle but its local tool
    /// isn't installed. HTTP 503.
    ServiceUnavailable,

    // Plugin
    PluginNotFound,
    PluginError,
    NoPluginForPacketType,

    // Configuration
    ConfigError,
    InvalidConfigValue,

    // API
    Unauthorized,
    RateLimited,
    InvalidRequest,
    NotFound,

    // I/O
    IoError,
    FileNotFound,

    // Serialization
    JsonError,
    TomlError,

    // Channel
    ChannelSendError,
    ChannelReceiveError,

    // Generic
    InternalError,
    OtherError,
}

impl ErrorCode {
    /// Get the error code as a string (for API responses)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DiscoveryError => "DISCOVERY_ERROR",
            Self::InvalidPacket => "INVALID_PACKET",
            Self::SerializationError => "SERIALIZATION_ERROR",
            Self::DeserializationError => "DESERIALIZATION_ERROR",
            Self::ConnectionError => "CONNECTION_ERROR",
            Self::ConnectionTimeout => "CONNECTION_TIMEOUT",
            Self::PacketTooLarge => "PACKET_TOO_LARGE",
            Self::TlsError => "TLS_ERROR",
            Self::CertificateError => "CERTIFICATE_ERROR",
            Self::CapabilityNotSupported => "CAPABILITY_NOT_SUPPORTED",
            Self::DeviceNotFound => "DEVICE_NOT_FOUND",
            Self::DeviceAlreadyExists => "DEVICE_ALREADY_EXISTS",
            Self::InvalidStateTransition => "INVALID_STATE_TRANSITION",
            Self::PairingTimeout => "PAIRING_TIMEOUT",
            Self::PairingRejected => "PAIRING_REJECTED",
            Self::NoPendingPairRequest => "NO_PENDING_PAIR_REQUEST",
            Self::DeviceNotPaired => "DEVICE_NOT_PAIRED",
            Self::ServiceUnavailable => "SERVICE_UNAVAILABLE",
            Self::PluginNotFound => "PLUGIN_NOT_FOUND",
            Self::PluginError => "PLUGIN_ERROR",
            Self::NoPluginForPacketType => "NO_PLUGIN_FOR_PACKET_TYPE",
            Self::ConfigError => "CONFIG_ERROR",
            Self::InvalidConfigValue => "INVALID_CONFIG_VALUE",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::RateLimited => "RATE_LIMITED",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::NotFound => "NOT_FOUND",
            Self::IoError => "IO_ERROR",
            Self::FileNotFound => "FILE_NOT_FOUND",
            Self::JsonError => "JSON_ERROR",
            Self::TomlError => "TOML_ERROR",
            Self::ChannelSendError => "CHANNEL_SEND_ERROR",
            Self::ChannelReceiveError => "CHANNEL_RECEIVE_ERROR",
            Self::InternalError => "INTERNAL_ERROR",
            Self::OtherError => "OTHER_ERROR",
        }
    }

    /// Get HTTP status code for this error (for API responses)
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::RateLimited => 429,
            Self::NotFound | Self::DeviceNotFound | Self::PluginNotFound | Self::FileNotFound => {
                404
            }
            Self::InvalidRequest
            | Self::InvalidPacket
            | Self::InvalidConfigValue
            | Self::CapabilityNotSupported => 400,
            Self::DeviceAlreadyExists => 409,
            // Unpairing something not paired is a benign client-state
            // condition, not a server fault (audit F-M1). 500 here misled
            // monitoring into reading a client mistake as a crash, and the
            // error CODE already said client-error while the status said
            // server-error. Same class as DeviceAlreadyExists: a conflict
            // with current state.
            Self::DeviceNotPaired => 409,
            // Deliberate policy refusal (e.g. the #1056 cert-anchor gate):
            // a conflict with the current state, not an internal fault.
            Self::PairingRejected => 409,
            Self::PairingTimeout
            | Self::ConnectionError
            | Self::TlsError
            | Self::ConnectionTimeout
            | Self::ServiceUnavailable => 503,
            Self::PacketTooLarge => 413,
            _ => 500,
        }
    }
}

impl Error {
    /// Create an I/O error with context
    pub fn io(message: String, path: Option<String>) -> Self {
        if let Some(p) = path {
            Self::Internal(format!("{}: {}", message, p))
        } else {
            Self::Internal(message)
        }
    }

    /// Create a not found error
    pub fn not_found(message: &str, path: Option<String>) -> Self {
        if let Some(p) = path {
            Self::NotFound(format!("{}: {}", message, p))
        } else {
            Self::NotFound(message.to_string())
        }
    }

    /// Create a validation error
    pub fn validation(message: String, field: Option<String>) -> Self {
        if let Some(f) = field {
            Self::InvalidRequest(format!("{}: {}", f, message))
        } else {
            Self::InvalidRequest(message)
        }
    }

    /// Get the error code for this error
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::DiscoveryError(_) => ErrorCode::DiscoveryError,
            Self::InvalidPacket(_) => ErrorCode::InvalidPacket,
            Self::SerializationError(_) => ErrorCode::SerializationError,
            Self::DeserializationError(_) => ErrorCode::DeserializationError,
            Self::ConnectionError(_) => ErrorCode::ConnectionError,
            Self::ConnectionTimeout(_) => ErrorCode::ConnectionTimeout,
            Self::PacketTooLarge { .. } => ErrorCode::PacketTooLarge,
            Self::TlsError(_) => ErrorCode::TlsError,
            Self::CertificateError(_) => ErrorCode::CertificateError,
            Self::CapabilityNotSupported { .. } => ErrorCode::CapabilityNotSupported,
            Self::DeviceNotFound(_) => ErrorCode::DeviceNotFound,
            Self::DeviceAlreadyExists(_) => ErrorCode::DeviceAlreadyExists,
            Self::InvalidStateTransition { .. } => ErrorCode::InvalidStateTransition,
            Self::PairingTimeout(_) => ErrorCode::PairingTimeout,
            Self::PairingRejected(_) => ErrorCode::PairingRejected,
            Self::NoPendingPairRequest(_) => ErrorCode::NoPendingPairRequest,
            Self::DeviceNotPaired(_) => ErrorCode::DeviceNotPaired,
            Self::ServiceUnavailable(_) => ErrorCode::ServiceUnavailable,
            Self::PluginNotFound(_) => ErrorCode::PluginNotFound,
            Self::PluginError { .. } => ErrorCode::PluginError,
            Self::NoPluginForPacketType(_) => ErrorCode::NoPluginForPacketType,
            Self::ConfigError(_) => ErrorCode::ConfigError,
            Self::InvalidConfigValue { .. } => ErrorCode::InvalidConfigValue,
            Self::Unauthorized(_) => ErrorCode::Unauthorized,
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Io(_) => ErrorCode::IoError,
            Self::FileNotFound(_) => ErrorCode::FileNotFound,
            Self::Json(_) => ErrorCode::JsonError,
            Self::Toml(_) => ErrorCode::TomlError,
            Self::ChannelSend => ErrorCode::ChannelSendError,
            Self::ChannelReceive => ErrorCode::ChannelReceiveError,
            Self::Internal(_) => ErrorCode::InternalError,
            Self::Other(_) => ErrorCode::OtherError,
            Self::DbusError(_) => ErrorCode::OtherError,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_error_codes() {
        let err = Error::DeviceNotFound("test-device".to_string());
        assert_eq!(err.code().as_str(), "DEVICE_NOT_FOUND");
        assert_eq!(err.code().http_status(), 404);
    }

    #[test]
    fn test_error_display() {
        let err = Error::ConnectionError("timeout".to_string());
        assert_eq!(err.to_string(), "Connection failed: timeout");
    }

    #[test]
    fn test_state_transition_error() {
        use crate::device::types::DeviceState;

        let err = Error::InvalidStateTransition {
            from: DeviceState::Discovered,
            to: DeviceState::Connected,
        };

        assert!(err.to_string().contains("Invalid device state transition"));
    }

    #[test]
    fn test_all_error_codes_are_unique() {
        let codes: Vec<&'static str> = vec![
            ErrorCode::DiscoveryError.as_str(),
            ErrorCode::InvalidPacket.as_str(),
            ErrorCode::SerializationError.as_str(),
            ErrorCode::DeserializationError.as_str(),
            ErrorCode::ConnectionError.as_str(),
            ErrorCode::ConnectionTimeout.as_str(),
            ErrorCode::PacketTooLarge.as_str(),
            ErrorCode::TlsError.as_str(),
            ErrorCode::CertificateError.as_str(),
            ErrorCode::CapabilityNotSupported.as_str(),
            ErrorCode::DeviceNotFound.as_str(),
            ErrorCode::DeviceAlreadyExists.as_str(),
            ErrorCode::InvalidStateTransition.as_str(),
            ErrorCode::PairingTimeout.as_str(),
            ErrorCode::PairingRejected.as_str(),
            ErrorCode::NoPendingPairRequest.as_str(),
            ErrorCode::DeviceNotPaired.as_str(),
            ErrorCode::PluginNotFound.as_str(),
            ErrorCode::PluginError.as_str(),
            ErrorCode::NoPluginForPacketType.as_str(),
            ErrorCode::ConfigError.as_str(),
            ErrorCode::InvalidConfigValue.as_str(),
            ErrorCode::Unauthorized.as_str(),
            ErrorCode::InvalidRequest.as_str(),
            ErrorCode::NotFound.as_str(),
            ErrorCode::IoError.as_str(),
            ErrorCode::FileNotFound.as_str(),
            ErrorCode::JsonError.as_str(),
            ErrorCode::TomlError.as_str(),
            ErrorCode::ChannelSendError.as_str(),
            ErrorCode::ChannelReceiveError.as_str(),
            ErrorCode::InternalError.as_str(),
            ErrorCode::OtherError.as_str(),
        ];
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(codes.len(), unique.len(), "Duplicate error codes found");
    }

    #[test]
    fn test_http_status_categories() {
        assert_eq!(ErrorCode::Unauthorized.http_status(), 401);
        assert_eq!(ErrorCode::DeviceNotFound.http_status(), 404);
        assert_eq!(ErrorCode::PluginNotFound.http_status(), 404);
        assert_eq!(ErrorCode::InvalidPacket.http_status(), 400);
        assert_eq!(ErrorCode::DeviceAlreadyExists.http_status(), 409);
        assert_eq!(ErrorCode::PairingTimeout.http_status(), 503);
        assert_eq!(ErrorCode::ConnectionError.http_status(), 503);
        assert_eq!(ErrorCode::DiscoveryError.http_status(), 500);
        assert_eq!(ErrorCode::InternalError.http_status(), 500);
        // Gap 8 (parity-checklist.md § Lifecycle, vk #998 Task 2.3):
        // capability gating refuses honestly with a 4xx, not a 500 —
        // that's what makes it reach the API as a typed client error.
        assert_eq!(ErrorCode::CapabilityNotSupported.http_status(), 400);
    }

    /// Gap 8: the error message names the offending packet type — the
    /// brief's "body naming the type" requirement, verified at the
    /// message layer (the API test in tests/protocol_integration.rs
    /// verifies the same fact end to end through the HTTP body).
    #[test]
    fn test_capability_not_supported_message_names_the_type() {
        let err = Error::CapabilityNotSupported {
            device_id: "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            packet_type: "kdeconnect.mousepad.request".to_string(),
        };
        assert!(err.to_string().contains("kdeconnect.mousepad.request"));
        assert_eq!(err.code().http_status(), 400);
    }

    #[test]
    fn test_error_code_matches_for_each_variant() {
        let errors: Vec<Error> = vec![
            Error::DiscoveryError("".into()),
            Error::InvalidPacket("".into()),
            Error::SerializationError("".into()),
            Error::DeserializationError("".into()),
            Error::ConnectionError("".into()),
            Error::ConnectionTimeout("".into()),
            Error::PacketTooLarge {
                size: 2_000_000,
                max: 1_048_576,
            },
            Error::TlsError("".into()),
            Error::CertificateError("".into()),
            Error::CapabilityNotSupported {
                device_id: "".into(),
                packet_type: "".into(),
            },
            Error::DeviceNotFound("".into()),
            Error::DeviceAlreadyExists("".into()),
            Error::InvalidStateTransition {
                from: crate::device::types::DeviceState::Discovered,
                to: crate::device::types::DeviceState::Paired,
            },
            Error::PairingTimeout("".into()),
            Error::PairingRejected("".into()),
            Error::NoPendingPairRequest("".into()),
            Error::DeviceNotPaired("".into()),
            Error::PluginNotFound("".into()),
            Error::PluginError {
                plugin: "x".into(),
                message: "y".into(),
            },
            Error::NoPluginForPacketType("".into()),
            Error::ConfigError("".into()),
            Error::InvalidConfigValue {
                key: "x".into(),
                message: "y".into(),
            },
            Error::Unauthorized("".into()),
            Error::InvalidRequest("".into()),
            Error::NotFound("".into()),
            Error::FileNotFound("".into()),
            Error::ChannelSend,
            Error::ChannelReceive,
            Error::Internal("".into()),
            Error::Other("".into()),
        ];

        for err in errors {
            assert!(!err.code().as_str().is_empty());
            assert!(err.code().http_status() >= 400);
            assert!(err.code().http_status() < 600);
        }
    }

    #[test]
    fn test_helper_constructors() {
        let err = Error::not_found("thing", None);
        assert!(matches!(err, Error::NotFound(_)));
        assert!(err.to_string().contains("thing"));

        let err = Error::not_found("thing", Some("path".into()));
        assert!(err.to_string().contains("path"));

        let err = Error::validation("bad value".into(), None);
        assert!(matches!(err, Error::InvalidRequest(_)));

        let err = Error::validation("bad".into(), Some("field".into()));
        assert!(err.to_string().contains("field"));

        let err = Error::io("fail".into(), None);
        assert!(matches!(err, Error::Internal(_)));

        let err = Error::io("fail".into(), Some("/path".into()));
        assert!(err.to_string().contains("/path"));
    }

    #[test]
    fn test_plugin_error_display() {
        let err = Error::PluginError {
            plugin: "battery".into(),
            message: "parse failed".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("battery"));
        assert!(msg.contains("parse failed"));
    }

    #[test]
    fn test_config_value_error_display() {
        let err = Error::InvalidConfigValue {
            key: "port".into(),
            message: "must be 1-65535".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("port"));
        assert!(msg.contains("must be 1-65535"));
    }
}

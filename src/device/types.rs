//! Device types for Rust Connect
//!
//! Following AI-first design principles:
//! - Explicit state representation
//! - Structured data (no ambiguity)
//! - Serializable for API responses
//!
//! Single Responsibility: Define device-related types

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use utoipa::ToSchema;

/// Unique identifier for a device
pub type DeviceId = String;

/// Device state in the connection lifecycle
///
/// State transitions follow a strict flow:
/// Discovered -> Pairing -> Paired -> Connected -> Disconnected
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeviceState {
    /// Device discovered on network but not yet paired
    Discovered,
    /// Pairing in progress
    Pairing,
    /// Device paired but not connected
    Paired,
    /// Device connected and ready for communication
    Connected,
    /// Device disconnected
    Disconnected,
}

impl DeviceState {
    /// Check if transition to new state is valid
    pub fn can_transition_to(&self, new_state: &DeviceState) -> bool {
        use DeviceState::*;

        matches!(
            (self, new_state),
            // Forward transitions
            (Discovered, Pairing)
                | (Discovered, Connected)
                | (Pairing, Paired)
                | (Pairing, Connected)
                | (Paired, Connected)
                | (Connected, Disconnected)
                | (Connected, Connected)
                // Backward transitions
                | (Pairing, Discovered)
                | (Connected, Paired)
                | (Disconnected, Discovered)
                | (Disconnected, Paired)
                | (Disconnected, Connected)
                // Re-connection
                | (Paired, Pairing)
        )
    }

    /// Get human-readable description of state
    pub fn description(&self) -> &'static str {
        match self {
            Self::Discovered => "Device discovered on network",
            Self::Pairing => "Pairing in progress",
            Self::Paired => "Device paired but not connected",
            Self::Connected => "Device connected and ready",
            Self::Disconnected => "Device disconnected",
        }
    }
}

/// Device type (desktop, phone, tablet, etc.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Desktop,
    Laptop,
    Phone,
    Tablet,
    Tv,
    Unknown,
}

impl DeviceType {
    /// Parse device type from string
    pub fn parse_device_type(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "desktop" => Self::Desktop,
            "laptop" => Self::Laptop,
            "phone" | "smartphone" => Self::Phone,
            "tablet" => Self::Tablet,
            "tv" => Self::Tv,
            _ => Self::Unknown,
        }
    }

    /// Convert device type to string
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Laptop => "laptop",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Tv => "tv",
            Self::Unknown => "unknown",
        }
    }
}

/// Complete device information
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Device {
    /// Unique device identifier
    pub id: DeviceId,

    /// Human-readable device name
    pub name: String,

    /// Device type
    pub device_type: DeviceType,

    /// Current state
    pub state: DeviceState,

    /// When the device entered current state
    pub state_since: DateTime<Utc>,

    /// Network address (if connected)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<SocketAddr>,

    /// Protocol version
    pub protocol_version: u32,

    /// Incoming capabilities (what this device can receive)
    pub incoming_capabilities: Vec<String>,

    /// Outgoing capabilities (what this device can send)
    pub outgoing_capabilities: Vec<String>,

    /// TLS certificate fingerprint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate_fingerprint: Option<String>,

    /// When device was first discovered
    pub discovered_at: DateTime<Utc>,

    /// When device was last seen
    pub last_seen: DateTime<Utc>,

    /// When device was paired (if paired)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_at: Option<DateTime<Utc>>,

    /// Verification key for pairing confirmation (8 hex chars, present during pending pairing)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_key: Option<String>,
}

impl Device {
    /// Create a new device from discovery information
    pub fn new(id: DeviceId, name: String, device_type: DeviceType, protocol_version: u32) -> Self {
        let now = Utc::now();

        Self {
            id,
            name,
            device_type,
            state: DeviceState::Discovered,
            state_since: now,
            address: None,
            protocol_version,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            certificate_fingerprint: None,
            discovered_at: now,
            last_seen: now,
            paired_at: None,
            verification_key: None,
        }
    }

    /// Carry the identity packet's advertised capabilities onto the device.
    ///
    /// Both lists arrive on every identity exchange (`protocol::types::Identity`)
    /// and are what `has_incoming_capability` / `has_outgoing_capability` read.
    /// Constructing a device without them leaves it advertising nothing, so those
    /// predicates always answer false and the API reports empty lists for
    /// connected, paired phones.
    pub fn with_capabilities(mut self, incoming: Vec<String>, outgoing: Vec<String>) -> Self {
        self.incoming_capabilities = incoming;
        self.outgoing_capabilities = outgoing;
        self
    }

    /// Take `paired_at` from the pairing store, which owns it.
    ///
    /// `PairingHandler`'s `paired` map (persisted as `paired.json`) is
    /// authoritative for when a device is paired. This field used to be a second
    /// copy, stamped once on the first lifecycle transition into `Paired` and
    /// never revisited — and a paired phone reconnecting goes
    /// `Discovered -> Connected` without passing through `Paired`, so the copy
    /// had no way to self-correct. Live 2026-07-30: the record reported
    /// 2026-04-07 while `paired.json` recorded the same day.
    ///
    /// `None` means the pairing store does not know this device, so it is not
    /// paired regardless of what the record remembers.
    pub fn reconcile_paired_at(&mut self, authoritative: Option<DateTime<Utc>>) {
        self.paired_at = authoritative;
    }

    pub fn set_verification_key(&mut self, key: String) {
        self.verification_key = Some(key);
    }

    pub fn clear_verification_key(&mut self) {
        self.verification_key = None;
    }

    /// Check if device is paired
    pub fn is_paired(&self) -> bool {
        matches!(
            self.state,
            DeviceState::Paired | DeviceState::Connected | DeviceState::Disconnected
        )
    }

    /// Check if device is connected
    pub fn is_connected(&self) -> bool {
        self.state == DeviceState::Connected
    }

    /// Update last seen timestamp
    pub fn update_last_seen(&mut self) {
        self.last_seen = Utc::now();
    }

    /// Check if device has capability
    pub fn has_incoming_capability(&self, capability: &str) -> bool {
        self.incoming_capabilities.iter().any(|c| c == capability)
    }

    /// Check if device can send capability
    pub fn has_outgoing_capability(&self, capability: &str) -> bool {
        self.outgoing_capabilities.iter().any(|c| c == capability)
    }
}

/// Device event types
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum DeviceEvent {
    /// Device discovered on network
    Discovered {
        device_id: DeviceId,
        device_name: String,
        device_type: DeviceType,
    },

    /// Device state changed
    StateChanged {
        device_id: DeviceId,
        old_state: DeviceState,
        new_state: DeviceState,
    },

    /// Device paired
    Paired {
        device_id: DeviceId,
        device_name: String,
    },

    /// Device connected
    Connected {
        device_id: DeviceId,
        address: SocketAddr,
    },

    /// Device disconnected
    Disconnected { device_id: DeviceId, reason: String },

    /// Device removed
    Removed { device_id: DeviceId },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_device_state_transitions() {
        use DeviceState::*;

        // Valid forward transitions
        assert!(Discovered.can_transition_to(&Pairing));
        assert!(Discovered.can_transition_to(&Connected));
        assert!(Pairing.can_transition_to(&Paired));
        assert!(Pairing.can_transition_to(&Connected));
        assert!(Paired.can_transition_to(&Connected));
        assert!(Connected.can_transition_to(&Disconnected));

        // Valid backward transitions
        assert!(Pairing.can_transition_to(&Discovered));
        assert!(Connected.can_transition_to(&Paired));
        assert!(Disconnected.can_transition_to(&Discovered));
        assert!(Disconnected.can_transition_to(&Paired));
        assert!(Disconnected.can_transition_to(&Connected));

        // Invalid transitions
        assert!(!Connected.can_transition_to(&Discovered));
    }

    #[test]
    fn test_device_type_parsing() {
        assert_eq!(DeviceType::parse_device_type("phone"), DeviceType::Phone);
        assert_eq!(DeviceType::parse_device_type("PHONE"), DeviceType::Phone);
        assert_eq!(
            DeviceType::parse_device_type("desktop"),
            DeviceType::Desktop
        );
        assert_eq!(
            DeviceType::parse_device_type("invalid"),
            DeviceType::Unknown
        );
    }

    #[test]
    fn test_device_type_smartphone_alias() {
        assert_eq!(
            DeviceType::parse_device_type("smartphone"),
            DeviceType::Phone
        );
        assert_eq!(
            DeviceType::parse_device_type("SMARTPHONE"),
            DeviceType::Phone
        );
    }

    #[test]
    fn test_device_creation() {
        let device = Device::new(
            "test-id".to_string(),
            "Test Device".to_string(),
            DeviceType::Phone,
            7,
        );

        assert_eq!(device.id, "test-id");
        assert_eq!(device.name, "Test Device");
        assert_eq!(device.device_type, DeviceType::Phone);
        assert_eq!(device.state, DeviceState::Discovered);
        assert_eq!(device.protocol_version, 7);
        assert!(!device.is_paired());
        assert!(!device.is_connected());
    }

    #[test]
    fn test_device_capabilities() {
        let mut device = Device::new(
            "test-id".to_string(),
            "Test Device".to_string(),
            DeviceType::Phone,
            7,
        );

        device
            .incoming_capabilities
            .push("kdeconnect.sms.messages".to_string());
        device
            .outgoing_capabilities
            .push("kdeconnect.sms.request".to_string());

        assert!(device.has_incoming_capability("kdeconnect.sms.messages"));
        assert!(!device.has_incoming_capability("kdeconnect.ping"));
        assert!(device.has_outgoing_capability("kdeconnect.sms.request"));
        assert!(!device.has_outgoing_capability("kdeconnect.ping"));
    }

    #[test]
    fn test_device_state_descriptions() {
        assert_eq!(
            DeviceState::Discovered.description(),
            "Device discovered on network"
        );
        assert_eq!(DeviceState::Pairing.description(), "Pairing in progress");
        assert_eq!(
            DeviceState::Paired.description(),
            "Device paired but not connected"
        );
        assert_eq!(
            DeviceState::Connected.description(),
            "Device connected and ready"
        );
        assert_eq!(
            DeviceState::Disconnected.description(),
            "Device disconnected"
        );
    }

    #[test]
    fn test_device_type_as_str() {
        assert_eq!(DeviceType::Desktop.as_str(), "desktop");
        assert_eq!(DeviceType::Laptop.as_str(), "laptop");
        assert_eq!(DeviceType::Phone.as_str(), "phone");
        assert_eq!(DeviceType::Tablet.as_str(), "tablet");
        assert_eq!(DeviceType::Tv.as_str(), "tv");
        assert_eq!(DeviceType::Unknown.as_str(), "unknown");
    }

    #[test]
    fn test_update_last_seen() {
        let mut device = Device::new(
            "test-id".to_string(),
            "Test".to_string(),
            DeviceType::Phone,
            7,
        );
        let original = device.last_seen;
        std::thread::sleep(std::time::Duration::from_millis(10));
        device.update_last_seen();
        assert!(device.last_seen > original);
    }
}

#[cfg(test)]
mod identity_capability_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The identity packet carries both capability lists (protocol/types.rs
    /// Identity), and they are what `has_incoming_capability` /
    /// `has_outgoing_capability` are for. Discarding them at construction left
    /// every device advertising nothing, so those predicates always answered
    /// false and the API surfaced empty lists for connected, paired phones.
    #[test]
    fn test_with_capabilities_populates_both_lists() {
        let device = Device::new(
            "aaaabbbbccccddddeeeeffff00003333".to_string(),
            "test phone".to_string(),
            DeviceType::Phone,
            8,
        )
        .with_capabilities(
            vec!["kdeconnect.notification".to_string()],
            vec!["kdeconnect.battery".to_string()],
        );

        assert!(device.has_incoming_capability("kdeconnect.notification"));
        assert!(device.has_outgoing_capability("kdeconnect.battery"));
        assert!(!device.has_incoming_capability("kdeconnect.mpris"));
    }
}

#[cfg(test)]
mod paired_at_reconciliation_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// `paired.json` (the PairingHandler's store) is authoritative for when a
    /// device is paired. The device record kept its own copy, set once on the
    /// first lifecycle transition into Paired and never again — and a paired
    /// phone reconnecting goes Discovered -> Connected without passing through
    /// Paired at all, so the copy could not self-correct. Live 2026-07-30: the
    /// record said 2026-04-07 while paired.json said the same day.
    ///
    /// The record must therefore take its value from the pairing store rather
    /// than maintain a second one that drifts.
    #[test]
    fn test_paired_at_adopts_the_authoritative_value() {
        let mut device = Device::new(
            "aaaabbbbccccddddeeeeffff00008888".to_string(),
            "phone".to_string(),
            DeviceType::Phone,
            8,
        );
        let april = Utc::now() - chrono::Duration::days(114);
        device.paired_at = Some(april);

        let authoritative = Utc::now();
        device.reconcile_paired_at(Some(authoritative));
        assert_eq!(device.paired_at, Some(authoritative));
    }

    /// A device the pairing store does not know is not paired, whatever the
    /// record remembers. Leaving a stale timestamp would report an unpaired
    /// device as paired since April.
    #[test]
    fn test_unknown_to_the_pairing_store_clears_paired_at() {
        let mut device = Device::new(
            "aaaabbbbccccddddeeeeffff00009999".to_string(),
            "phone".to_string(),
            DeviceType::Phone,
            8,
        );
        device.paired_at = Some(Utc::now() - chrono::Duration::days(114));

        device.reconcile_paired_at(None);
        assert_eq!(device.paired_at, None);
    }
}

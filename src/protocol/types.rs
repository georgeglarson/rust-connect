//! Protocol types for KDE Connect
//!
//! Single Responsibility: Define protocol-level types

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::device::types::{DeviceId, DeviceType};

/// KDE Connect protocol version
pub const PROTOCOL_VERSION: u32 = 8;

/// Default TCP port for KDE Connect
pub const DEFAULT_TCP_PORT: u16 = 1716;

/// Default UDP port for discovery
pub const DEFAULT_UDP_PORT: u16 = 1716;

/// Minimum port in the KDE Connect port range
pub const MIN_PORT: u16 = 1716;

/// Maximum port in the KDE Connect port range
pub const MAX_PORT: u16 = 1764;

/// Device identity information
///
/// This is broadcast during discovery and exchanged during pairing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub device_id: DeviceId,
    pub device_name: String,
    pub device_type: String,
    pub protocol_version: u32,
    #[serde(default)]
    pub incoming_capabilities: Vec<String>,
    #[serde(default)]
    pub outgoing_capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
    /// Android writes this as a string (LanLinkProvider.java:249 passes
    /// `getString("protocolVersion")`) but reads it back as an int
    /// (`getIntOrNull`, LanLinkProvider.java:170). Type it numeric per the
    /// wire meaning; accept both forms on deserialize, emit a number.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "deserialize_lenient_u32_opt"
    )]
    pub target_protocol_version: Option<u32>,
}

/// Lenient `Option<u32>` for `targetProtocolVersion`: a JSON number or a
/// numeric string, matching Android's `getIntOrNull` coercion.
fn deserialize_lenient_u32_opt<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| Error::custom("targetProtocolVersion must be a non-negative integer")),
        Some(serde_json::Value::String(s)) => s
            .parse::<u32>()
            .map(Some)
            .map_err(|_| Error::custom("targetProtocolVersion must be numeric")),
        Some(_) => Err(Error::custom(
            "targetProtocolVersion must be a number or numeric string",
        )),
    }
}

/// Lenient `i64` for the packet `id`: a JSON number or a numeric string.
/// Both references NEVER read the id (kde networkpacket.cpp:46 writes it,
/// nothing reads it; Android NetworkPacket.kt unserialize ignores it), so
/// its wire type is unconstrained — a string id must not kill the link
/// (parity-checklist gap 5).
fn deserialize_lenient_i64<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| Error::custom("id must be an integer")),
        serde_json::Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| Error::custom("id must be numeric")),
        _ => Err(Error::custom("id must be a number or numeric string")),
    }
}

/// Android's device-name cap (DeviceHelper.kt:41, MAX_DEVICE_NAME_LENGTH).
/// Note: Android never REJECTS a name on length — it sanitizes received
/// names (see `sanitize_device_name`); the protocol doc's "max 32 chars" is
/// what Android emits for itself, not a receive-side rule.
pub const MAX_DEVICE_NAME_LENGTH: usize = 32;

/// Android `DeviceHelper.filterInvalidCharactersFromDeviceNameAndLimitLength`
/// (DeviceHelper.kt:40-41,159): strip `["',;:.!?()\[\]<>]`, trim, take the
/// first 32 chars. Applied to received names exactly as Android's
/// `DeviceInfo.fromIdentityPacketAndCert` does.
fn sanitize_device_name(name: &str) -> String {
    const INVALID_CHARS: &[char] = &[
        '"', '\'', ',', ';', ':', '.', '!', '?', '(', ')', '[', ']', '<', '>',
    ];
    name.chars()
        .filter(|c| !INVALID_CHARS.contains(c))
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_DEVICE_NAME_LENGTH)
        .collect()
}

impl Identity {
    pub fn new(
        device_id: DeviceId,
        device_name: String,
        device_type: DeviceType,
        incoming_capabilities: Vec<String>,
        outgoing_capabilities: Vec<String>,
    ) -> Self {
        Self {
            device_id,
            device_name,
            device_type: device_type.as_str().to_string(),
            protocol_version: PROTOCOL_VERSION,
            tcp_port: Some(DEFAULT_TCP_PORT),
            incoming_capabilities,
            outgoing_capabilities,
            target_device_id: None,
            target_protocol_version: None,
        }
    }

    pub fn generate_device_id() -> DeviceId {
        Uuid::new_v4().to_string()
    }

    pub fn to_packet(&self) -> crate::utils::errors::Result<Packet> {
        Ok(Packet {
            id: Self::generate_packet_id(),
            packet_type: "kdeconnect.identity".to_string(),
            body: serde_json::to_value(self).map_err(|e| {
                crate::utils::errors::Error::SerializationError(format!(
                    "Failed to serialize identity: {}",
                    e
                ))
            })?,
            payload_size: None,
            payload_transfer_info: None,
        })
    }

    /// Identity packet for sending over a TCP connection (plaintext or the
    /// encrypted re-exchange). Per the protocol, `tcpPort` MUST be present in
    /// the UDP broadcast identity but MUST NOT appear in the TCP identity.
    pub fn to_tcp_packet(&self) -> crate::utils::errors::Result<Packet> {
        let mut tcp_identity = self.clone();
        tcp_identity.tcp_port = None;
        tcp_identity.to_packet()
    }

    pub fn from_packet(packet: Packet) -> crate::utils::errors::Result<Self> {
        if packet.packet_type != "kdeconnect.identity" {
            return Err(crate::utils::errors::Error::InvalidPacket(format!(
                "Expected kdeconnect.identity, got {}",
                packet.packet_type
            )));
        }

        let identity: Identity = packet.body_as("identity")?;

        if identity.device_id.is_empty() {
            return Err(crate::utils::errors::Error::validation(
                "device_id must not be empty".to_string(),
                Some("device_id".to_string()),
            ));
        }
        if identity.device_id.len() > 128 {
            return Err(crate::utils::errors::Error::validation(
                "device_id must not exceed 128 characters".to_string(),
                Some("device_id".to_string()),
            ));
        }
        // Android sanitizes received device names (filter invalid chars,
        // trim, cap at 32) instead of rejecting on length
        // (DeviceInfo.fromIdentityPacketAndCert); only a name that is blank
        // AFTER sanitizing fails isValidIdentityPacket.
        let identity = Identity {
            device_name: sanitize_device_name(&identity.device_name),
            ..identity
        };
        if identity.device_name.is_empty() {
            return Err(crate::utils::errors::Error::validation(
                "device_name must not be blank".to_string(),
                Some("device_name".to_string()),
            ));
        }
        if identity.protocol_version == 0 {
            return Err(crate::utils::errors::Error::validation(
                "protocol_version must not be 0".to_string(),
                Some("protocol_version".to_string()),
            ));
        }

        crate::protocol::crypto::validate_device_id(&identity.device_id)?;

        Ok(identity)
    }

    pub fn generate_packet_id() -> i64 {
        let uuid = uuid::Uuid::new_v4();
        let bytes = uuid.as_bytes();
        let mut id: i64 = 0;
        for &byte in bytes.iter().take(8) {
            id = (id << 8) | (byte as i64);
        }
        id
    }
}

/// Wire representation of `payloadSize`: a non-negative byte count, or the
/// kde-only `-1` "endless stream" sentinel (`core/networkpacket.h:85`,
/// `qint64 payloadSize`, `-1` means the sender doesn't know how much it
/// will send; `filetransferjob.cpp:108-110` receives it by streaming to a
/// clean EOF instead of checking a declared count). Before this type
/// existed `Packet.payload_size` was `Option<u64>`, so a kdeconnect-kde
/// peer sending the sentinel failed deserialization of the WHOLE packet,
/// not just the transfer (parity-checklist.md gap 7's live defect). Any
/// value other than a non-negative integer or exactly `-1` is rejected —
/// same lenient-but-bounded posture as `deserialize_lenient_i64` above for
/// `id`. We never SEND `-1` (no producer; `with_payload_size` stays `u64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadSize {
    Known(u64),
    /// `payloadSize: -1` — endless stream, no declared byte count.
    Stream,
}

impl PayloadSize {
    /// The declared byte count, or `None` for the endless-stream sentinel.
    /// Callers with no streaming support (e.g. the notification plugin's
    /// bounded icon transfer) use this to decline exactly as they would a
    /// missing `payloadSize`.
    pub fn known(self) -> Option<u64> {
        match self {
            Self::Known(n) => Some(n),
            Self::Stream => None,
        }
    }
}

impl std::fmt::Display for PayloadSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Known(n) => write!(f, "{n}"),
            Self::Stream => write!(f, "-1 (stream)"),
        }
    }
}

impl Serialize for PayloadSize {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            // -1 must round-trip: a kde peer re-reads what we relay.
            Self::Known(n) => serializer.serialize_u64(*n),
            Self::Stream => serializer.serialize_i64(-1),
        }
    }
}

impl<'de> Deserialize<'de> for PayloadSize {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let n = i64::deserialize(deserializer)?;
        match n {
            -1 => Ok(Self::Stream),
            n if n >= 0 => Ok(Self::Known(n as u64)),
            other => Err(Error::custom(format!(
                "payloadSize must be a non-negative integer or -1 (endless stream), got {other}"
            ))),
        }
    }
}

/// KDE Connect packet
///
/// All communication uses this packet format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Packet {
    #[serde(deserialize_with = "deserialize_lenient_i64")]
    pub id: i64,

    #[serde(rename = "type")]
    pub packet_type: String,

    pub body: serde_json::Value,

    #[serde(skip_serializing_if = "Option::is_none", rename = "payloadSize")]
    pub payload_size: Option<PayloadSize>,

    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "payloadTransferInfo"
    )]
    pub payload_transfer_info: Option<serde_json::Value>,
}

impl Packet {
    pub fn new(packet_type: String, body: serde_json::Value) -> Self {
        Self {
            id: Identity::generate_packet_id(),
            packet_type,
            body,
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    pub fn with_payload_size(mut self, size: u64) -> Self {
        self.payload_size = Some(PayloadSize::Known(size));
        self
    }

    pub fn with_payload_transfer_info(mut self, info: serde_json::Value) -> Self {
        self.payload_transfer_info = Some(info);
        self
    }

    pub fn ping() -> Self {
        Self::new("kdeconnect.ping".to_string(), serde_json::json!({}))
    }

    /// Deserialize the packet body into a typed struct.
    ///
    /// This is a convenience method that replaces the common pattern of
    /// `serde_json::from_value(packet.body).map_err(...)`.
    pub fn body_as<T: serde::de::DeserializeOwned>(
        &self,
        context: &str,
    ) -> crate::utils::errors::Result<T> {
        serde_json::from_value(self.body.clone()).map_err(|e| {
            crate::utils::errors::Error::DeserializationError(format!(
                "Failed to parse {}: {}",
                context, e
            ))
        })
    }

    pub fn pair_request() -> Self {
        Self::pair_request_with_timestamp(chrono::Utc::now().timestamp())
    }

    /// Pair request carrying a specific v8 timestamp. The pairing handler
    /// records this exact timestamp so both sides compute the same SAS
    /// (Android: getVerificationKey uses the REQUEST's timestamp).
    pub fn pair_request_with_timestamp(timestamp: i64) -> Self {
        Self::new(
            "kdeconnect.pair".to_string(),
            serde_json::json!({
                "pair": true,
                "timestamp": timestamp
            }),
        )
    }

    pub fn pair_response(accepted: bool) -> Self {
        Self::new(
            "kdeconnect.pair".to_string(),
            serde_json::json!({
                "pair": accepted,
                "timestamp": chrono::Utc::now().timestamp()
            }),
        )
    }

    pub fn is_identity(&self) -> bool {
        self.packet_type == "kdeconnect.identity"
    }

    pub fn is_pair(&self) -> bool {
        self.packet_type == "kdeconnect.pair"
    }

    pub fn is_ping(&self) -> bool {
        self.packet_type == "kdeconnect.ping"
    }
}

/// Pairing request information
#[derive(Debug, Clone)]
pub struct PairingRequest {
    pub device_id: DeviceId,
    pub created_at: DateTime<Utc>,
    pub timeout: chrono::Duration,
    /// The v8 pair-packet timestamp. Both sides must compute the SAS over
    /// this exact value (Android PairingHandler.pairingTimestamp), so the
    /// timestamp from the pair packet is recorded here, not re-sampled.
    pub pair_timestamp: i64,
}

impl PairingRequest {
    pub fn new(device_id: DeviceId) -> Self {
        Self {
            device_id,
            created_at: Utc::now(),
            timeout: chrono::Duration::minutes(30),
            pair_timestamp: Utc::now().timestamp(),
        }
    }

    pub fn with_pair_timestamp(mut self, pair_timestamp: i64) -> Self {
        self.pair_timestamp = pair_timestamp;
        self
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() > self.created_at + self.timeout
    }

    pub fn time_remaining(&self) -> chrono::Duration {
        let expires_at = self.created_at + self.timeout;
        expires_at - Utc::now()
    }
}

/// Connection information
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub device_id: DeviceId,
    pub connected_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub generation: u64,
}

impl ConnectionInfo {
    pub fn new(device_id: DeviceId, generation: u64) -> Self {
        let now = Utc::now();
        Self {
            device_id,
            connected_at: now,
            last_activity: now,
            packets_sent: 0,
            packets_received: 0,
            generation,
        }
    }

    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }

    pub fn increment_sent(&mut self) {
        self.packets_sent += 1;
        self.update_activity();
    }

    pub fn increment_received(&mut self) {
        self.packets_received += 1;
        self.update_activity();
    }

    pub fn duration(&self) -> chrono::Duration {
        Utc::now() - self.connected_at
    }

    pub fn is_idle(&self, threshold: chrono::Duration) -> bool {
        Utc::now() - self.last_activity > threshold
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_identity_creation() {
        let identity = Identity::new(
            "test-id".to_string(),
            "Test Device".to_string(),
            DeviceType::Phone,
            vec!["kdeconnect.ping".to_string()],
            vec!["kdeconnect.ping".to_string()],
        );

        assert_eq!(identity.device_id, "test-id");
        assert_eq!(identity.device_name, "Test Device");
        assert_eq!(identity.device_type, "phone");
        assert_eq!(identity.protocol_version, PROTOCOL_VERSION);
        assert_eq!(identity.tcp_port, Some(DEFAULT_TCP_PORT));
    }

    #[test]
    fn test_identity_packet_conversion() {
        let identity = Identity::new(
            "test-idaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            "Test Device".to_string(),
            DeviceType::Phone,
            vec![],
            vec![],
        );

        let packet = identity.to_packet().expect("Value expected to be present");
        assert_eq!(packet.packet_type, "kdeconnect.identity");
        assert!(packet.is_identity());

        let recovered = Identity::from_packet(packet).expect("Value expected to be present");
        assert_eq!(recovered.device_id, identity.device_id);
        assert_eq!(recovered.device_name, identity.device_name);
    }

    #[test]
    fn test_identity_deserialize_with_target_fields() {
        let json = serde_json::json!({
            "deviceId": "phone-id",
            "deviceName": "Phone",
            "deviceType": "phone",
            "protocolVersion": 8,
            "targetDeviceId": "fedora-id",
            "targetProtocolVersion": "8"
        });

        let identity: Identity =
            serde_json::from_value(json).expect("Value expected to be present");
        assert_eq!(identity.device_id, "phone-id");
        assert_eq!(identity.target_device_id, Some("fedora-id".to_string()));
        // Android writes targetProtocolVersion as a string but reads it as an
        // int — accept the string form, type it numeric.
        assert_eq!(identity.target_protocol_version, Some(8));
        assert_eq!(identity.tcp_port, None);

        // The numeric wire form parses identically.
        let json_numeric = serde_json::json!({
            "deviceId": "phone-id",
            "deviceName": "Phone",
            "deviceType": "phone",
            "protocolVersion": 8,
            "targetProtocolVersion": 8
        });
        let identity: Identity =
            serde_json::from_value(json_numeric).expect("Value expected to be present");
        assert_eq!(identity.target_protocol_version, Some(8));
    }

    #[test]
    fn test_identity_serialize_skips_none_fields() {
        let identity = Identity::new(
            "test-id".to_string(),
            "Test".to_string(),
            DeviceType::Desktop,
            vec![],
            vec![],
        );
        let json =
            serde_json::to_string(&identity).expect("Serialization of known types cannot fail");
        assert!(!json.contains("targetDeviceId"));
        assert!(!json.contains("targetProtocolVersion"));
        assert!(json.contains("tcpPort"));
    }

    #[test]
    fn test_packet_creation() {
        let packet = Packet::ping();
        assert_eq!(packet.packet_type, "kdeconnect.ping");
        assert!(packet.is_ping());

        let pair_req = Packet::pair_request();
        assert_eq!(pair_req.packet_type, "kdeconnect.pair");
        assert!(pair_req.is_pair());
    }

    #[test]
    fn test_pairing_request_expiration() {
        let mut request = PairingRequest::new("test-id".to_string());
        assert!(!request.is_expired());

        request.created_at = Utc::now() - chrono::Duration::hours(1);
        assert!(request.is_expired());
    }

    #[test]
    fn test_connection_info() {
        let mut info = ConnectionInfo::new("test-id".to_string(), 1);
        assert_eq!(info.packets_sent, 0);
        assert_eq!(info.packets_received, 0);

        info.increment_sent();
        assert_eq!(info.packets_sent, 1);

        info.increment_received();
        assert_eq!(info.packets_received, 1);

        assert!(!info.is_idle(chrono::Duration::seconds(1)));
    }

    #[test]
    fn test_packet_serialization() {
        let packet = Packet::new(
            "kdeconnect.test".to_string(),
            serde_json::json!({"key": "value"}),
        );

        let json =
            serde_json::to_string(&packet).expect("Serialization of known types cannot fail");
        let deserialized: Packet =
            serde_json::from_str(&json).expect("Value expected to be present");

        assert_eq!(packet, deserialized);
    }

    // Conformance: Android's NetworkPacket.kt uses camelCase for the payload
    // fields (`payloadSize`, `payloadTransferInfo`). Emitting snake_case means
    // Android silently drops them and payload transfers never happen.
    #[test]
    fn test_packet_payload_fields_serialize_camel_case() {
        let packet = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({}),
        )
        .with_payload_size(1024)
        .with_payload_transfer_info(serde_json::json!({"port": 1739}));
        let json = serde_json::to_string(&packet).expect("serialize");
        assert!(json.contains("\"payloadSize\":1024"), "wire json: {json}");
        assert!(
            json.contains("\"payloadTransferInfo\""),
            "wire json: {json}"
        );
        assert!(!json.contains("payload_size"), "wire json: {json}");
        assert!(!json.contains("payload_transfer_info"), "wire json: {json}");
    }

    #[test]
    fn test_packet_payload_fields_deserialize_android_wire_format() {
        let wire = r#"{"id":1,"type":"kdeconnect.share.request","body":{},"payloadSize":1024,"payloadTransferInfo":{"port":1739}}"#;
        let packet: Packet = serde_json::from_str(wire).expect("deserialize android wire format");
        assert_eq!(packet.payload_size, Some(PayloadSize::Known(1024)));
        assert_eq!(
            packet.payload_transfer_info,
            Some(serde_json::json!({"port": 1739}))
        );
    }

    // Gap 7 (parity-checklist.md § Payload transfers, vk #998 Task 2.3):
    // kdeconnect-kde's payloadSize=-1 endless-stream sentinel
    // (core/networkpacket.h:85) used to fail deserialization of the WHOLE
    // packet, not just the transfer, because `payload_size` was
    // `Option<u64>`. Fixture: tests/fixtures/upstream-wire/share/
    // payload_size_endless_stream.json (hand-authored from source; see
    // provenance.yaml).
    #[test]
    fn test_payload_size_deserializes_endless_stream_sentinel() {
        let fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/upstream-wire/share/payload_size_endless_stream.json"
        ))
        .expect("fixture must be readable");
        let packet: Packet =
            serde_json::from_str(&fixture).expect("payloadSize: -1 must deserialize the packet");
        assert_eq!(packet.payload_size, Some(PayloadSize::Stream));
    }

    #[test]
    fn test_payload_size_stream_round_trips_back_to_negative_one() {
        // KDE peers re-read what we relay: -1 in, -1 out.
        let packet = Packet::new("kdeconnect.share.request".to_string(), serde_json::json!({}));
        let mut packet = packet;
        packet.payload_size = Some(PayloadSize::Stream);
        let json = serde_json::to_string(&packet).expect("serialize");
        assert!(
            json.contains("\"payloadSize\":-1"),
            "wire json must carry -1, got: {json}"
        );
        let round_tripped: Packet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped.payload_size, Some(PayloadSize::Stream));
    }

    #[test]
    fn test_payload_size_negative_two_rejected() {
        let wire = r#"{"id":1,"type":"kdeconnect.share.request","body":{},"payloadSize":-2}"#;
        let result: std::result::Result<Packet, _> = serde_json::from_str(wire);
        assert!(
            result.is_err(),
            "payloadSize values other than -1 must be rejected, not silently accepted"
        );
    }

    #[test]
    fn test_payload_size_known_value_unaffected() {
        let packet = Packet::new("kdeconnect.ping".to_string(), serde_json::json!({}))
            .with_payload_size(4096);
        assert_eq!(packet.payload_size, Some(PayloadSize::Known(4096)));
        assert_eq!(packet.payload_size.and_then(PayloadSize::known), Some(4096));
        assert_eq!(PayloadSize::Stream.known(), None);
    }

    #[test]
    fn test_packet_with_no_payload_size_field_deserializes_as_none() {
        // A missing `payloadSize` key (e.g. kdeconnect.ping) must still
        // deserialize to `None`, same as before this type existed — the
        // custom `Deserialize for PayloadSize` only runs when the field is
        // PRESENT; serde's field-level `Option<T>` handling (recognizing
        // the field type syntactically) still supplies `None` on absence.
        let wire = r#"{"id":1,"type":"kdeconnect.ping","body":{}}"#;
        let packet: Packet = serde_json::from_str(wire).expect("deserialize");
        assert_eq!(packet.payload_size, None);
    }

    // Conformance: tcpPort MUST be present in the UDP broadcast identity but
    // MUST NOT be present in the TCP identity packet (konnect packet.py:62,
    // Android LanLinkProvider).
    #[test]
    fn test_tcp_identity_packet_omits_tcp_port() {
        let identity = Identity::new(
            "a".repeat(32),
            "Test".to_string(),
            DeviceType::Laptop,
            vec![],
            vec![],
        );
        let udp_body = identity
            .to_packet()
            .expect("Value expected to be present")
            .body;
        assert!(
            udp_body.get("tcpPort").is_some(),
            "UDP broadcast identity must carry tcpPort"
        );
        let tcp_body = identity
            .to_tcp_packet()
            .expect("Value expected to be present")
            .body;
        assert!(
            tcp_body.get("tcpPort").is_none(),
            "TCP identity must NOT carry tcpPort: {tcp_body}"
        );
    }

    // Conformance: protocol v8 pair packets carry a timestamp (Android
    // PairingHandler.kt rejects pairs whose timestamp is off by >1800s) —
    // that includes the response, not just the request.
    #[test]
    fn test_pair_response_includes_timestamp() {
        let before = chrono::Utc::now().timestamp() - 1;
        for accepted in [true, false] {
            let packet = Packet::pair_response(accepted);
            let ts = packet.body["timestamp"]
                .as_i64()
                .expect("pair_response must include a timestamp (protocol v8)");
            assert!(ts >= before, "timestamp must be current");
        }
    }

    #[test]
    fn test_identity_from_packet_empty_device_id_rejected() {
        let packet = Packet::new(
            "kdeconnect.identity".to_string(),
            serde_json::json!({
                "deviceId": "",
                "deviceName": "Test",
                "deviceType": "phone",
                "protocolVersion": 8
            }),
        );
        assert!(Identity::from_packet(packet).is_err());
    }

    #[test]
    fn test_identity_from_packet_long_device_id_rejected() {
        let packet = Packet::new(
            "kdeconnect.identity".to_string(),
            serde_json::json!({
                "deviceId": "x".repeat(129),
                "deviceName": "Test",
                "deviceType": "phone",
                "protocolVersion": 8
            }),
        );
        assert!(Identity::from_packet(packet).is_err());
    }

    #[test]
    fn test_identity_from_packet_long_device_name_sanitized_not_rejected() {
        // Android never rejects on name length — it truncates to 32
        // (DeviceHelper.kt:159). The doc's "max 32" is what Android emits,
        // not a receive-side rule.
        let packet = Packet::new(
            "kdeconnect.identity".to_string(),
            serde_json::json!({
                "deviceId": "test-id-aaaaaaaaaaaaaaaaaaaaaaaaa",
                "deviceName": "x".repeat(257),
                "deviceType": "phone",
                "protocolVersion": 8
            }),
        );
        let identity = Identity::from_packet(packet).expect("Value expected to be present");
        assert_eq!(identity.device_name.len(), 32);
    }

    #[test]
    fn test_identity_from_packet_device_name_invalid_chars_filtered() {
        // Android's NAME_INVALID_CHARACTERS_REGEX: ["',;:.!?()\[\]<>]
        // (DeviceHelper.kt:40).
        let packet = Packet::new(
            "kdeconnect.identity".to_string(),
            serde_json::json!({
                "deviceId": "test-id-aaaaaaaaaaaaaaaaaaaaaaaaa",
                "deviceName": "  George's (Pixel) <9>!  ",
                "deviceType": "phone",
                "protocolVersion": 8
            }),
        );
        let identity = Identity::from_packet(packet).expect("Value expected to be present");
        assert_eq!(identity.device_name, "Georges Pixel 9");
    }

    #[test]
    fn test_identity_from_packet_blank_device_name_rejected() {
        // isValidIdentityPacket: the name must be non-blank AFTER filtering.
        let packet = Packet::new(
            "kdeconnect.identity".to_string(),
            serde_json::json!({
                "deviceId": "test-id",
                "deviceName": "  <>!!  ",
                "deviceType": "phone",
                "protocolVersion": 8
            }),
        );
        assert!(Identity::from_packet(packet).is_err());
    }

    #[test]
    fn test_identity_from_packet_zero_protocol_version_rejected() {
        let packet = Packet::new(
            "kdeconnect.identity".to_string(),
            serde_json::json!({
                "deviceId": "test-id",
                "deviceName": "Test",
                "deviceType": "phone",
                "protocolVersion": 0
            }),
        );
        assert!(Identity::from_packet(packet).is_err());
    }

    #[test]
    fn test_identity_from_packet_path_traversal_device_id_rejected() {
        let packet = Packet::new(
            "kdeconnect.identity".to_string(),
            serde_json::json!({
                "deviceId": "../etc/passwd",
                "deviceName": "Test",
                "deviceType": "phone",
                "protocolVersion": 8
            }),
        );
        assert!(Identity::from_packet(packet).is_err());
    }

    #[test]
    fn test_pair_response_accepted() {
        let packet = Packet::pair_response(true);
        assert_eq!(packet.packet_type, "kdeconnect.pair");
        assert!(packet.is_pair());
        assert_eq!(packet.body["pair"], true);
        assert!(packet.body["timestamp"].is_i64());
    }

    #[test]
    fn test_pair_response_rejected() {
        let packet = Packet::pair_response(false);
        assert_eq!(packet.packet_type, "kdeconnect.pair");
        assert!(packet.is_pair());
        assert_eq!(packet.body["pair"], false);
        assert!(packet.body["timestamp"].is_i64());
    }

    #[test]
    fn test_pairing_request_time_remaining_positive() {
        let request = PairingRequest::new("test-id".to_string());
        let remaining = request.time_remaining();
        assert!(remaining.num_seconds() > 0);
        assert!(remaining.num_seconds() <= 1800);
    }

    #[test]
    fn test_pairing_request_time_remaining_negative_when_expired() {
        let mut request = PairingRequest::new("test-id".to_string());
        request.created_at = Utc::now() - chrono::Duration::hours(1);
        let remaining = request.time_remaining();
        assert!(remaining.num_seconds() < 0);
    }

    #[test]
    fn test_connection_info_duration() {
        let info = ConnectionInfo::new("test-id".to_string(), 0);
        let duration = info.duration();
        assert!(duration.num_seconds() >= 0);
        assert!(duration.num_seconds() < 2);
    }

    #[test]
    fn test_connection_info_duration_increases() {
        let info = ConnectionInfo::new("test-id".to_string(), 0);
        let d1 = info.duration().num_milliseconds();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let d2 = info.duration().num_milliseconds();
        assert!(d2 >= d1);
    }
}

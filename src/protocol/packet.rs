//! Packet serialization for KDE Connect protocol
//!
//! Following AI-first design principles:
//! - JSON format (human and machine readable)
//! - Newline delimiter (for streaming)
//! - Clear error messages
//!
//! Single Responsibility: Serialize and deserialize packets

use crate::protocol::types::Packet;
use crate::utils::errors::{Error, Result};

/// Packet serializer
///
/// Handles encoding and decoding of KDE Connect packets.
/// Packets are JSON objects terminated by a newline character.
pub struct PacketSerializer;

impl PacketSerializer {
    /// Serialize a packet to bytes
    ///
    /// Format: JSON + newline delimiter
    ///
    /// # Arguments
    /// * `packet` - The packet to serialize
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Serialized packet bytes
    /// * `Err(Error)` - Serialization error
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_connect::protocol::packet::PacketSerializer;
    /// use rust_connect::protocol::types::Packet;
    ///
    /// let packet = Packet::ping();
    /// let bytes = PacketSerializer::serialize(&packet).expect("Value expected to be present");
    /// assert!(bytes.ends_with(&[b'\n']));
    /// ```
    pub fn serialize(packet: &Packet) -> Result<Vec<u8>> {
        let json =
            serde_json::to_string(packet).map_err(|e| Error::SerializationError(e.to_string()))?;

        let mut bytes = json.into_bytes();
        bytes.push(b'\n'); // Add newline delimiter

        Ok(bytes)
    }

    /// Deserialize bytes to a packet
    ///
    /// Expects JSON format, optionally terminated by newline.
    ///
    /// # Arguments
    /// * `bytes` - The bytes to deserialize
    ///
    /// # Returns
    /// * `Ok(Packet)` - Deserialized packet
    /// * `Err(Error)` - Deserialization error
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_connect::protocol::packet::PacketSerializer;
    /// use rust_connect::protocol::types::Packet;
    ///
    /// let packet = Packet::ping();
    /// let bytes = PacketSerializer::serialize(&packet).expect("Value expected to be present");
    /// let deserialized = PacketSerializer::deserialize(&bytes).expect("Value expected to be present");
    /// assert_eq!(packet.packet_type, deserialized.packet_type);
    /// ```
    pub fn deserialize(bytes: &[u8]) -> Result<Packet> {
        // The references' steady-state line cap (landevicelink.cpp:19,
        // LanLink.java:46). The tighter 512 KiB identity cap
        // (LanLinkProvider.java:68) is enforced at the stream layer by
        // read_line_bounded on the pre-auth reads, never reaching here.
        const MAX_PACKET_SIZE: usize = 32 * 1024 * 1024;

        if bytes.len() > MAX_PACKET_SIZE {
            return Err(Error::PacketTooLarge {
                size: bytes.len(),
                max: MAX_PACKET_SIZE,
            });
        }

        // Remove trailing newline if present
        let bytes = if bytes.last() == Some(&b'\n') {
            &bytes[..bytes.len() - 1]
        } else {
            bytes
        };

        let json = std::str::from_utf8(bytes)
            .map_err(|e| Error::DeserializationError(format!("Invalid UTF-8: {}", e)))?;

        let trimmed = json.trim();
        if trimmed.is_empty() {
            return Err(Error::DeserializationError("Empty packet".to_string()));
        }

        serde_json::from_str(trimmed).map_err(|e| Error::DeserializationError(e.to_string()))
    }

    /// Serialize a packet to a string (for debugging)
    ///
    /// # Arguments
    /// * `packet` - The packet to serialize
    ///
    /// # Returns
    /// * `Ok(String)` - JSON string representation
    /// * `Err(Error)` - Serialization error
    pub fn to_string(packet: &Packet) -> Result<String> {
        serde_json::to_string_pretty(packet).map_err(|e| Error::SerializationError(e.to_string()))
    }

    /// Deserialize a string to a packet (for debugging)
    ///
    /// # Arguments
    /// * `s` - The JSON string to deserialize
    ///
    /// # Returns
    /// * `Ok(Packet)` - Deserialized packet
    /// * `Err(Error)` - Deserialization error
    pub fn from_string(s: &str) -> Result<Packet> {
        serde_json::from_str(s).map_err(|e| Error::DeserializationError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn test_serialize_ping() {
        let packet = Packet::ping();
        let bytes = PacketSerializer::serialize(&packet).expect("Value expected to be present");

        // Should end with newline
        assert_eq!(bytes.last(), Some(&b'\n'));

        // Should be valid JSON
        let json_str =
            std::str::from_utf8(&bytes[..bytes.len() - 1]).expect("Value expected to be present");
        let _: serde_json::Value =
            serde_json::from_str(json_str).expect("Value expected to be present");
    }

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let original = Packet::new(
            "kdeconnect.test".to_string(),
            json!({"key": "value", "number": 42}),
        );

        let bytes = PacketSerializer::serialize(&original).expect("Value expected to be present");
        let deserialized =
            PacketSerializer::deserialize(&bytes).expect("Value expected to be present");

        assert_eq!(original.packet_type, deserialized.packet_type);
        assert_eq!(original.body, deserialized.body);
    }

    #[test]
    fn test_deserialize_with_newline() {
        let json = r#"{"id":123,"type":"kdeconnect.ping","body":{}}"#;
        let bytes_with_newline = format!("{}\n", json).into_bytes();

        let packet = PacketSerializer::deserialize(&bytes_with_newline)
            .expect("Value expected to be present");
        assert_eq!(packet.packet_type, "kdeconnect.ping");
    }

    #[test]
    fn test_deserialize_without_newline() {
        let json = r#"{"id":123,"type":"kdeconnect.ping","body":{}}"#;
        let bytes = json.as_bytes();

        let packet = PacketSerializer::deserialize(bytes).expect("Value expected to be present");
        assert_eq!(packet.packet_type, "kdeconnect.ping");
    }

    #[test]
    fn test_serialize_identity_packet() {
        use crate::device::types::DeviceType;
        use crate::protocol::types::Identity;

        let identity = Identity::new(
            "test-idaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            "Test Device".to_string(),
            DeviceType::Phone,
            vec!["kdeconnect.ping".to_string()],
            vec!["kdeconnect.ping".to_string()],
        );

        let packet = identity.to_packet().expect("Value expected to be present");
        let bytes = PacketSerializer::serialize(&packet).expect("Value expected to be present");
        let deserialized =
            PacketSerializer::deserialize(&bytes).expect("Value expected to be present");

        assert_eq!(packet.packet_type, deserialized.packet_type);

        // Verify identity can be recovered
        let recovered_identity =
            Identity::from_packet(deserialized).expect("Value expected to be present");
        assert_eq!(identity.device_id, recovered_identity.device_id);
        assert_eq!(identity.device_name, recovered_identity.device_name);
    }

    #[test]
    fn test_serialize_pair_request() {
        let packet = Packet::pair_request();
        let bytes = PacketSerializer::serialize(&packet).expect("Value expected to be present");
        let deserialized =
            PacketSerializer::deserialize(&bytes).expect("Value expected to be present");

        assert_eq!(packet.packet_type, deserialized.packet_type);
        assert_eq!(packet.body["pair"], true);
    }

    #[test]
    fn test_deserialize_invalid_json() {
        let invalid = b"not valid json\n";
        let result = PacketSerializer::deserialize(invalid);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_invalid_utf8() {
        let invalid = vec![0xFF, 0xFE, 0xFD];
        let result = PacketSerializer::deserialize(&invalid);
        assert!(result.is_err());
    }

    #[test]
    fn test_to_string_pretty() {
        let packet = Packet::new("kdeconnect.test".to_string(), json!({"key": "value"}));

        let string =
            PacketSerializer::to_string(&packet).expect("Serialization of known types cannot fail");
        assert!(string.contains("kdeconnect.test"));
        assert!(string.contains("key"));
        assert!(string.contains("value"));
    }

    #[test]
    fn test_from_string() {
        let json = r#"{"id":123,"type":"kdeconnect.ping","body":{}}"#;
        let packet = PacketSerializer::from_string(json).expect("Value expected to be present");
        assert_eq!(packet.packet_type, "kdeconnect.ping");
    }

    #[test]
    fn test_serialize_large_packet() {
        // Test with a large body
        let mut large_body = serde_json::Map::new();
        for i in 0..1000 {
            large_body.insert(format!("key_{}", i), json!(format!("value_{}", i)));
        }

        let packet = Packet::new("kdeconnect.test".to_string(), json!(large_body));

        let bytes = PacketSerializer::serialize(&packet).expect("Value expected to be present");
        let deserialized =
            PacketSerializer::deserialize(&bytes).expect("Value expected to be present");

        assert_eq!(packet.packet_type, deserialized.packet_type);
        assert_eq!(packet.body, deserialized.body);
    }

    #[test]
    fn test_deserialize_empty_line() {
        let result = PacketSerializer::deserialize(b"\n");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Empty packet"));
    }

    #[test]
    fn test_deserialize_whitespace_only() {
        let result = PacketSerializer::deserialize(b"   \n");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Empty packet"));
    }

    #[test]
    fn test_deserialize_leading_trailing_whitespace() {
        let json = r#"  {"id":123,"type":"kdeconnect.ping","body":{}}  "#;
        let bytes = format!("{}\n", json).into_bytes();

        let packet = PacketSerializer::deserialize(&bytes).expect("Value expected to be present");
        assert_eq!(packet.packet_type, "kdeconnect.ping");
    }

    #[test]
    fn test_deserialize_packet_too_large() {
        // The wire cap is 32 MiB — the references' steady-state limit
        // (landevicelink.cpp:19, LanLink.java:46). The 512 KiB cap applies
        // to the pre-auth identity read at the stream layer.
        let oversized: Vec<u8> = vec![b'x'; 32 * 1024 * 1024 + 1];
        let result = PacketSerializer::deserialize(&oversized);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Packet too large"));
        assert!(msg.contains("33554433"));
        assert!(msg.contains("33554432"));
    }

    /// Parity gap 5 (docs/parity-checklist.md): both references never READ
    /// the packet id, so its wire type is unconstrained — a JSON string id
    /// must parse (same lenient handling as targetProtocolVersion).
    #[test]
    fn test_deserialize_accepts_string_id() {
        let bytes = br#"{"id":"12345","type":"kdeconnect.ping","body":{}}"#;
        let packet = PacketSerializer::deserialize(bytes)
            .expect("a string id must deserialize (the refs never read it)");
        assert_eq!(packet.id, 12345);

        let bytes = br#"{"id":12345,"type":"kdeconnect.ping","body":{}}"#;
        let packet =
            PacketSerializer::deserialize(bytes).expect("a numeric id must still deserialize");
        assert_eq!(packet.id, 12345);
    }

    #[test]
    fn test_deserialize_packet_exactly_at_limit() {
        let valid_json = r#"{"id":1,"type":"kdeconnect.ping","body":{}}"#;
        let mut bytes = vec![b' '; 524_288 - valid_json.len() - 1];
        bytes.extend_from_slice(valid_json.as_bytes());
        bytes.push(b'\n');
        assert_eq!(bytes.len(), 524_288);

        let result = PacketSerializer::deserialize(&bytes);
        assert!(
            result.is_ok(),
            "Packet at exactly the 512KiB limit should deserialize"
        );
    }
}

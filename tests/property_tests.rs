use proptest::prelude::*;
use rust_connect::protocol::{Packet, PacketSerializer, PayloadSize};

fn arbitrary_payload_size() -> impl Strategy<Value = PayloadSize> {
    prop_oneof![
        any::<u64>().prop_map(PayloadSize::Known),
        Just(PayloadSize::Stream),
    ]
}

fn arbitrary_json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(serde_json::Value::from),
        any::<u32>().prop_map(|n| serde_json::Value::Number(n.into())),
        ".*".prop_map(serde_json::Value::String),
    ];
    leaf.prop_recursive(3, 16, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
            prop::collection::hash_map(".*", inner, 0..4)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

#[test]
fn property_serialize_deserialize_roundtrip() {
    let body_strat = arbitrary_json_value();
    proptest!(|(
        pt in "kdeconnect\\.[a-z._]{1,50}",
        body in body_strat,
        id in any::<i64>(),
        payload_size in prop::option::of(arbitrary_payload_size()),
    )| {
        let packet = Packet {
            id,
            packet_type: pt.clone(),
            body: body.clone(),
            payload_size,
            payload_transfer_info: None,
        };

        let bytes = PacketSerializer::serialize(&packet).unwrap();
        prop_assert!(bytes.ends_with(b"\n"));

        let recovered = PacketSerializer::deserialize(&bytes).unwrap();
        assert_eq!(pt, recovered.packet_type);
        assert_eq!(id, recovered.id);
        assert_eq!(payload_size, recovered.payload_size);

        // Body should be semantically equal (JSON values, not byte-identical)
        // serde_json may reorder object keys or normalize floats
        assert_eq!(packet.body, recovered.body, "Body should be semantically equal");
    });
}

#[test]
fn property_packet_type_invariants() {
    proptest!(|(pt in "kdeconnect\\.[a-z._]{1,50}")| {
        let packet = Packet::new(pt.clone(), serde_json::json!({}));

        let bytes = PacketSerializer::serialize(&packet).unwrap();
        let recovered = PacketSerializer::deserialize(&bytes).unwrap();
        assert_eq!(pt, recovered.packet_type);
    });
}

#[test]
fn fuzz_deserializer_never_panics() {
    proptest!(|(bytes in prop::collection::vec(any::<u8>(), 0..2048))| {
        let _ = PacketSerializer::deserialize(&bytes);
    });
}

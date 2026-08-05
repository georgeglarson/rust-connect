use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rust_connect::device::types::DeviceType;
use rust_connect::protocol::types::{Identity, Packet};

fn bench_packet_creation(c: &mut Criterion) {
    c.bench_function("packet_creation", |b| {
        b.iter(|| {
            Packet::new(
                "kdeconnect.ping".to_string(),
                serde_json::json!({ "timestamp": chrono::Utc::now().timestamp() }),
            )
        })
    });
}

fn bench_packet_serialization(c: &mut Criterion) {
    let packet = Packet::new(
        "kdeconnect.ping".to_string(),
        serde_json::json!({ "timestamp": 1234567890 }),
    );
    c.bench_function("packet_serialize", |b| {
        b.iter(|| {
            let _ = serde_json::to_string(black_box(&packet));
        })
    });
}

fn bench_packet_deserialization(c: &mut Criterion) {
    let packet = Packet::new(
        "kdeconnect.ping".to_string(),
        serde_json::json!({ "timestamp": 1234567890 }),
    );
    let json = serde_json::to_string(&packet).unwrap();
    c.bench_function("packet_deserialize", |b| {
        b.iter(|| {
            let _: Packet = serde_json::from_str(black_box(&json)).unwrap();
        })
    });
}

fn bench_identity_creation(c: &mut Criterion) {
    c.bench_function("identity_creation", |b| {
        b.iter(|| {
            Identity::new(
                "test-device-123".to_string(),
                "Test Device".to_string(),
                DeviceType::Desktop,
                vec!["kdeconnect.ping".to_string()],
                vec!["kdeconnect.ping".to_string()],
            )
        })
    });
}

fn bench_identity_to_packet(c: &mut Criterion) {
    let identity = Identity::new(
        "test-device-123".to_string(),
        "Test Device".to_string(),
        DeviceType::Desktop,
        vec!["kdeconnect.ping".to_string()],
        vec!["kdeconnect.ping".to_string()],
    );
    c.bench_function("identity_to_packet", |b| {
        b.iter(|| identity.to_packet().expect("identity serialization"))
    });
}

criterion_group!(
    benches,
    bench_packet_creation,
    bench_packet_serialization,
    bench_packet_deserialization,
    bench_identity_creation,
    bench_identity_to_packet,
);
criterion_main!(benches);

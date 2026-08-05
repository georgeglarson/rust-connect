#![no_main]

use libfuzzer_sys::fuzz_target;
use rust_connect::protocol::packet::PacketSerializer;
use rust_connect::protocol::types::{Identity, MAX_PORT, MIN_PORT};

// UDP discovery decode path, mirroring DiscoveryService::listen:
// deserialize -> is_identity gate -> Identity::from_packet (body parse,
// device_id/name validation, name sanitizing, crypto::validate_device_id)
// -> tcpPort range check. None of this may panic on attacker-controlled
// broadcast traffic.
fuzz_target!(|data: &[u8]| {
    let Ok(packet) = PacketSerializer::deserialize(data) else {
        return;
    };
    if !packet.is_identity() {
        return;
    }
    // Results are intentionally discarded — the signal is "never panics",
    // not the boolean values. black_box keeps the checks from being
    // optimized out.
    if let Ok(identity) = Identity::from_packet(packet) {
        let tcp_port = identity.tcp_port.unwrap_or(MIN_PORT);
        std::hint::black_box((MIN_PORT..=MAX_PORT).contains(&tcp_port));
    }
});

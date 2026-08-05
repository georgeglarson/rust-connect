#![no_main]

use libfuzzer_sys::fuzz_target;
use rust_connect::protocol::packet::PacketSerializer;

// Wire-parser entry point: arbitrary bytes from the network feed straight
// into PacketSerializer::deserialize (512 KiB cap, JSON parse). It must
// never panic on any input. When the input parses, the round trip
// serialize -> deserialize must not panic either.
fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = PacketSerializer::deserialize(data) {
        if let Ok(bytes) = PacketSerializer::serialize(&packet) {
            let _ = PacketSerializer::deserialize(&bytes);
        }
    }
});

//! Packet transcript recorder
//!
//! When the `RUST_CONNECT_TRANSCRIPT_DIR` environment variable names a
//! writable directory, every packet sent or received on a device link is
//! appended to `<dir>/<device_id>.jsonl`, one JSON object per line:
//!
//! ```json
//! {"ts": 1760000000000, "dir": "in", "type": "kdeconnect.ping", "body": {"id": 1, "type": "kdeconnect.ping", "body": {}}}
//! ```
//!
//! `body` is the FULL packet JSON exactly as it went over the wire, so a
//! capture drops into the replay harness (`protocol::replay`,
//! `tests/fixtures/*.jsonl`) unmodified.
//!
//! # Security posture
//!
//! Transcripts contain full packet bodies — SMS contents, clipboard text,
//! notification payloads. That is why recording is env-gated, OFF by
//! default, and writes only to a directory the operator explicitly names.
//! Treat a transcript directory like key material: restrict its
//! permissions, never commit captures, delete it after the debugging
//! session (and scrub a capture before turning it into a fixture).
//!
//! Cost when disabled is one `OnceCell` read per packet. When enabled the
//! append is a small blocking write on the packet path — acceptable for a
//! debugging facility, and another reason it is opt-in. A recording failure
//! logs once and never fails the packet path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use once_cell::sync::OnceCell;
use tracing::warn;

use crate::protocol::types::Packet;

/// Environment variable enabling transcript recording.
pub const TRANSCRIPT_DIR_ENV: &str = "RUST_CONNECT_TRANSCRIPT_DIR";

/// Direction of a recorded packet on the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Received from the peer.
    In,
    /// Sent to the peer.
    Out,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

static TRANSCRIPT_DIR: OnceCell<Option<PathBuf>> = OnceCell::new();
static WRITE_WARNED: AtomicBool = AtomicBool::new(false);

/// Resolved once: the env var is read on the first recorded packet and
/// cached for the process lifetime.
fn transcript_dir() -> Option<&'static Path> {
    TRANSCRIPT_DIR
        .get_or_init(|| std::env::var_os(TRANSCRIPT_DIR_ENV).map(PathBuf::from))
        .as_deref()
}

/// Record one packet on `device_id`'s link. No-op unless
/// `RUST_CONNECT_TRANSCRIPT_DIR` was set at first use; a recording failure
/// logs once and is otherwise swallowed — the packet path never fails
/// because of the recorder.
pub(crate) fn record(device_id: &str, direction: Direction, packet: &Packet) {
    record_to(transcript_dir(), device_id, direction, packet);
}

fn record_to(base_dir: Option<&Path>, device_id: &str, direction: Direction, packet: &Packet) {
    let Some(base_dir) = base_dir else {
        return;
    };
    if let Err(e) = append_entry(base_dir, device_id, direction, packet) {
        // Warn ONCE, not per-packet: a full disk would otherwise spam the
        // log on every packet of every link.
        if !WRITE_WARNED.swap(true, Ordering::Relaxed) {
            warn!(
                error = %e,
                event = "transcript_write_failed",
                "Transcript recording failed; continuing without it"
            );
        }
    }
}

fn append_entry(
    base_dir: &Path,
    device_id: &str,
    direction: Direction,
    packet: &Packet,
) -> std::io::Result<()> {
    use std::io::Write;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let body = serde_json::to_value(packet)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let entry = serde_json::json!({
        "ts": ts,
        "dir": direction.as_str(),
        "type": packet.packet_type.as_str(),
        "body": body,
    });
    let mut line = serde_json::to_string(&entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');

    // device_id is already path-safe (validate_device_id rejects '/', '\\',
    // "..", NUL and anything non-alphanumeric besides '-' and '_').
    let path = base_dir.join(format!("{}.jsonl", device_id));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::protocol::packet::PacketSerializer;

    #[test]
    fn test_record_to_writes_valid_jsonl() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let device_id = "transcript-device-aaaaaaaaaaaaaaa";
        let ping = Packet::ping();
        record_to(Some(temp.path()), device_id, Direction::In, &ping);
        record_to(
            Some(temp.path()),
            device_id,
            Direction::Out,
            &Packet::pair_response(true),
        );

        let content = std::fs::read_to_string(temp.path().join(format!("{}.jsonl", device_id)))
            .expect("Value expected to be present");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "one JSON object per recorded packet");

        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSONL");
        assert_eq!(first["dir"], "in");
        assert_eq!(first["type"], "kdeconnect.ping");
        assert!(first["ts"].is_u64(), "ts must be unix millis");
        // `body` is the full packet JSON: it must round-trip as a Packet.
        let packet: Packet =
            serde_json::from_value(first["body"].clone()).expect("body is a full packet JSON");
        assert_eq!(packet, ping);

        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("valid JSONL");
        assert_eq!(second["dir"], "out");
        assert_eq!(second["type"], "kdeconnect.pair");
    }

    #[test]
    fn test_record_to_none_is_noop() {
        // Disabled recorder: nothing to write to, nothing happens.
        record_to(None, "any-device", Direction::In, &Packet::ping());
    }

    #[test]
    fn test_record_noop_when_env_unset() {
        // The disabled-path guarantee only holds if the environment really
        // is unset; a recording environment can't exercise it.
        if std::env::var_os(TRANSCRIPT_DIR_ENV).is_some() {
            return;
        }
        assert!(transcript_dir().is_none());
        record("some-device", Direction::In, &Packet::ping());
    }

    #[test]
    fn test_write_failure_does_not_break_caller() {
        // A file where a directory is expected: every append fails, and the
        // recorder must swallow it (the packet path never sees the error).
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let not_a_dir = temp.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"").expect("Value expected to be present");

        record_to(
            Some(&not_a_dir),
            "any-device",
            Direction::In,
            &Packet::ping(),
        );
        // And again: the warn-once latch must not turn into a panic either.
        record_to(
            Some(&not_a_dir),
            "any-device",
            Direction::Out,
            &Packet::ping(),
        );
    }

    #[test]
    fn test_append_entry_round_trips_through_packet_serializer() {
        // The recorded body must be byte-compatible with the wire format the
        // replay harness re-sends.
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let packet = Packet::new(
            "kdeconnect.clipboard".to_string(),
            serde_json::json!({"content": "transcript"}),
        );
        append_entry(temp.path(), "dev", Direction::In, &packet)
            .expect("Value expected to be present");

        let content = std::fs::read_to_string(temp.path().join("dev.jsonl"))
            .expect("Value expected to be present");
        let entry: serde_json::Value =
            serde_json::from_str(content.trim_end()).expect("valid JSONL");
        let wire = serde_json::to_vec(&entry["body"]).expect("Value expected to be present");
        let parsed = PacketSerializer::deserialize(&wire).expect("wire-format packet");
        assert_eq!(parsed, packet);
    }
}

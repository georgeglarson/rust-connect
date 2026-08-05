//! Transcript fixture replay harness (test-helpers only)
//!
//! Replays a packet transcript — the exact JSONL format the recorder
//! (`protocol::transcript`) writes, so real captures drop in unmodified —
//! through REAL TCP loopback and the production `accept_incoming` path.
//! The harness plays the phone: it dials, serves TLS, sends the
//! transcript's `in` packets over the wire, and captures the daemon's `out`
//! packets for assertion. No mock stack: the packets cross real sockets,
//! real TLS, and the real connection registry.
//!
//! A transcript spans as many dials as the peer made: each inbound
//! `kdeconnect.identity` packet begins a new dial (a redial re-runs the
//! identity exchange), so `segment_by_dial` splits on it. Every dial gets
//! its own TCP connection and its own `accept_incoming`, exactly like the
//! production listener.

use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::protocol::connection::{ConnectionManager, TlsStream};
use crate::protocol::crypto::CertificateManager;
use crate::protocol::packet::PacketSerializer;
use crate::protocol::types::Packet;
use crate::utils::errors::{Error, Result};

/// One transcript line — the recorder's format.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TranscriptEntry {
    /// Unix millis when the packet was recorded.
    pub ts: u64,
    /// "in" (peer → daemon) or "out" (daemon → peer).
    pub dir: String,
    /// Packet type, e.g. "kdeconnect.battery".
    #[serde(rename = "type")]
    pub packet_type: String,
    /// The full packet JSON as it went over the wire.
    pub body: serde_json::Value,
}

impl TranscriptEntry {
    /// The recorded packet.
    pub fn packet(&self) -> Result<Packet> {
        serde_json::from_value(self.body.clone()).map_err(|e| {
            Error::DeserializationError(format!("transcript entry body is not a packet: {}", e))
        })
    }

    fn is_inbound_identity(&self) -> bool {
        self.dir == "in" && self.packet_type == "kdeconnect.identity"
    }
}

/// Read a transcript JSONL file (blank lines ignored).
pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEntry>> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        Error::ConnectionError(format!("Failed to read transcript {:?}: {}", path, e))
    })?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).map_err(|e| {
                Error::DeserializationError(format!(
                    "transcript {:?} line {} is not a transcript entry: {}",
                    path,
                    i + 1,
                    e
                ))
            })
        })
        .collect()
}

/// The device id a transcript belongs to (from its first identity entry).
pub fn transcript_device_id(entries: &[TranscriptEntry]) -> Result<String> {
    let first = entries
        .first()
        .ok_or_else(|| Error::InvalidPacket("transcript is empty".to_string()))?;
    if !first.is_inbound_identity() {
        return Err(Error::InvalidPacket(format!(
            "transcript must begin with an inbound identity packet, got {} {}",
            first.dir, first.packet_type
        )));
    }
    let identity = crate::protocol::types::Identity::from_packet(first.packet()?)?;
    Ok(identity.device_id)
}

/// Split a transcript into per-dial segments: a new segment begins at each
/// inbound identity packet (a redial re-runs the identity exchange).
pub fn segment_by_dial(entries: &[TranscriptEntry]) -> Vec<&[TranscriptEntry]> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (i, entry) in entries.iter().enumerate() {
        if i > start && entry.is_inbound_identity() {
            segments.push(&entries[start..i]);
            start = i;
        }
    }
    if start < entries.len() {
        segments.push(&entries[start..]);
    }
    segments
}

/// The peer (phone) end of one replayed dial: the test writes `in` packets
/// and reads the daemon's `out` packets back.
struct ReplayPeer {
    reader: BufReader<tokio::io::ReadHalf<TlsStream>>,
    writer: tokio::io::WriteHalf<TlsStream>,
}

impl ReplayPeer {
    async fn send(&mut self, packet: &Packet) -> Result<()> {
        let bytes = PacketSerializer::serialize(packet)?;
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|e| Error::ConnectionError(format!("replay peer write failed: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| Error::ConnectionError(format!("replay peer flush failed: {}", e)))
    }

    async fn recv(&mut self) -> Result<Packet> {
        let mut line = Vec::new();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.reader.read_until(b'\n', &mut line),
        )
        .await
        .map_err(|_| Error::ConnectionTimeout("replay peer read timed out".to_string()))?
        .map_err(|e| Error::ConnectionError(format!("replay peer read failed: {}", e)))?;
        if n == 0 {
            return Err(Error::ConnectionError(
                "replay peer link closed by daemon".to_string(),
            ));
        }
        PacketSerializer::deserialize(&line)
    }
}

/// What happened on one replayed dial.
pub struct DialOutcome {
    /// Device id the dial registered under.
    pub device_id: String,
    /// The generation `accept_incoming` assigned this dial.
    pub generation: u64,
    /// Packets the daemon received, in order (one per `in` entry).
    pub received: Vec<Packet>,
    /// Packets captured on the peer side, in order (one per `out` entry).
    pub captured_out: Vec<Packet>,
}

/// Replay a transcript against `cm` through real TCP loopback and
/// `accept_incoming` — the same path `listener.rs::handle_connection`
/// drives in production. `peer_certs` must hold an own-certificate whose CN
/// is the transcript's device id (the TLS server cert the daemon verifies).
/// Returns one outcome per dial, in dial order.
pub async fn replay_transcript(
    cm: &Arc<ConnectionManager>,
    listener: &Arc<TcpListener>,
    peer_certs: &Arc<CertificateManager>,
    entries: &[TranscriptEntry],
) -> Result<Vec<DialOutcome>> {
    let addr = listener
        .local_addr()
        .map_err(|e| Error::ConnectionError(format!("listener addr: {}", e)))?;
    let mut outcomes = Vec::new();

    for segment in segment_by_dial(entries) {
        let first = segment
            .first()
            .ok_or_else(|| Error::InvalidPacket("empty dial segment".to_string()))?;
        if !first.is_inbound_identity() {
            return Err(Error::InvalidPacket(format!(
                "a dial segment must begin with an inbound identity packet, got {} {}",
                first.dir, first.packet_type
            )));
        }

        // The daemon side: accept the dial exactly as the production
        // listener does.
        let accept_cm = cm.clone();
        let accept_listener = listener.clone();
        let accept = tokio::spawn(async move {
            let (stream, _) = accept_listener
                .accept()
                .await
                .map_err(|e| Error::ConnectionError(format!("replay accept failed: {}", e)))?;
            accept_cm.accept_incoming(stream).await
        });

        // The peer side: dial, send the (plaintext) pre-TLS identity from
        // the transcript, then serve TLS — Android's roles on its dial.
        let identity_bytes = PacketSerializer::serialize(&first.packet()?)?;
        let mut tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| Error::ConnectionError(format!("replay peer connect failed: {}", e)))?;
        tcp.write_all(&identity_bytes)
            .await
            .map_err(|e| Error::ConnectionError(format!("replay peer identity write: {}", e)))?;
        tcp.flush()
            .await
            .map_err(|e| Error::ConnectionError(format!("replay peer identity flush: {}", e)))?;
        let (tls_stream, _) =
            crate::protocol::connection::tls::tls_accept(peer_certs.clone(), None, tcp).await?;

        let accepted = accept
            .await
            .map_err(|e| Error::ConnectionError(format!("replay accept task: {}", e)))??;
        let (device_id, _remote_identity, generation) = accepted;

        let (read_half, write_half) = tokio::io::split(tls_stream);
        let mut peer = ReplayPeer {
            reader: BufReader::new(read_half),
            writer: write_half,
        };

        let mut received = Vec::new();
        let mut captured_out = Vec::new();
        for entry in segment {
            let packet = entry.packet()?;
            match entry.dir.as_str() {
                // The phone sent it: put it on the wire and verify the
                // daemon receives exactly that packet.
                "in" => {
                    peer.send(&packet).await?;
                    let got = cm.recv_packet_current(&device_id, generation).await?;
                    if got != packet {
                        return Err(Error::InvalidPacket(format!(
                            "replay mismatch: daemon received {:?}, transcript says {:?}",
                            got, packet
                        )));
                    }
                    received.push(got);
                }
                // The daemon sent it: drive the send and capture what
                // actually crosses the wire.
                "out" => {
                    cm.send_packet(&device_id, &packet).await?;
                    let got = peer.recv().await?;
                    if got != packet {
                        return Err(Error::InvalidPacket(format!(
                            "replay mismatch: peer captured {:?}, transcript says {:?}",
                            got, packet
                        )));
                    }
                    captured_out.push(got);
                }
                other => {
                    return Err(Error::InvalidPacket(format!(
                        "transcript dir must be \"in\" or \"out\", got {:?}",
                        other
                    )));
                }
            }
        }

        outcomes.push(DialOutcome {
            device_id,
            generation,
            received,
            captured_out,
        });
    }

    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    /// The redial-storm fixture: two same-cert dials, each identity +
    /// battery + clipboard, from the observed A15 behavior.
    const REDIAL_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/redial_replaces.jsonl"
    );

    fn fixture_cm() -> (Arc<ConnectionManager>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
        let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
        cert_manager.init().expect("Value expected to be present");
        let cm =
            Arc::new(ConnectionManager::new(cert_manager).expect("Value expected to be present"));
        cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Us");
        (cm, temp_dir)
    }

    /// A certificate manager holding the fixture peer's own-identity (its
    /// TLS server cert, CN = device id).
    fn fixture_peer_certs(device_id: &str) -> (Arc<CertificateManager>, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let cm = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
        cm.init().expect("Value expected to be present");
        cm.ensure_own_certificate(device_id, "Galaxy A15 5G")
            .expect("Value expected to be present");
        (cm, temp)
    }

    #[test]
    fn test_segment_by_dial_splits_on_inbound_identity() {
        let entries = read_transcript(Path::new(REDIAL_FIXTURE)).expect("fixture parses");
        let segments = segment_by_dial(&entries);
        assert_eq!(segments.len(), 2, "the redial fixture has two dials");
        assert!(
            segments
                .iter()
                .all(|s| s.first().is_some_and(|e| e.is_inbound_identity())),
            "every segment must begin with an inbound identity packet"
        );
    }

    #[tokio::test]
    async fn test_replay_redial_second_dial_replaces_link() {
        let entries = read_transcript(Path::new(REDIAL_FIXTURE)).expect("fixture parses");
        let device_id = transcript_device_id(&entries).expect("fixture has a device id");
        let (cm, _t) = fixture_cm();
        let (peer_certs, _pt) = fixture_peer_certs(&device_id);
        let listener = Arc::new(
            TcpListener::bind("127.0.0.1:0")
                .await
                .expect("Value expected to be present"),
        );

        let outcomes = replay_transcript(&cm, &listener, &peer_certs, &entries)
            .await
            .expect("replay must succeed");
        assert_eq!(outcomes.len(), 2, "two dials replayed");

        // The second same-cert dial REPLACES the link: new generation, and
        // the replacement is the live connection.
        let gen1 = outcomes[0].generation;
        let gen2 = outcomes[1].generation;
        assert!(gen2 > gen1, "the redial must take a new generation");
        assert!(cm.is_connected(&device_id).await);
        assert_eq!(
            cm.get_generation(&device_id).await,
            Some(gen2),
            "the second dial's link must be the live one"
        );

        // Both dials delivered their payloads through the real path, and
        // each captured our identity reply on the wire.
        for outcome in &outcomes {
            let types: Vec<&str> = outcome
                .received
                .iter()
                .map(|p| p.packet_type.as_str())
                .collect();
            assert_eq!(
                types,
                [
                    "kdeconnect.identity",
                    "kdeconnect.battery",
                    "kdeconnect.clipboard"
                ],
                "each dial must deliver identity + battery + clipboard"
            );
            assert_eq!(outcome.captured_out.len(), 1);
            assert!(outcome.captured_out[0].is_identity());
        }

        // Nothing from the first link is authoritative after the replace: a
        // read on the stale generation fails fast instead of consuming the
        // replacement's stream.
        let stale = cm.recv_packet_current(&device_id, gen1).await;
        let err = stale.expect_err("a stale generation must not read the live link");
        assert!(
            err.to_string().contains("Stale generation"),
            "unexpected error: {err}"
        );
    }
}

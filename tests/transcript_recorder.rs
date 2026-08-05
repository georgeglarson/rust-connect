//! End-to-end transcript recorder test: with `RUST_CONNECT_TRANSCRIPT_DIR`
//! set, packets crossing a real link are appended to `<dir>/<device_id>.jsonl`.
//! Lives in its own integration binary so the process-wide env var and the
//! recorder's OnceCell are not shared with other tests.

use std::sync::Arc;

use rust_connect::protocol::crypto::CertificateManager;
use rust_connect::protocol::transcript::TRANSCRIPT_DIR_ENV;
use rust_connect::protocol::{ConnectionManager, Packet};

#[tokio::test]
async fn test_transcript_records_both_directions_on_a_live_link() {
    let transcript_dir = tempfile::TempDir::new().unwrap();
    // Must happen before any packet flows: the recorder resolves the env
    // var once, on first use.
    std::env::set_var(TRANSCRIPT_DIR_ENV, transcript_dir.path());

    let cert_dir = tempfile::TempDir::new().unwrap();
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.path().to_path_buf()));
    cert_manager.init().unwrap();

    let server_cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());
    let client_cm = ConnectionManager::new(cert_manager).unwrap();
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = server_cm.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        server
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .unwrap();
    });

    client_cm
        .connect(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .unwrap();
    server_handle.await.unwrap();

    let packet = Packet::ping();
    client_cm
        .send_packet(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &packet)
        .await
        .unwrap();
    let received = server_cm
        .recv_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .unwrap();
    assert_eq!(received.packet_type, "kdeconnect.ping");

    // The client's transcript of the link holds the OUT entry, the server's
    // holds the IN entry — same packet, both directions recorded.
    let client_log = std::fs::read_to_string(
        transcript_dir
            .path()
            .join("serveraaaaaaaaaaaaaaaaaaaaaaaaaa.jsonl"),
    )
    .expect("client transcript must exist");
    let out: serde_json::Value = serde_json::from_str(client_log.trim_end()).unwrap();
    assert_eq!(out["dir"], "out");
    assert_eq!(out["type"], "kdeconnect.ping");
    assert!(out["ts"].is_u64());
    assert_eq!(out["body"]["body"], serde_json::json!({}));

    let server_log = std::fs::read_to_string(
        transcript_dir
            .path()
            .join("clientaaaaaaaaaaaaaaaaaaaaaaaaaa.jsonl"),
    )
    .expect("server transcript must exist");
    let inbound: serde_json::Value = serde_json::from_str(server_log.trim_end()).unwrap();
    assert_eq!(inbound["dir"], "in");
    assert_eq!(inbound["type"], "kdeconnect.ping");
}

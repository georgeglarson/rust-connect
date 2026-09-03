use super::*;
use std::sync::Arc;

use crate::protocol::crypto::CertificateManager;
use crate::protocol::types::{Identity, Packet};

fn init_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {});
}

fn setup() -> (ConnectionManager, tempfile::TempDir) {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let cm = ConnectionManager::new(cert_manager).expect("Value expected to be present");
    (cm, temp_dir)
}

#[tokio::test]
async fn test_connection_manager_creation() {
    let (cm, _temp) = setup();
    assert!(cm.connected_device_ids().await.is_empty());
}

#[tokio::test]
async fn test_send_to_nonexistent_device() {
    let (cm, _temp) = setup();
    let packet = Packet::ping();
    let result = cm.send_packet(&"nonexistent".to_string(), &packet).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_recv_from_nonexistent_device() {
    let (cm, _temp) = setup();
    let result = cm.recv_packet(&"nonexistent".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_disconnect_nonexistent_device() {
    let (cm, _temp) = setup();
    let result = cm.disconnect(&"nonexistent".to_string(), 0).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_is_connected() {
    let (cm, _temp) = setup();
    assert!(!cm.is_connected(&"test-device".to_string()).await);
}

#[tokio::test]
async fn test_get_connection_info_nonexistent() {
    let (cm, _temp) = setup();
    assert!(cm
        .get_connection_info(&"nonexistent".to_string())
        .await
        .is_none());
}

#[tokio::test]
async fn test_connect_and_communicate() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    let server_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");

    let (received_tx, received_rx) = tokio::sync::oneshot::channel();

    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server_cm
            .accept_test("client-deviceaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");

        let client_device_id = "client-deviceaaaaaaaaaaaaaaaaaaa".to_string();
        let packet = server_cm
            .recv_packet(&client_device_id)
            .await
            .expect("Value expected to be present");
        let _ = received_tx.send(packet.packet_type);
    });

    let server_device_id = "server-deviceaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&server_device_id, addr)
        .await
        .expect("Value expected to be present");

    let packet = Packet::ping();
    client_cm
        .send_packet(&server_device_id, &packet)
        .await
        .expect("Value expected to be present");

    let received_type = received_rx.await.expect("Value expected to be present");
    assert_eq!(received_type, "kdeconnect.ping");

    let gen = client_cm
        .get_generation(&server_device_id)
        .await
        .expect("Value expected to be present");
    client_cm
        .disconnect(&server_device_id, gen)
        .await
        .expect("Value expected to be present");
    assert!(!client_cm.is_connected(&server_device_id).await);

    let _ = server_handle.await;
}

/// A3 (2026-09-02 audit): `send_packet` bounds `write_all` with a short
/// timeout. tokio-rustls hands back partial counts under backpressure, so a
/// timed-out write leaves part of the packet queued and the rest never
/// written; the next packet on the same stream lands after the truncated
/// line and the peer drops both. A link whose framing is undefined must not
/// stay in the map: a send timeout tears it down.
#[tokio::test]
async fn test_send_timeout_tears_down_the_link() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    let server_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");

    // The peer completes TLS and then never reads: socket buffers fill,
    // the write backpressures, the send times out.
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server_cm
            .accept_test("client-deviceaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });

    let server_device_id = "server-deviceaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&server_device_id, addr)
        .await
        .expect("Value expected to be present");

    let big = Packet::new(
        "kdeconnect.test".to_string(),
        serde_json::json!({ "blob": "x".repeat(4 * 1024 * 1024) }),
    );
    let mut last = Ok(());
    for _ in 0..64 {
        last = client_cm.send_packet(&server_device_id, &big).await;
        if last.is_err() {
            break;
        }
    }
    let err = last.expect_err("a peer that never reads must time the send out");
    assert!(
        matches!(err, Error::ConnectionTimeout(_)),
        "expected a send timeout, got {err}"
    );
    assert!(
        !client_cm.is_connected(&server_device_id).await,
        "a timed-out send leaves the stream's framing undefined; the link must be torn down"
    );

    server_handle.abort();
}

/// B3 (2026-09-02 audit): `disconnect` held the connections write lock
/// across the dying socket's `shutdown().await`. On a stalled peer that is
/// bounded only by TCP_USER_TIMEOUT (30 s), and every `is_connected`,
/// `send_packet`, accept and connect for EVERY device waited behind it.
#[tokio::test]
async fn test_disconnect_of_a_stalled_peer_does_not_block_other_lookups() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let client_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    let server_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");

    // The peer completes TLS and then never reads.
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server_cm
            .accept_test("client-deviceaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });

    let server_device_id = "server-deviceaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&server_device_id, addr)
        .await
        .expect("Value expected to be present");
    let generation = client_cm
        .get_generation(&server_device_id)
        .await
        .expect("Value expected to be present");

    // Fill every buffer between us and the peer so the socket is stalled.
    // Write through the raw stream lock, not send_packet (which now tears
    // the link down on its own timeout).
    {
        let connections = client_cm.connections.read().await;
        let conn = connections
            .get(&server_device_id)
            .cloned()
            .expect("Value expected to be present");
        drop(connections);
        let mut w = conn.write_stream.lock().await;
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..64 {
            use tokio::io::AsyncWriteExt;
            if tokio::time::timeout(std::time::Duration::from_millis(50), w.write_all(&chunk))
                .await
                .is_err()
            {
                break;
            }
        }
    }

    let cm = client_cm.clone();
    let id = server_device_id.clone();
    let disconnecting = tokio::spawn(async move { cm.disconnect(&id, generation).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    let other = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client_cm.is_connected(&"some-other-device-aaaaaaaaaaaaaa".to_string()),
    )
    .await;
    assert!(
        other.is_ok() && started.elapsed() < std::time::Duration::from_millis(500),
        "a lookup for another device waited {:?} behind one stalled disconnect",
        started.elapsed()
    );

    disconnecting.abort();
    server_handle.abort();
}

/// D3 (2026-09-02 audit): the payload sender advertised whatever address
/// a UDP probe to 8.8.8.8 picked (the default route), which is wrong under
/// a VPN exit node. The link's own local address is the right source, so
/// the connection records it.
#[tokio::test]
async fn test_connection_records_its_local_address() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    let server_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server_cm
            .accept_test("client-deviceaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });

    let server_device_id = "server-deviceaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&server_device_id, addr)
        .await
        .expect("Value expected to be present");

    let local = client_cm
        .get_local_addr(&server_device_id)
        .await
        .expect("a live link must know its local address");
    assert!(local.ip().is_loopback(), "got {local}");
    server_handle.abort();
}

#[tokio::test]
async fn test_connect_disconnect_reconnect() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    let h1 = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
        server
            .recv_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
            .expect("Value expected to be present");
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
        server
            .recv_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
            .expect("Value expected to be present");
    });

    let device_id = "serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    client_cm
        .send_packet(&device_id, &Packet::ping())
        .await
        .expect("Value expected to be present");
    let gen1 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    client_cm
        .disconnect(&device_id, gen1)
        .await
        .expect("Value expected to be present");
    assert!(!client_cm.is_connected(&device_id).await);

    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    client_cm
        .send_packet(&device_id, &Packet::ping())
        .await
        .expect("Value expected to be present");
    assert!(client_cm.is_connected(&device_id).await);
    let gen2 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    client_cm
        .disconnect(&device_id, gen2)
        .await
        .expect("Value expected to be present");

    h1.await.expect("Value expected to be present");
}

#[tokio::test]
async fn test_connected_device_ids() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    tokio::spawn(async move {
        for _ in 0..3 {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            server
                .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
                .await
                .expect("Value expected to be present");
        }
    });

    client_cm
        .connect(&"s1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .expect("Value expected to be present");
    client_cm
        .connect(&"s2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .expect("Value expected to be present");
    client_cm
        .connect(&"s3aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .expect("Value expected to be present");

    let mut ids = client_cm.connected_device_ids().await;
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "s1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "s2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "s3aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ]
    );

    let gen = client_cm
        .get_generation(&"s2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .expect("Value expected to be present");
    client_cm
        .disconnect(&"s2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), gen)
        .await
        .expect("Value expected to be present");
    let mut ids = client_cm.connected_device_ids().await;
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "s1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "s3aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ]
    );
}

#[tokio::test]
async fn test_get_connection_info_returns_info() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
    });

    client_cm
        .connect(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .expect("Value expected to be present");
    let info = client_cm
        .get_connection_info(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await;
    assert!(info.is_some());
    assert_eq!(
        info.expect("Value expected to be present").device_id,
        "serveraaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
}

#[tokio::test]
async fn test_accept_incoming_tls_server_receives_identity() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    server_cm.set_device_identity("server-idaaaaaaaaaaaaaaaaaaaaaaa", "Server");
    cert_manager
        .ensure_certificate("server-idaaaaaaaaaaaaaaaaaaaaaaa", "Server")
        .expect("Value expected to be present");

    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    client_cm.set_device_identity("client-idaaaaaaaaaaaaaaaaaaaaaaa", "Client");
    cert_manager
        .ensure_certificate("client-idaaaaaaaaaaaaaaaaaaaaaaa", "Client")
        .expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server = server_cm.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server
            .accept_test("client-idaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
    });

    client_cm
        .connect(&"server-idaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .expect("Value expected to be present");
    server_handle.await.expect("Value expected to be present");

    let packet = Packet::ping();
    client_cm
        .send_packet(&"server-idaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &packet)
        .await
        .expect("Value expected to be present");

    let received = server_cm
        .recv_packet(&"client-idaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .expect("Value expected to be present");
    assert_eq!(received.packet_type, "kdeconnect.ping");

    assert!(
        server_cm
            .is_connected(&"client-idaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
    );
}

#[tokio::test]
async fn test_tls_handshake_failure_with_non_tls_server() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    cert_manager
        .ensure_certificate("client-idaaaaaaaaaaaaaaaaaaaaaaa", "Client")
        .expect("Value expected to be present");

    let cm = ConnectionManager::new(cert_manager).expect("Value expected to be present");
    cm.set_device_identity("client-idaaaaaaaaaaaaaaaaaaaaaaa", "Client");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        let _ = stream.write_all(b"NOT TLS\n").await;
    });

    let identity = Identity::new(
        "client-idaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        "Client".to_string(),
        crate::device::types::DeviceType::Desktop,
        vec![],
        vec![],
    );

    let result = cm.connect_to_device(&identity, addr, None).await;
    assert!(result.is_err(), "Should fail when server doesn't do TLS");
}

#[tokio::test]
async fn test_connection_closed_before_tls_handshake() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    cert_manager
        .ensure_certificate("client-idaaaaaaaaaaaaaaaaaaaaaaa", "Client")
        .expect("Value expected to be present");

    let cm = ConnectionManager::new(cert_manager).expect("Value expected to be present");
    cm.set_device_identity("client-idaaaaaaaaaaaaaaaaaaaaaaa", "Client");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        drop(stream);
    });

    let identity = Identity::new(
        "client-idaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        "Client".to_string(),
        crate::device::types::DeviceType::Desktop,
        vec![],
        vec![],
    );

    let result = cm.connect_to_device(&identity, addr, None).await;
    assert!(
        result.is_err(),
        "Should fail when connection is closed before TLS"
    );
}

#[tokio::test]
async fn test_generation_counter_increments_on_connect() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    let h = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            server
                .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
                .await
                .expect("Value expected to be present");
        }
    });

    let device_id = "serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    let gen1 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    assert_eq!(gen1, 1);

    client_cm
        .disconnect(&device_id, gen1)
        .await
        .expect("Value expected to be present");

    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    let gen2 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    assert!(
        gen2 > gen1,
        "Generation should increment on reconnect, got gen1={} gen2={}",
        gen1,
        gen2
    );

    client_cm
        .disconnect(&device_id, gen2)
        .await
        .expect("Value expected to be present");
    h.await.expect("Value expected to be present");
}

#[tokio::test]
async fn test_stale_disconnect_does_not_remove_current_connection() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    let h = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            server
                .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
                .await
                .expect("Value expected to be present");
        }
    });

    let device_id = "serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    let gen1 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");

    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    let gen2 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    assert!(
        gen2 > gen1,
        "Generation should increment, got gen1={} gen2={}",
        gen1,
        gen2
    );

    let stale_result = client_cm.disconnect(&device_id, gen1).await;
    assert!(stale_result.is_ok());
    assert!(
        client_cm.is_connected(&device_id).await,
        "Stale disconnect should not remove current connection"
    );

    client_cm
        .disconnect(&device_id, gen2)
        .await
        .expect("Value expected to be present");
    assert!(!client_cm.is_connected(&device_id).await);

    h.await.expect("Value expected to be present");
}

#[tokio::test]
async fn test_send_packet_after_disconnect_fails() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
    });

    let device_id = "serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    let gen = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    client_cm
        .disconnect(&device_id, gen)
        .await
        .expect("Value expected to be present");

    let result = client_cm.send_packet(&device_id, &Packet::ping()).await;
    assert!(result.is_err(), "Should fail to send after disconnect");
}

#[tokio::test]
async fn test_recv_packet_timeout() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        server
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .expect("Value expected to be present");
    });

    let device_id = "serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");

    let result = client_cm.recv_packet(&device_id).await;
    assert!(result.is_err(), "Should timeout when no packets are sent");

    let gen = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    client_cm
        .disconnect(&device_id, gen)
        .await
        .expect("Value expected to be present");
}

// MAJOR-2: end-to-end CN-mismatch rejection over real in-process TLS
// handshakes (inbound.rs / outbound.rs CN checks). The previous test only
// compared two string literals via extract_cn_from_der — a tautology that
// never touched the rejection code.

const IMPOSTOR_CERT_ID: &str = "impostor-device-aaaaaaaaaaaaaaaaaaaa";

#[tokio::test]
async fn test_certificate_cn_mismatch_rejects_connection() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    // The peer claims INBOUND_PEER_ID in its plaintext identity packet but
    // presents a certificate whose CN is a DIFFERENT device id.
    let (peer_certs, _pt) = peer_cert_manager(IMPOSTOR_CERT_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let cm1 = cm.clone();
    let l1 = listener.clone();
    let accept = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.expect("Value expected to be present");
        cm1.accept_incoming(stream).await
    });
    let _peer = spawn_inbound_peer(addr, peer_certs, INBOUND_PEER_ID).await;

    let err = accept
        .await
        .expect("Value expected to be present")
        .expect_err("a cert CN that differs from the identity deviceId must be rejected");
    assert!(
        err.to_string().contains("does not match"),
        "unexpected error: {err}"
    );
    assert!(
        !cm.is_connected(&INBOUND_PEER_ID.to_string()).await,
        "rejected connection must not be registered"
    );
}

#[tokio::test]
async fn test_certificate_cn_mismatch_rejects_outbound_connection() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    // The peer presents a cert with CN=IMPOSTOR_CERT_ID during the TLS
    // handshake, then claims INBOUND_PEER_ID in its encrypted identity.
    let (peer_certs, _pt) = peer_cert_manager(IMPOSTOR_CERT_ID);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let peer = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (tcp, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");

        // Read our plaintext identity.
        let mut reader = BufReader::new(tcp);
        let mut line = Vec::new();
        reader
            .read_until(b'\n', &mut line)
            .await
            .expect("Value expected to be present");

        // TLS client role (connect_to_device is the TLS server).
        let tcp = reader.into_inner();
        let (tls, _) = super::tls::tls_connect(peer_certs, INBOUND_OUR_ID, tcp)
            .await
            .expect("Value expected to be present");

        // Read our encrypted identity.
        let mut reader = BufReader::new(tls);
        let mut line = Vec::new();
        reader
            .read_until(b'\n', &mut line)
            .await
            .expect("Value expected to be present");

        // Send an encrypted identity claiming a device id that does NOT
        // match the cert CN.
        let identity = Identity::new(
            INBOUND_PEER_ID.to_string(),
            "Impostor".to_string(),
            crate::device::DeviceType::Phone,
            vec![],
            vec![],
        );
        let bytes = crate::protocol::packet::PacketSerializer::serialize(
            &identity
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .expect("Value expected to be present");
        // The far end rejects on the CN check and drops the link; the write
        // may therefore fail — that is fine and not what we assert on.
        let _ = reader.get_mut().write_all(&bytes).await;
        let _ = reader.get_mut().flush().await;
        // Give the far end a moment to read before this task's streams drop.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    });

    let our_identity = cm.get_identity().expect("Value expected to be present");
    let result = cm.connect_to_device(&our_identity, addr, None).await;
    let err = result
        .expect_err("a cert CN that differs from the encrypted-identity deviceId must be rejected");
    assert!(
        err.to_string().contains("does not match"),
        "unexpected error: {err}"
    );
    assert!(
        !cm.is_connected(&INBOUND_PEER_ID.to_string()).await,
        "rejected connection must not be registered"
    );

    peer.await.expect("Value expected to be present");
}

// Lane-3 #8/#10: the outbound path must compare the encrypted identity
// against the pre-TLS (UDP broadcast) identity it dialed on — same deviceId,
// same protocolVersion — mirroring the inbound P6 exchange check
// (LanLinkProvider.java:316-327).

/// Peer side for the outbound cross-check tests: accept one TCP connection,
/// read our plaintext identity, drive TLS as client, read our encrypted
/// identity, then send `encrypted_identity` as its own encrypted identity.
async fn spawn_outbound_tls_peer(
    listener: tokio::net::TcpListener,
    peer_certs: Arc<CertificateManager>,
    encrypted_identity: Identity,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (tcp, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");

        // Read our plaintext identity.
        let mut reader = BufReader::new(tcp);
        let mut line = Vec::new();
        reader
            .read_until(b'\n', &mut line)
            .await
            .expect("Value expected to be present");

        // TLS client role (connect_to_device is the TLS server).
        let tcp = reader.into_inner();
        let (tls, _) = super::tls::tls_connect(peer_certs, INBOUND_OUR_ID, tcp)
            .await
            .expect("Value expected to be present");

        // Read our encrypted identity.
        let mut reader = BufReader::new(tls);
        let mut line = Vec::new();
        reader
            .read_until(b'\n', &mut line)
            .await
            .expect("Value expected to be present");

        let bytes = crate::protocol::packet::PacketSerializer::serialize(
            &encrypted_identity
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .expect("Value expected to be present");
        // The far end may reject and drop the link; the write may therefore
        // fail — that is fine and not what we assert on.
        let _ = reader.get_mut().write_all(&bytes).await;
        let _ = reader.get_mut().flush().await;
        // Give the far end a moment to read before this task's streams drop.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    })
}

#[tokio::test]
async fn test_outbound_rejects_device_id_change_vs_broadcast_identity() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    // The peer's cert CN and encrypted identity agree with each other (the
    // CN check passes) — but the encrypted identity's deviceId differs from
    // the UDP broadcast identity we dialed on.
    let (peer_certs, _pt) = peer_cert_manager(IMPOSTOR_CERT_ID);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");
    let impostor_identity = Identity::new(
        IMPOSTOR_CERT_ID.to_string(),
        "Impostor".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    let peer = spawn_outbound_tls_peer(listener, peer_certs, impostor_identity).await;

    let our_identity = cm.get_identity().expect("Value expected to be present");
    let broadcast = Identity::new(
        INBOUND_PEER_ID.to_string(),
        "Peer".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    let result = cm
        .connect_to_device(&our_identity, addr, Some(&broadcast))
        .await;
    let err = result.expect_err(
        "an encrypted deviceId that differs from the broadcast identity must be rejected",
    );
    assert!(
        err.to_string().contains("does not match pre-TLS identity"),
        "unexpected error: {err}"
    );
    assert!(
        !cm.is_connected(&IMPOSTOR_CERT_ID.to_string()).await,
        "rejected connection must not be registered"
    );

    peer.await.expect("Value expected to be present");
}

#[tokio::test]
async fn test_outbound_rejects_protocol_version_change_vs_broadcast_identity() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    // Same deviceId as the broadcast identity, but a mid-handshake
    // protocolVersion change (LanLinkProvider.java:316-321 rejects this).
    let (peer_certs, _pt) = peer_cert_manager(INBOUND_PEER_ID);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");
    let mut downgraded = Identity::new(
        INBOUND_PEER_ID.to_string(),
        "Peer".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    downgraded.protocol_version = 7;
    let peer = spawn_outbound_tls_peer(listener, peer_certs, downgraded).await;

    let our_identity = cm.get_identity().expect("Value expected to be present");
    let broadcast = Identity::new(
        INBOUND_PEER_ID.to_string(),
        "Peer".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    let result = cm
        .connect_to_device(&our_identity, addr, Some(&broadcast))
        .await;
    let err = result.expect_err("a mid-handshake protocolVersion change must be rejected");
    assert!(
        err.to_string().contains("mid-handshake"),
        "unexpected error: {err}"
    );
    assert!(
        !cm.is_connected(&INBOUND_PEER_ID.to_string()).await,
        "rejected connection must not be registered"
    );

    peer.await.expect("Value expected to be present");
}

#[tokio::test]
async fn test_accept_incoming_rejects_own_device_id() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    // The peer claims our own device id. The guard fires before the TLS
    // handshake, so the peer only observes the link being dropped.
    let peer = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("Value expected to be present");
        let identity = Identity::new(
            INBOUND_OUR_ID.to_string(),
            "Reflected Self".to_string(),
            crate::device::DeviceType::Phone,
            vec![],
            vec![],
        );
        let bytes = crate::protocol::packet::PacketSerializer::serialize(
            &identity
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .expect("Value expected to be present");
        tcp.write_all(&bytes)
            .await
            .expect("Value expected to be present");
        tcp.flush().await.expect("Value expected to be present");
        let mut buf = [0u8; 16];
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), tcp.read(&mut buf))
            .await
            .expect("self-connection must be dropped promptly, not left hanging");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "self-connection must be closed without a TLS handshake, got {:?}",
            read
        );
    });

    let (stream, _) = listener
        .accept()
        .await
        .expect("Value expected to be present");
    let err = cm
        .accept_incoming(stream)
        .await
        .expect_err("a peer claiming our own device id must be rejected");
    assert!(
        err.to_string()
            .contains("Rejecting incoming connection from self"),
        "unexpected error: {err}"
    );
    assert!(
        !cm.is_connected(&INBOUND_OUR_ID.to_string()).await,
        "a rejected self-connection must not be registered"
    );

    peer.await.expect("Value expected to be present");
}

// Lane-3 #9: an identity packet carrying targetDeviceId /
// targetProtocolVersion that doesn't address US is dropped, pre-TLS
// (Android tcpPacketReceived, LanLinkProvider.java:169-178).

/// Peer side for the target-field tests: connect, send the given plaintext
/// identity, then expect the far end to drop the link without a TLS
/// handshake.
async fn spawn_plaintext_identity_peer(addr: std::net::SocketAddr, identity: Identity) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("Value expected to be present");
    let bytes = crate::protocol::packet::PacketSerializer::serialize(
        &identity
            .to_tcp_packet()
            .expect("Value expected to be present"),
    )
    .expect("Value expected to be present");
    tcp.write_all(&bytes)
        .await
        .expect("Value expected to be present");
    tcp.flush().await.expect("Value expected to be present");
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), tcp.read(&mut buf))
        .await
        .expect("a rejected connection must be dropped promptly, not left hanging");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "a rejected connection must be closed without a TLS handshake, got {:?}",
        read
    );
}

#[tokio::test]
async fn test_accept_incoming_rejects_target_device_id_not_us() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let mut identity = Identity::new(
        INBOUND_PEER_ID.to_string(),
        "Peer".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    identity.target_device_id = Some("some-other-device-aaaaaaaaaaaaaaaa".to_string());
    let peer = tokio::spawn(spawn_plaintext_identity_peer(addr, identity));

    let (stream, _) = listener
        .accept()
        .await
        .expect("Value expected to be present");
    let err = cm
        .accept_incoming(stream)
        .await
        .expect_err("a connection request addressed to another device must be rejected");
    assert!(
        err.to_string().contains("isn't us"),
        "unexpected error: {err}"
    );
    assert!(!cm.is_connected(&INBOUND_PEER_ID.to_string()).await);

    peer.await.expect("Value expected to be present");
}

/// Task 3.2 M1 live-harness finding (2026-08-14): kdeconnect-kde rewrites
/// our dashed-UUID deviceId to underscores (networkpacket.cpp:82-87 @
/// dcd6ded4) before echoing it as targetDeviceId (lanlinkprovider.cpp:371).
/// A string-exact target check rejects EVERY kdeconnectd-initiated
/// connection pre-TLS. The underscore-normalized echo of OUR id must pass.
#[tokio::test]
async fn test_accept_incoming_accepts_kde_normalized_target_device_id() {
    init_crypto();
    let (cm, _t) = setup();
    // INBOUND_OUR_ID ("server-self-aaaa...") already contains dashes; the
    // kde-echoed form replaces every non-[A-Za-z0-9_] char with '_'.
    let kde_echoed: String = INBOUND_OUR_ID
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    assert_ne!(kde_echoed, INBOUND_OUR_ID, "test requires a dashed own id");
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let mut identity = Identity::new(
        INBOUND_PEER_ID.to_string(),
        "Peer".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    identity.target_device_id = Some(kde_echoed);

    // Bespoke peer (not spawn_plaintext_identity_peer, which asserts the
    // link is dropped): send the identity, then read — the server passing
    // the target check proceeds to the TLS handshake as CLIENT, so the
    // peer must observe handshake bytes.
    let peer = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("Value expected to be present");
        let bytes = crate::protocol::packet::PacketSerializer::serialize(
            &identity
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .expect("Value expected to be present");
        tcp.write_all(&bytes)
            .await
            .expect("Value expected to be present");
        let mut buf = [0u8; 64];
        tokio::time::timeout(std::time::Duration::from_secs(5), tcp.read(&mut buf))
            .await
            .expect("the server must proceed to the TLS handshake, not drop us")
            .expect("Value expected to be present")
    });

    let (stream, _) = listener
        .accept()
        .await
        .expect("Value expected to be present");
    // The target check must PASS; the connection then fails later (the
    // peer can't complete TLS) with any error EXCEPT the "isn't us"
    // rejection.
    let err = cm
        .accept_incoming(stream)
        .await
        .expect_err("a plaintext-only peer still cannot complete TLS");
    assert!(
        !err.to_string().contains("isn't us"),
        "kde-normalized targetDeviceId must not be rejected: {err}"
    );

    let handshake_bytes = peer.await.expect("Value expected to be present");
    assert!(
        handshake_bytes > 0,
        "passing the target check must be followed by a TLS ClientHello"
    );
}

#[tokio::test]
async fn test_accept_incoming_rejects_target_protocol_version_not_ours() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let mut identity = Identity::new(
        INBOUND_PEER_ID.to_string(),
        "Peer".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    identity.target_protocol_version = Some(7);
    let peer = tokio::spawn(spawn_plaintext_identity_peer(addr, identity));

    let (stream, _) = listener
        .accept()
        .await
        .expect("Value expected to be present");
    let err = cm
        .accept_incoming(stream)
        .await
        .expect_err("a connection request for another protocol version must be rejected");
    assert!(
        err.to_string().contains("isn't ours"),
        "unexpected error: {err}"
    );
    assert!(!cm.is_connected(&INBOUND_PEER_ID.to_string()).await);

    peer.await.expect("Value expected to be present");
}

// F-3: the size cap is enforced DURING the read, not after a newline/EOF
// arrives (Android readLineBounded, LanLinkProvider.java:153,308).

#[tokio::test]
async fn test_bounded_read_over_cap_without_newline_errors_promptly() {
    // A peer that streams past the cap but never sends `\n` and never closes:
    // the read must fail on the byte count alone, not wait for the delimiter.
    let (mut tx, rx) = tokio::io::duplex(MAX_PACKET_SIZE + 4096);
    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        tx.write_all(&vec![b'a'; MAX_PACKET_SIZE + 1])
            .await
            .expect("Value expected to be present");
        // Hold the stream open: no newline, no EOF, ever.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });

    let mut reader = BufReader::new(rx);
    let mut buf = Vec::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_line_bounded(&mut reader, &mut buf, MAX_PACKET_SIZE),
    )
    .await
    .expect("over-cap stream must error promptly, not hang waiting for a newline");

    assert!(
        matches!(result, Err(Error::PacketTooLarge { .. })),
        "expected PacketTooLarge, got {:?}",
        result
    );
    writer.abort();
}

#[tokio::test]
async fn test_bounded_read_at_cap_with_newline_succeeds() {
    // Exactly MAX_PACKET_SIZE bytes including the delimiter is legal — the
    // cap rejects only what EXCEEDS it.
    let mut data = vec![b'a'; MAX_PACKET_SIZE - 1];
    data.push(b'\n');
    let mut slice: &[u8] = &data;
    let mut buf = Vec::new();
    let n = read_line_bounded(&mut slice, &mut buf, MAX_PACKET_SIZE)
        .await
        .expect("Value expected to be present");
    assert_eq!(n, MAX_PACKET_SIZE);
    assert_eq!(buf.len(), MAX_PACKET_SIZE);
}

#[tokio::test]
async fn test_bounded_read_one_byte_over_cap_with_newline_errors() {
    let mut data = vec![b'a'; MAX_PACKET_SIZE];
    data.push(b'\n');
    let mut slice: &[u8] = &data;
    let mut buf = Vec::new();
    let result = read_line_bounded(&mut slice, &mut buf, MAX_PACKET_SIZE).await;
    assert!(
        matches!(result, Err(Error::PacketTooLarge { .. })),
        "expected PacketTooLarge, got {:?}",
        result
    );
}

#[tokio::test]
async fn test_bounded_read_matches_read_until_semantics() {
    // Normal line: delimiter included, byte count returned.
    let mut slice: &[u8] = b"hello\n";
    let mut buf = Vec::new();
    let n = read_line_bounded(&mut slice, &mut buf, MAX_PACKET_SIZE)
        .await
        .expect("Value expected to be present");
    assert_eq!(n, 6);
    assert_eq!(&buf, b"hello\n");

    // EOF with nothing read returns 0 (the "connection closed" signal).
    let n = read_line_bounded(&mut slice, &mut buf, MAX_PACKET_SIZE)
        .await
        .expect("Value expected to be present");
    assert_eq!(n, 0);

    // EOF after a partial line returns the partial bytes, like read_until.
    let mut slice: &[u8] = b"partial";
    let mut buf = Vec::new();
    let n = read_line_bounded(&mut slice, &mut buf, MAX_PACKET_SIZE)
        .await
        .expect("Value expected to be present");
    assert_eq!(n, 7);
    assert_eq!(&buf, b"partial");
}

// F-4: Android addOrUpdateLink semantics for duplicate inbound connections
// (LanLinkProvider.java:364-374) — same cert resets the link, a different
// cert aborts the replacement.

const INBOUND_OUR_ID: &str = "server-self-aaaaaaaaaaaaaaaaaaaa";
const INBOUND_PEER_ID: &str = "peer-device-aaaaaaaaaaaaaaaaaaaaaa";

/// A live inbound link: the daemon end (cm + generation) and the peer's raw
/// TLS stream, for tests that write arbitrary bytes onto the wire.
async fn setup_inbound_link() -> (
    Arc<ConnectionManager>,
    super::TlsStream,
    u64,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    init_crypto();
    let (cm, t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs, pt) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let accept_cm = cm.clone();
    let accept_listener = listener.clone();
    let accept = tokio::spawn(async move {
        let (stream, _) = accept_listener
            .accept()
            .await
            .expect("Value expected to be present");
        accept_cm.accept_incoming(stream).await
    });
    let peer = spawn_inbound_peer(addr, peer_certs, INBOUND_PEER_ID).await;
    let (_, _, generation) = accept
        .await
        .expect("Value expected to be present")
        .expect("dial must succeed");
    (cm, peer, generation, t, pt)
}

/// A certificate manager holding a full own-identity for `id` (the peer's
/// side of a test link).
fn peer_cert_manager(id: &str) -> (Arc<CertificateManager>, tempfile::TempDir) {
    let temp = tempfile::TempDir::new().expect("Value expected to be present");
    let cm = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
    cm.init().expect("Value expected to be present");
    cm.ensure_own_certificate(id, "Peer")
        .expect("Value expected to be present");
    (cm, temp)
}

/// A peer that connects over TCP, sends its plaintext identity packet, then
/// serves TLS (the inbound path: `accept_incoming` is the TLS client). The
/// presented cert is `peer_certs`' own certificate — its CN may differ from
/// `identity_id`, which is how the CN-mismatch tests drive the rejection.
async fn spawn_inbound_peer(
    addr: std::net::SocketAddr,
    peer_certs: Arc<CertificateManager>,
    identity_id: &str,
) -> super::TlsStream {
    use tokio::io::AsyncWriteExt;
    let mut tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("Value expected to be present");
    let identity = Identity::new(
        identity_id.to_string(),
        "Peer".to_string(),
        crate::device::DeviceType::Phone,
        vec![],
        vec![],
    );
    let bytes = crate::protocol::packet::PacketSerializer::serialize(
        &identity
            .to_tcp_packet()
            .expect("Value expected to be present"),
    )
    .expect("Value expected to be present");
    tcp.write_all(&bytes)
        .await
        .expect("Value expected to be present");
    tcp.flush().await.expect("Value expected to be present");
    super::tls::tls_accept(peer_certs, None, tcp)
        .await
        .expect("Value expected to be present")
        .0
}

/// Drive one read on a peer-closed link so the connection is OBSERVED dead.
/// Replacement no longer requires it — a same-cert redial replaces a healthy
/// link too (Android LanLink.reset) — but tests whose scenario is a dead
/// link still mark it, so the flag reflects reality.
async fn observe_link_dead(cm: &ConnectionManager, device_id: &str, generation: u64) {
    assert!(
        cm.recv_packet_current(&device_id.to_string(), generation)
            .await
            .is_err(),
        "read on a peer-closed link must fail"
    );
}

#[tokio::test]
async fn test_inbound_same_cert_duplicate_replaces_when_healthy() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs, _pt) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let cm1 = cm.clone();
    let l1 = listener.clone();
    let accept1 = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.expect("Value expected to be present");
        cm1.accept_incoming(stream).await
    });
    let mut peer1 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen1) = accept1
        .await
        .expect("Value expected to be present")
        .expect("first accept must succeed");
    assert!(cm.is_connected(&INBOUND_PEER_ID.to_string()).await);

    // Second inbound connection from the same device, same certificate,
    // while the first link is HEALTHY: Android LanLink.reset semantics —
    // the NEW socket is adopted and the old link evicted, always.
    let cm2 = cm.clone();
    let l2 = listener.clone();
    let accept2 = tokio::spawn(async move {
        let (stream, _) = l2.accept().await.expect("Value expected to be present");
        cm2.accept_incoming(stream).await
    });
    let mut peer2 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen2) = accept2
        .await
        .expect("Value expected to be present")
        .expect("duplicate accept must not error");

    assert!(gen2 > gen1, "the replacement must take a new generation");
    assert!(cm.is_connected(&INBOUND_PEER_ID.to_string()).await);
    assert_eq!(
        cm.get_generation(&INBOUND_PEER_ID.to_string()).await,
        Some(gen2),
        "the replacement connection must be the live one"
    );

    // The evicted link is torn down by us; the new link stays open.
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 16];
    let old_read = tokio::time::timeout(std::time::Duration::from_secs(5), peer1.read(&mut buf))
        .await
        .expect("the evicted link must close promptly");
    assert!(
        matches!(old_read, Ok(0) | Err(_)),
        "the evicted link must be torn down, got {:?}",
        old_read
    );
    let new_read =
        tokio::time::timeout(std::time::Duration::from_millis(300), peer2.read(&mut buf)).await;
    assert!(
        new_read.is_err(),
        "the replacement link must NOT be torn down (timeout = still open), got {:?}",
        new_read
    );
}

#[tokio::test]
async fn test_inbound_rapid_same_cert_redial_storm() {
    // Hostile-peer scenario: the peer redials N times in rapid succession
    // with no pause between dials (the test-phone redial storm, scripted). Every
    // dial must REPLACE: generations strictly increase, exactly one live
    // link at the end, and stale-generation reads on the evicted links fail
    // fast instead of consuming the live stream.
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs, _pt) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    const DIALS: usize = 5;
    let mut peers = Vec::new();
    let mut generations = Vec::new();
    for _ in 0..DIALS {
        let accept_cm = cm.clone();
        let accept_listener = listener.clone();
        let accept = tokio::spawn(async move {
            let (stream, _) = accept_listener
                .accept()
                .await
                .expect("Value expected to be present");
            accept_cm.accept_incoming(stream).await
        });
        let peer = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
        let (_, _, generation) = accept
            .await
            .expect("Value expected to be present")
            .expect("storm dial must not error");
        peers.push(peer);
        generations.push(generation);
    }

    for window in generations.windows(2) {
        assert!(
            window[1] > window[0],
            "generations must strictly increase across the storm: {generations:?}"
        );
    }

    // Exactly one live link — the last dial's.
    let last_gen = *generations.last().expect("Value expected to be present");
    assert!(cm.is_connected(&INBOUND_PEER_ID.to_string()).await);
    assert_eq!(
        cm.get_generation(&INBOUND_PEER_ID.to_string()).await,
        Some(last_gen),
        "the last storm dial must own the link"
    );

    // Stale-generation reads on every evicted link fail fast.
    for generation in &generations[..DIALS - 1] {
        let stale = cm
            .recv_packet_current(&INBOUND_PEER_ID.to_string(), *generation)
            .await;
        let err = stale.expect_err("an evicted generation must not read the live link");
        assert!(
            err.to_string().contains("Stale generation"),
            "unexpected error: {err}"
        );
    }

    // Every evicted peer socket was torn down by us; the live one carries
    // traffic.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = [0u8; 16];
    for peer in &mut peers[..DIALS - 1] {
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), peer.read(&mut buf))
            .await
            .expect("an evicted link must close promptly");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "an evicted link must be torn down, got {:?}",
            read
        );
    }
    let ping = crate::protocol::packet::PacketSerializer::serialize(&Packet::ping())
        .expect("Value expected to be present");
    let live = peers.last_mut().expect("Value expected to be present");
    live.write_all(&ping)
        .await
        .expect("Value expected to be present");
    live.flush().await.expect("Value expected to be present");
    let packet = cm
        .recv_packet_current(&INBOUND_PEER_ID.to_string(), last_gen)
        .await
        .expect("the surviving link must carry traffic after the storm");
    assert_eq!(packet.packet_type, "kdeconnect.ping");
}

#[tokio::test]
async fn test_inbound_same_cert_replacement_when_old_link_dead() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs, _pt) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let cm1 = cm.clone();
    let l1 = listener.clone();
    let accept1 = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.expect("Value expected to be present");
        cm1.accept_incoming(stream).await
    });
    let peer1 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen1) = accept1
        .await
        .expect("Value expected to be present")
        .expect("first accept must succeed");
    assert!(cm.is_connected(&INBOUND_PEER_ID.to_string()).await);

    // The old link dies (peer closed): one read observes the EOF and marks
    // it dead. The redial replaces either way (F-4); this exercises the
    // dead-link case specifically.
    drop(peer1);
    observe_link_dead(&cm, INBOUND_PEER_ID, gen1).await;

    let cm2 = cm.clone();
    let l2 = listener.clone();
    let accept2 = tokio::spawn(async move {
        let (stream, _) = l2.accept().await.expect("Value expected to be present");
        cm2.accept_incoming(stream).await
    });
    let _peer2 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen2) = accept2
        .await
        .expect("Value expected to be present")
        .expect("same-cert replacement must be accepted");

    assert!(gen2 > gen1, "replacement must take a new generation");
    assert!(cm.is_connected(&INBOUND_PEER_ID.to_string()).await);
    assert_eq!(
        cm.get_generation(&INBOUND_PEER_ID.to_string()).await,
        Some(gen2),
        "the replacement connection must be the live one"
    );
}

#[tokio::test]
async fn test_inbound_different_cert_replacement_rejected() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs_a, _pa) = peer_cert_manager(INBOUND_PEER_ID);
    // Same CN (device id), different key — Android's "certificate doesn't
    // match, aborting" case.
    let (peer_certs_b, _pb) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let cm1 = cm.clone();
    let l1 = listener.clone();
    let accept1 = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.expect("Value expected to be present");
        cm1.accept_incoming(stream).await
    });
    let mut peer1 = spawn_inbound_peer(addr, peer_certs_a.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen1) = accept1
        .await
        .expect("Value expected to be present")
        .expect("first accept must succeed");

    let cm2 = cm.clone();
    let l2 = listener.clone();
    let accept2 = tokio::spawn(async move {
        let (stream, _) = l2.accept().await.expect("Value expected to be present");
        cm2.accept_incoming(stream).await
    });
    let mut peer2 = spawn_inbound_peer(addr, peer_certs_b.clone(), INBOUND_PEER_ID).await;
    let err = accept2
        .await
        .expect("Value expected to be present")
        .expect_err("a certificate change must reject the replacement");
    assert!(
        err.to_string().contains("different certificate"),
        "unexpected error: {err}"
    );

    // The original connection is untouched.
    assert!(cm.is_connected(&INBOUND_PEER_ID.to_string()).await);
    assert_eq!(
        cm.get_generation(&INBOUND_PEER_ID.to_string()).await,
        Some(gen1),
        "rejected replacement must not disturb the existing connection"
    );

    // ... and still fully functional: traffic on the original link flows
    // exactly as if the hostile redial never happened.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ping = crate::protocol::packet::PacketSerializer::serialize(&Packet::ping())
        .expect("Value expected to be present");
    peer1
        .write_all(&ping)
        .await
        .expect("Value expected to be present");
    peer1.flush().await.expect("Value expected to be present");
    let packet = cm
        .recv_packet_current(&INBOUND_PEER_ID.to_string(), gen1)
        .await
        .expect("the untouched link must still carry traffic");
    assert_eq!(packet.packet_type, "kdeconnect.ping");

    // The rejected socket is closed, not adopted.
    let mut buf = [0u8; 16];
    let rejected_read =
        tokio::time::timeout(std::time::Duration::from_secs(5), peer2.read(&mut buf))
            .await
            .expect("the rejected socket must close promptly");
    assert!(
        matches!(rejected_read, Ok(0) | Err(_)),
        "the rejected socket must be closed, got {:?}",
        rejected_read
    );
}

// Generation-aware cleanup for
// the replacement path. The stale loop's exit arms used to remove the
// cancel token and mark the device Disconnected unconditionally — both
// clobber the live replacement when the evicted loop runs late.

#[tokio::test]
async fn test_stale_generation_recv_cannot_read_replacement_stream() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs, _pt) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let cm1 = cm.clone();
    let l1 = listener.clone();
    let accept1 = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.expect("Value expected to be present");
        cm1.accept_incoming(stream).await
    });
    let _peer1 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen1) = accept1
        .await
        .expect("Value expected to be present")
        .expect("first accept must succeed");

    // Same-cert redial: F-4 replacement path (the old link is marked
    // observed-dead first, though replacement no longer requires it).
    drop(_peer1);
    observe_link_dead(&cm, INBOUND_PEER_ID, gen1).await;

    let cm2 = cm.clone();
    let l2 = listener.clone();
    let accept2 = tokio::spawn(async move {
        let (stream, _) = l2.accept().await.expect("Value expected to be present");
        cm2.accept_incoming(stream).await
    });
    let mut peer2 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen2) = accept2
        .await
        .expect("Value expected to be present")
        .expect("same-cert replacement must be accepted");

    assert!(
        cm.is_current_generation(&INBOUND_PEER_ID.to_string(), gen2)
            .await
    );
    assert!(
        !cm.is_current_generation(&INBOUND_PEER_ID.to_string(), gen1)
            .await
    );

    // The replacement sends a packet; a STALE task's read must fail fast
    // instead of consuming it off the new stream. Before the
    // generation-scoped read this returned the replacement's packet — two
    // readers interleaved on one TLS stream.
    use tokio::io::AsyncWriteExt;
    let bytes = crate::protocol::packet::PacketSerializer::serialize(&Packet::ping())
        .expect("Value expected to be present");
    peer2
        .write_all(&bytes)
        .await
        .expect("Value expected to be present");
    peer2.flush().await.expect("Value expected to be present");

    let stale = cm
        .recv_packet_current(&INBOUND_PEER_ID.to_string(), gen1)
        .await;
    let err = stale.expect_err("a stale generation must not read the replacement's stream");
    assert!(
        err.to_string().contains("Stale generation"),
        "unexpected error: {err}"
    );

    // The live generation reads the packet normally.
    let packet = cm
        .recv_packet_current(&INBOUND_PEER_ID.to_string(), gen2)
        .await
        .expect("the live generation must read the replacement's stream");
    assert_eq!(packet.packet_type, "kdeconnect.ping");
}

#[tokio::test]
async fn test_stale_loop_cannot_remove_replacement_cancel_token() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs, _pt) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let cm1 = cm.clone();
    let l1 = listener.clone();
    let accept1 = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.expect("Value expected to be present");
        cm1.accept_incoming(stream).await
    });
    let _peer1 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen1) = accept1
        .await
        .expect("Value expected to be present")
        .expect("first accept must succeed");

    // The old link is marked observed-dead before the redial replaces it,
    // though replacement no longer requires it.
    drop(_peer1);
    observe_link_dead(&cm, INBOUND_PEER_ID, gen1).await;

    let cm2 = cm.clone();
    let l2 = listener.clone();
    let accept2 = tokio::spawn(async move {
        let (stream, _) = l2.accept().await.expect("Value expected to be present");
        cm2.accept_incoming(stream).await
    });
    let _peer2 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen2) = accept2
        .await
        .expect("Value expected to be present")
        .expect("same-cert replacement must be accepted");

    // The replacement's listener registered its cancel token.
    let device_id = INBOUND_PEER_ID.to_string();
    let new_token = tokio_util::sync::CancellationToken::new();
    cm.register_cancel_token(&device_id, new_token.clone())
        .await;

    // The evicted loop's cancel arm runs LATE — after the replacement
    // registered. It must not strip the replacement's token (before the
    // generation check it removed the map entry unconditionally).
    assert!(
        !cm.remove_cancel_token_if_current(&device_id, gen1).await,
        "stale generation must report not-current and remove nothing"
    );
    cm.cancel_loop(&device_id).await;
    assert!(
        new_token.is_cancelled(),
        "the replacement's token must still be registered and cancellable"
    );

    // The live generation's cleanup does remove it.
    let another = tokio_util::sync::CancellationToken::new();
    cm.register_cancel_token(&device_id, another.clone()).await;
    assert!(
        cm.remove_cancel_token_if_current(&device_id, gen2).await,
        "current generation must report current"
    );
    cm.cancel_loop(&device_id).await;
    assert!(
        !another.is_cancelled(),
        "a removed token must be gone from the map"
    );
}

#[tokio::test]
async fn test_disconnect_reports_whether_generation_owns_link() {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");
    let client_cm =
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");

    let server = server_cm.clone();
    let h = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            server
                .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
                .await
                .expect("Value expected to be present");
        }
    });

    let device_id = "serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    let gen1 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");
    client_cm
        .connect(&device_id, addr)
        .await
        .expect("Value expected to be present");
    let gen2 = client_cm
        .get_generation(&device_id)
        .await
        .expect("Value expected to be present");

    // Stale generation: the replacement owns the link — report false, and
    // the loop's caller must skip lifecycle/token cleanup.
    let owned = client_cm
        .disconnect(&device_id, gen1)
        .await
        .expect("Value expected to be present");
    assert!(!owned, "stale generation must not own the link");
    assert!(client_cm.is_connected(&device_id).await);

    // Current generation: torn down, report true.
    let owned = client_cm
        .disconnect(&device_id, gen2)
        .await
        .expect("Value expected to be present");
    assert!(owned, "current generation must own the link");
    assert!(!client_cm.is_connected(&device_id).await);

    // Already-empty slot: nothing newer holds it, still report true.
    let owned = client_cm
        .disconnect(&device_id, gen2)
        .await
        .expect("Value expected to be present");
    assert!(owned, "an empty slot has no newer owner");

    h.await.expect("Value expected to be present");
}

#[tokio::test]
async fn test_stale_generation_cannot_register_cancel_token() {
    init_crypto();
    let (cm, _t) = setup();
    cm.set_device_identity(INBOUND_OUR_ID, "Us");
    let cm = Arc::new(cm);
    let (peer_certs, _pt) = peer_cert_manager(INBOUND_PEER_ID);

    let listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present"),
    );
    let addr = listener.local_addr().expect("Value expected to be present");

    let cm1 = cm.clone();
    let l1 = listener.clone();
    let accept1 = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.expect("Value expected to be present");
        cm1.accept_incoming(stream).await
    });
    let _peer1 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen1) = accept1
        .await
        .expect("Value expected to be present")
        .expect("first accept must succeed");

    // The old link is marked observed-dead before the redial replaces it,
    // though replacement no longer requires it.
    drop(_peer1);
    observe_link_dead(&cm, INBOUND_PEER_ID, gen1).await;

    let cm2 = cm.clone();
    let l2 = listener.clone();
    let accept2 = tokio::spawn(async move {
        let (stream, _) = l2.accept().await.expect("Value expected to be present");
        cm2.accept_incoming(stream).await
    });
    let _peer2 = spawn_inbound_peer(addr, peer_certs.clone(), INBOUND_PEER_ID).await;
    let (_, _, gen2) = accept2
        .await
        .expect("Value expected to be present")
        .expect("same-cert replacement must be accepted");

    let device_id = INBOUND_PEER_ID.to_string();

    // A stale handler (redial landed before it registered) must NOT
    // overwrite the replacement's token slot: registration refused,
    // nothing inserted (before the atomic check-and-insert it registered
    // unconditionally).
    let stale_token = tokio_util::sync::CancellationToken::new();
    assert!(
        !cm.register_cancel_token_if_current(&device_id, stale_token.clone(), gen1)
            .await,
        "stale generation must not register"
    );
    cm.cancel_loop(&device_id).await;
    assert!(
        !stale_token.is_cancelled(),
        "a refused registration must not be in the token map"
    );

    // The live generation registers and is cancellable.
    let live_token = tokio_util::sync::CancellationToken::new();
    assert!(
        cm.register_cancel_token_if_current(&device_id, live_token.clone(), gen2)
            .await,
        "current generation must register"
    );
    cm.cancel_loop(&device_id).await;
    assert!(live_token.is_cancelled());
}

// Parity gaps (docs/parity-checklist.md, gaps 2 and 4): the steady-state
// read must tolerate packets up to 32 MiB, skip oversized lines, and skip
// blank lines — the references do all three (landevicelink.cpp:19,98-101,
// LanLink.java:46,85-91) without killing the link.

/// Gap 2, first half: a steady-state packet between 512 KiB and 32 MiB is
/// received intact — the 512 KiB cap applies to the pre-auth identity read
/// only.
#[tokio::test]
async fn test_steady_state_packet_up_to_32mib_is_received() {
    let (cm, mut peer, generation, _t, _pt) = setup_inbound_link().await;

    use tokio::io::AsyncWriteExt;
    let big = "x".repeat(600 * 1024);
    let packet = Packet::new(
        "kdeconnect.clipboard".to_string(),
        serde_json::json!({"content": big}),
    );
    let bytes = crate::protocol::packet::PacketSerializer::serialize(&packet)
        .expect("Value expected to be present");
    assert!(
        bytes.len() > 524_288,
        "the test packet must exceed the identity-read cap"
    );
    // The write runs concurrently with the read: the daemon only reads once
    // recv is called, and a write larger than the kernel buffers would
    // otherwise stall forever.
    let writer = tokio::spawn(async move {
        peer.write_all(&bytes)
            .await
            .expect("Value expected to be present");
        peer.flush().await.expect("Value expected to be present");
    });

    let got = cm
        .recv_packet_current(&INBOUND_PEER_ID.to_string(), generation)
        .await
        .expect("a steady-state packet under 32 MiB must be received");
    assert_eq!(got.packet_type, "kdeconnect.clipboard");
    assert_eq!(
        got.body
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::len),
        Some(600 * 1024)
    );
    writer.await.expect("Value expected to be present");
}

/// Gap 2, second half: a line OVER 32 MiB is consumed and discarded — the
/// link survives and the NEXT packet is delivered (references: skip-and-
/// continue, not disconnect).
#[tokio::test]
async fn test_oversize_steady_state_line_is_skipped_link_survives() {
    let (cm, mut peer, generation, _t, _pt) = setup_inbound_link().await;

    use tokio::io::AsyncWriteExt;
    let mut junk = vec![b'a'; 32 * 1024 * 1024 + 1];
    junk.push(b'\n');
    let ping = crate::protocol::packet::PacketSerializer::serialize(&Packet::ping())
        .expect("Value expected to be present");
    // Concurrent with the read — see the note on the 600 KiB test.
    let writer = tokio::spawn(async move {
        peer.write_all(&junk)
            .await
            .expect("Value expected to be present");
        peer.write_all(&ping)
            .await
            .expect("Value expected to be present");
        peer.flush().await.expect("Value expected to be present");
    });

    let got = cm
        .recv_packet_current(&INBOUND_PEER_ID.to_string(), generation)
        .await
        .expect("an oversize line must be skipped, not kill the link");
    assert_eq!(got.packet_type, "kdeconnect.ping");
    assert!(
        cm.is_connected(&INBOUND_PEER_ID.to_string()).await,
        "the link must survive an oversize line"
    );
    writer.await.expect("Value expected to be present");
}

/// Gap 4: blank lines in the stream are skipped, not fatal.
#[tokio::test]
async fn test_blank_lines_are_skipped() {
    let (cm, mut peer, generation, _t, _pt) = setup_inbound_link().await;

    use tokio::io::AsyncWriteExt;
    peer.write_all(b"\n\r\n   \n")
        .await
        .expect("Value expected to be present");
    let ping = crate::protocol::packet::PacketSerializer::serialize(&Packet::ping())
        .expect("Value expected to be present");
    peer.write_all(&ping)
        .await
        .expect("Value expected to be present");
    peer.flush().await.expect("Value expected to be present");

    let got = cm
        .recv_packet_current(&INBOUND_PEER_ID.to_string(), generation)
        .await
        .expect("blank lines must be skipped, not kill the link");
    assert_eq!(got.packet_type, "kdeconnect.ping");
}

// Gap D (parity-checklist.md § Lifecycle, vk #998 Task 2.3): send-side
// capability gating. `record_peer_capabilities` is `pub(crate)`, reachable
// directly from this module (a descendant of `connection`), so these tests
// drive the gate's logic directly rather than through a full identity
// exchange — `test_capability_gating_wired_from_real_identity_exchange`
// below covers that the PRODUCTION wiring (accept_incoming /
// connect_to_device) actually populates the map from a real identity.

/// A raw connected pair (no identity exchange — `client_cm.connect` /
/// `server_cm.accept_test`), for driving `send_packet`'s gate directly via
/// `record_peer_capabilities` without needing a full TLS + identity dance.
async fn setup_gated_pair() -> (
    Arc<ConnectionManager>,
    Arc<ConnectionManager>,
    String,
    tempfile::TempDir,
) {
    init_crypto();
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let client_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    let server_cm = Arc::new(
        ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present"),
    );
    server_cm.set_device_identity("gate-server-aaaaaaaaaaaaaaaaaaaa", "Server");

    let peer_device_id = "gate-peer-device-aaaaaaaaaaaaaaaa".to_string();
    let accept_cm = server_cm.clone();
    let accept_id = peer_device_id.clone();
    let accept = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        accept_cm
            .accept_test(accept_id, stream)
            .await
            .expect("Value expected to be present")
    });

    client_cm
        .connect(&peer_device_id, addr)
        .await
        .expect("Value expected to be present");
    accept.await.expect("Value expected to be present");

    (client_cm, server_cm, peer_device_id, temp_dir)
}

#[tokio::test]
async fn test_send_packet_refuses_unsupported_capability() {
    let (client_cm, _server_cm, peer_id, _t) = setup_gated_pair().await;

    client_cm
        .record_peer_capabilities(
            &peer_id,
            &["kdeconnect.ping".to_string()],
            &["kdeconnect.ping".to_string()],
        )
        .await;

    let packet = Packet::new(
        "kdeconnect.mousepad.request".to_string(),
        serde_json::json!({}),
    );
    let err = client_cm
        .send_packet(&peer_id, &packet)
        .await
        .expect_err("a type the peer never advertised must be refused");
    assert_eq!(err.code().http_status(), 400, "must reach the API as a 4xx");
    match err {
        crate::utils::errors::Error::CapabilityNotSupported {
            device_id,
            packet_type,
        } => {
            assert_eq!(device_id, peer_id);
            assert_eq!(packet_type, "kdeconnect.mousepad.request");
        }
        other => panic!("expected CapabilityNotSupported, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_send_packet_allows_advertised_capability() {
    let (client_cm, server_cm, peer_id, _t) = setup_gated_pair().await;

    client_cm
        .record_peer_capabilities(
            &peer_id,
            &["kdeconnect.ping".to_string()],
            &["kdeconnect.ping".to_string()],
        )
        .await;

    let packet = Packet::ping();
    client_cm
        .send_packet(&peer_id, &packet)
        .await
        .expect("an advertised type must send");

    let received = server_cm
        .recv_packet(&peer_id)
        .await
        .expect("Value expected to be present");
    assert_eq!(received.packet_type, "kdeconnect.ping");
}

#[tokio::test]
async fn test_send_packet_exempts_identity_and_pair() {
    let (client_cm, server_cm, peer_id, _t) = setup_gated_pair().await;

    // The peer's advertised caps carry NEITHER identity NOR pair — real
    // peers never advertise those as plugin capabilities either, since
    // they're protocol packets, not plugin packets.
    client_cm
        .record_peer_capabilities(
            &peer_id,
            &["kdeconnect.ping".to_string()],
            &["kdeconnect.ping".to_string()],
        )
        .await;

    let identity = Identity::new(
        "us-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        "Us".to_string(),
        crate::device::types::DeviceType::Desktop,
        vec![],
        vec![],
    );
    client_cm
        .send_packet(&peer_id, &identity.to_packet().expect("to_packet"))
        .await
        .expect("kdeconnect.identity must be exempt from gating");
    client_cm
        .send_packet(&peer_id, &Packet::pair_request())
        .await
        .expect("kdeconnect.pair must be exempt from gating");

    // Drain both off the wire so the test doesn't depend on socket buffer
    // capacity happening to be large enough to swallow two small packets.
    let first = server_cm
        .recv_packet(&peer_id)
        .await
        .expect("Value expected to be present");
    let second = server_cm
        .recv_packet(&peer_id)
        .await
        .expect("Value expected to be present");
    assert!(first.is_identity() || second.is_identity());
    assert!(first.is_pair() || second.is_pair());
}

/// The empty-caps-device ordering case named in the brief: a device we've
/// never exchanged capabilities with (no map entry at all) must NOT have
/// plugin sends silently start failing — unknown caps means "don't gate",
/// not "gate everything". Upstream gates unconditionally because its caps
/// always arrive with identity; ordering here isn't guaranteed the same
/// way (see record_peer_capabilities's doc).
#[tokio::test]
async fn test_send_packet_allows_when_peer_capabilities_unknown() {
    let (client_cm, server_cm, peer_id, _t) = setup_gated_pair().await;
    // Deliberately never call record_peer_capabilities.

    let packet = Packet::new(
        "kdeconnect.mousepad.request".to_string(),
        serde_json::json!({}),
    );
    client_cm
        .send_packet(&peer_id, &packet)
        .await
        .expect("a device with no known capabilities must not be gated");

    let received = server_cm
        .recv_packet(&peer_id)
        .await
        .expect("Value expected to be present");
    assert_eq!(received.packet_type, "kdeconnect.mousepad.request");
}

/// A capability update (e.g. a fresh identity re-announcing a newly
/// installed plugin) must re-allow a previously-refused type — the gate
/// reads the CURRENT map, not a value cached from the first refusal.
#[tokio::test]
async fn test_capability_update_re_allows_previously_refused_type() {
    let (client_cm, server_cm, peer_id, _t) = setup_gated_pair().await;

    client_cm
        .record_peer_capabilities(
            &peer_id,
            &["kdeconnect.ping".to_string()],
            &["kdeconnect.ping".to_string()],
        )
        .await;

    let packet = Packet::new(
        "kdeconnect.mousepad.request".to_string(),
        serde_json::json!({}),
    );
    client_cm
        .send_packet(&peer_id, &packet)
        .await
        .expect_err("must be refused before the capability update");

    client_cm
        .record_peer_capabilities(
            &peer_id,
            &[
                "kdeconnect.ping".to_string(),
                "kdeconnect.mousepad.request".to_string(),
            ],
            &["kdeconnect.ping".to_string()],
        )
        .await;

    client_cm
        .send_packet(&peer_id, &packet)
        .await
        .expect("must be allowed after the capability update");

    let received = server_cm
        .recv_packet(&peer_id)
        .await
        .expect("Value expected to be present");
    assert_eq!(received.packet_type, "kdeconnect.mousepad.request");
}

/// A subsequent empty-caps identity (legitimate or hostile) must NOT wipe
/// capabilities already learned — same non-empty-both guard as
/// `Device::apply_capability_update`.
#[tokio::test]
async fn test_record_peer_capabilities_empty_update_does_not_erase_known_caps() {
    let (client_cm, server_cm, peer_id, _t) = setup_gated_pair().await;

    client_cm
        .record_peer_capabilities(
            &peer_id,
            &["kdeconnect.ping".to_string()],
            &["kdeconnect.ping".to_string()],
        )
        .await;
    // A hostile or legitimately-empty follow-up identity must be a no-op.
    client_cm.record_peer_capabilities(&peer_id, &[], &[]).await;

    let packet = Packet::ping();
    client_cm
        .send_packet(&peer_id, &packet)
        .await
        .expect("previously-known capabilities must survive an empty update");

    let received = server_cm
        .recv_packet(&peer_id)
        .await
        .expect("Value expected to be present");
    assert_eq!(received.packet_type, "kdeconnect.ping");
}

/// Proves the PRODUCTION wiring, not just the gate's logic: a real
/// `connect_to_device` identity exchange populates `peer_capabilities`
/// from the remote identity's advertised `incomingCapabilities`, and
/// `send_packet` gates on it immediately afterward with no test-only
/// setup step.
#[tokio::test]
async fn test_capability_gating_wired_from_real_identity_exchange() {
    // TWO separate certificate managers, one per party. `tls_connect` /
    // `tls_accept` present the manager's fixed-path OWN cert (own.crt /
    // own.key) as the party's identity — sharing ONE manager between both
    // sides of a real handshake would make the remote peer's TLS client
    // cert carry OUR own CN instead of its own, since `connect_to_device`
    // already calls `ensure_own_certificate(our_id, ...)` on whatever
    // manager it holds.
    let our_temp = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(our_temp.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let peer_temp = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager_peer = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
    cert_manager_peer
        .init()
        .expect("Value expected to be present");

    let our_id = "wiring-our-device-aaaaaaaaaaaaaaaa";
    let remote_id = "wiring-remote-device-aaaaaaaaaaaa";

    let cm = ConnectionManager::new(cert_manager.clone()).expect("Value expected to be present");
    cm.set_device_identity(our_id, "Our Device");
    let our_identity = cm.get_identity().expect("Value expected to be present");

    // The remote peer advertises ONLY kdeconnect.ping as an incoming
    // capability — it never asked for kdeconnect.mousepad.request.
    let remote_identity = Identity::new(
        remote_id.to_string(),
        "Remote Device".to_string(),
        crate::device::types::DeviceType::Phone,
        vec!["kdeconnect.ping".to_string()],
        vec!["kdeconnect.ping".to_string()],
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Value expected to be present");
    let addr = listener.local_addr().expect("Value expected to be present");

    let remote_handle = tokio::spawn(async move {
        let (tcp_stream, _) = listener
            .accept()
            .await
            .expect("Value expected to be present");
        let mut buf_reader = tokio::io::BufReader::new(tcp_stream);
        use tokio::io::AsyncBufReadExt;
        let mut line = Vec::new();
        buf_reader
            .read_until(b'\n', &mut line)
            .await
            .expect("Value expected to be present");
        let _ = PacketSerializer::deserialize(&line).expect("Value expected to be present");

        // `connect_to_device` (outbound.rs) dials expecting the PEER to
        // drive TLS as the client and plays TLS SERVER itself (reversed-
        // role convention: whoever initiated TCP is the TLS server). This
        // test's remote peer is on the other end of that dial, so it
        // takes the TLS CLIENT role via `tls_connect`.
        let (tls_stream, _peer_cert) =
            super::tls::tls_connect(cert_manager_peer, remote_id, buf_reader.into_inner())
                .await
                .expect("Value expected to be present");

        // connect_to_device writes ITS OWN encrypted identity onto the
        // stream immediately after the handshake, before reading ours —
        // that must be drained first or it's what the later read below
        // sees instead of the ping.
        let mut tls_reader = tokio::io::BufReader::new(tls_stream);
        let mut our_identity_line = Vec::new();
        tls_reader
            .read_until(b'\n', &mut our_identity_line)
            .await
            .expect("Value expected to be present");
        let our_encrypted_identity = PacketSerializer::deserialize(&our_identity_line)
            .expect("Value expected to be present");
        assert!(our_encrypted_identity.is_identity());

        let resp = remote_identity
            .to_packet()
            .expect("Value expected to be present");
        let resp_bytes = PacketSerializer::serialize(&resp).expect("Value expected to be present");
        use tokio::io::AsyncWriteExt;
        tls_reader
            .get_mut()
            .write_all(&resp_bytes)
            .await
            .expect("Value expected to be present");
        tls_reader
            .get_mut()
            .flush()
            .await
            .expect("Value expected to be present");

        // Read (and discard) whatever the sends below actually put on the
        // wire — only kdeconnect.ping should ever arrive.
        let mut received_line = Vec::new();
        tls_reader
            .read_until(b'\n', &mut received_line)
            .await
            .expect("Value expected to be present");
        PacketSerializer::deserialize(&received_line).expect("Value expected to be present")
    });

    let (device_id, _remote_identity, _generation) = cm
        .connect_to_device(&our_identity, addr, None)
        .await
        .expect("Value expected to be present");
    assert_eq!(device_id, remote_id);

    // Refused: the real identity exchange above never advertised this.
    let unsupported = Packet::new(
        "kdeconnect.mousepad.request".to_string(),
        serde_json::json!({}),
    );
    cm.send_packet(&device_id, &unsupported)
        .await
        .expect_err("production wiring must gate on the exchanged identity's capabilities");

    // Allowed: it was advertised.
    cm.send_packet(&device_id, &Packet::ping())
        .await
        .expect("kdeconnect.ping was advertised by the real identity exchange");

    let received = remote_handle.await.expect("Value expected to be present");
    assert_eq!(received.packet_type, "kdeconnect.ping");
}

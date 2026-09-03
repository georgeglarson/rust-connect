use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

use rust_connect::api::build_router;
use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;

async fn create_test_app() -> (Arc<AppState>, tempfile::TempDir, String) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let api_key = "test-api-key".to_string();
    let settings = AppSettings::default()
        .with_data_dir(temp_dir.path().to_path_buf())
        .with_cert_dir(temp_dir.path().join("certs"))
        .with_api_keys(vec![api_key.clone()]);
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    state.init_plugins().await;
    (state, temp_dir, api_key)
}

/// Build a router whose `device_id` is registered and connected through a
/// real TLS pair. accept_test performs a TLSv1.2 handshake, so the client
/// must speak TLS (a bare TCP connect leaves the handshake — and the test —
/// blocked forever). The accepting side needs a valid on-wire device id for
/// its own certificate. The returned client stream must be kept alive for
/// the duration of the test or the device disconnects.
#[allow(clippy::type_complexity)]
async fn app_with_connected_device(
    device_id: &str,
) -> (
    axum::Router,
    tempfile::TempDir,
    String,
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
) {
    let (state, temp_dir, api_key) = create_test_app().await;

    let device = rust_connect::device::Device::new(
        device_id.to_string(),
        "Test Device".to_string(),
        rust_connect::device::types::DeviceType::Phone,
        7,
    );
    state.registry.add(device).await.unwrap();

    state
        .connection_manager
        .set_device_identity("daemon-self-aaaaaaaaaaaaaaaaaaaa", "Test Daemon");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client_handle = tokio::spawn(async move {
        #[derive(Debug)]
        struct AcceptAny {
            provider: Arc<rustls::crypto::CryptoProvider>,
        }
        impl rustls::client::danger::ServerCertVerifier for AcceptAny {
            fn verify_server_cert(
                &self,
                _e: &rustls::pki_types::CertificateDer<'_>,
                _i: &[rustls::pki_types::CertificateDer<'_>],
                _s: &rustls::pki_types::ServerName<'_>,
                _o: &[u8],
                _n: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                m: &[u8],
                c: &rustls::pki_types::CertificateDer<'_>,
                d: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                rustls::crypto::verify_tls12_signature(
                    m,
                    c,
                    d,
                    &self.provider.signature_verification_algorithms,
                )
            }
            fn verify_tls13_signature(
                &self,
                m: &[u8],
                c: &rustls::pki_types::CertificateDer<'_>,
                d: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                rustls::crypto::verify_tls13_signature(
                    m,
                    c,
                    d,
                    &self.provider.signature_verification_algorithms,
                )
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                self.provider
                    .signature_verification_algorithms
                    .supported_schemes()
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny { provider }))
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let server_name = rustls::pki_types::ServerName::try_from("kdeconnect")
            .unwrap()
            .to_owned();
        connector.connect(server_name, stream).await.unwrap()
    });
    let server_stream = listener.accept().await.unwrap().0;

    state
        .connection_manager
        .accept_test(device_id.to_string(), server_stream)
        .await
        .unwrap();

    // The client handshake completes once accept_test drives the server side.
    let client_stream = client_handle.await.unwrap();

    (build_router(state), temp_dir, api_key, client_stream)
}

#[tokio::test]
async fn test_share_path_traversal_blocked() {
    let (app, _temp, api_key, _client_stream) =
        app_with_connected_device("some-deviceaaaaaaaaaaaaaaaaaaaaa").await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/some-deviceaaaaaaaaaaaaaaaaaaaaa/share/send?filename=../../../etc/passwd")
                .header("X-API-Key", &api_key)
                .body(Body::from("file content"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

    assert_eq!(body["error"]["code"], "INVALID_REQUEST");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Invalid filename"));
}

/// Multipart uploads (curl -F, browser forms) must be parsed as multipart:
/// a traversal attempt in the PART filename must be caught by the same
/// sanitize step as the query-param path. A raw-body misread of this
/// request would fall back to the "shared_file" default and pass sanitize
/// — so the 400 proves the part filename was actually extracted.
/// Regression test for a live interop bug where `curl -F` delivered
/// multipart framing to the phone as file content. (Positive-path parsing
/// is unit-tested in src/api/handlers/share.rs; a full positive endpoint
/// test would need a simulated phone on the payload channel.)
#[tokio::test]
async fn test_share_multipart_part_filename_traversal_blocked() {
    let (app, _temp, api_key, _client_stream) =
        app_with_connected_device("some-deviceaaaaaaaaaaaaaaaaaaaaa").await;

    let multipart_body = "--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"../../../etc/passwd\"\r\nContent-Type: application/octet-stream\r\n\r\nbytes\r\n--BOUNDARY--\r\n";

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/some-deviceaaaaaaaaaaaaaaaaaaaaa/share/send")
                .header("X-API-Key", &api_key)
                .header("Content-Type", "multipart/form-data; boundary=BOUNDARY")
                .body(Body::from(multipart_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["error"]["code"], "INVALID_REQUEST");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Invalid filename"));
}

/// Multipart uploads larger than axum's default 2 MiB DefaultBodyLimit must
/// reach the handler: the share send route raises the limit to the 100 MiB
/// upload cap. A ~3 MiB part would previously die as 413 before parse_upload
/// ran; here it is parsed and only fails later on the traversal filename —
/// so the assertion is specifically "not 413".
#[tokio::test]
async fn test_share_multipart_over_default_body_limit_not_rejected_413() {
    let (app, _temp, api_key, _client_stream) =
        app_with_connected_device("some-deviceaaaaaaaaaaaaaaaaaaaaa").await;

    let content = "x".repeat(3 * 1024 * 1024);
    let multipart_body = format!(
        "--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n{content}\r\n--BOUNDARY--\r\n"
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/some-deviceaaaaaaaaaaaaaaaaaaaaa/share/send?filename=../../../etc/passwd")
                .header("X-API-Key", &api_key)
                .header("Content-Type", "multipart/form-data; boundary=BOUNDARY")
                .body(Body::from(multipart_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Receive-path coverage: drive an incoming `kdeconnect.share.request`
/// through the SharePlugin over a real loopback payload-TLS transfer, with
/// cert stores that trust each other like paired devices (same TOFU
/// cross-store pattern as payload_transfer's tls_tests).
mod receive_security {
    use rust_connect::plugins::{Plugin, SharePlugin};
    use rust_connect::protocol::payload_transfer::PayloadTransfer;
    use rust_connect::protocol::types::Packet;
    use rust_connect::protocol::CertificateManager;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const SENDER_ID: &str = "sender-device-aaaaaaaaaaaaaaaaaaaaa";
    const RECEIVER_ID: &str = "receiver-device-aaaaaaaaaaaaaaaaaaa";

    fn paired_cert_managers() -> (
        Arc<CertificateManager>,
        Arc<CertificateManager>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let t_a = tempfile::TempDir::new().unwrap();
        let t_b = tempfile::TempDir::new().unwrap();
        let cm_a = Arc::new(CertificateManager::new(t_a.path().to_path_buf()));
        let cm_b = Arc::new(CertificateManager::new(t_b.path().to_path_buf()));
        cm_a.init().unwrap();
        cm_b.init().unwrap();
        cm_a.ensure_own_certificate(SENDER_ID, "Sender").unwrap();
        cm_b.ensure_own_certificate(RECEIVER_ID, "Receiver")
            .unwrap();

        let (a_pem, _) = cm_a.load_own_certificate().unwrap();
        let (b_pem, _) = cm_b.load_own_certificate().unwrap();
        let a_der = openssl::x509::X509::from_pem(&a_pem)
            .unwrap()
            .to_der()
            .unwrap();
        let b_der = openssl::x509::X509::from_pem(&b_pem)
            .unwrap()
            .to_der()
            .unwrap();
        cm_a.store_peer_certificate(RECEIVER_ID, &b_der).unwrap();
        cm_b.store_peer_certificate(SENDER_ID, &a_der).unwrap();

        (cm_a, cm_b, t_a, t_b)
    }

    /// Offer `content` from a loopback payload-TLS sender, hand the plugin a
    /// share request for `filename`, and wait for the spawned receive task to
    /// record the file. Returns the plugin for further assertions.
    async fn share_roundtrip(download_dir: &Path, filename: &str, content: &[u8]) -> SharePlugin {
        let (cm_sender, cm_receiver, _t_a, _t_b) = paired_cert_managers();
        let src_dir = tempfile::TempDir::new().unwrap();
        let src = src_dir.path().join("payload.bin");
        std::fs::write(&src, content).unwrap();

        let sender = PayloadTransfer::new(cm_sender, RECEIVER_ID.to_string());
        let (mut info, send_handle) = sender
            .send_file(&src, "127.0.0.1".parse().expect("ip"))
            .await
            .unwrap();
        info.ip = Some("127.0.0.1".to_string()); // loopback for the test

        let plugin = SharePlugin::new()
            .with_cert_manager(cm_receiver)
            .with_download_dir(download_dir.to_path_buf());
        let packet = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({ "filename": filename }),
        )
        .with_payload_size(content.len() as u64)
        .with_payload_transfer_info(serde_json::to_value(&info).unwrap());
        plugin.handle_packet(SENDER_ID, packet).await.unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while plugin.received_files().await.is_empty() {
            assert!(Instant::now() < deadline, "receive task did not complete");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        send_handle.abort();
        plugin
    }

    /// A nested filename must land as its basename in the download dir — the
    /// intermediate components must never be created or traversed.
    #[tokio::test]
    async fn test_share_nested_filename_is_flattened_to_basename() {
        let download_dir = tempfile::TempDir::new().unwrap();

        let plugin = share_roundtrip(download_dir.path(), "subdir/evil.txt", b"payload").await;

        assert!(
            !download_dir.path().join("subdir").exists(),
            "intermediate directory must not be created"
        );
        assert_eq!(
            std::fs::read(download_dir.path().join("evil.txt")).unwrap(),
            b"payload"
        );
        let files = plugin.received_files().await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "evil.txt");
    }

    /// A symlinked intermediate directory inside the download dir must not be
    /// followed: `link/evil.txt` flattens to `evil.txt`, so nothing is
    /// written through the symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_share_symlinked_intermediate_dir_not_followed() {
        let download_dir = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), download_dir.path().join("link")).unwrap();

        let _plugin = share_roundtrip(download_dir.path(), "link/evil.txt", b"payload").await;

        assert!(
            !outside.path().join("evil.txt").exists(),
            "nothing may be written through the symlinked intermediate dir"
        );
        assert_eq!(
            std::fs::read(download_dir.path().join("evil.txt")).unwrap(),
            b"payload"
        );
    }
}

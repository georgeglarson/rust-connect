//! Integration tests for REST API endpoints

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use rust_connect::api::build_router;
use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use rust_connect::device::{Device, DeviceType};
use rust_connect::plugins::Plugin;
use utoipa::OpenApi;

async fn create_test_app() -> (Arc<AppState>, tempfile::TempDir, String) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let api_key = "test-api-key".to_string();
    let settings = AppSettings::default()
        .with_data_dir(temp_dir.path().to_path_buf())
        .with_api_keys(vec![api_key.clone()]);
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    state.init_plugins().await;
    (state, temp_dir, api_key)
}

async fn create_test_app_with_keys(keys: Vec<String>) -> (Arc<AppState>, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let settings =
        AppSettings::new_with_data_dir(temp_dir.path().to_path_buf()).with_api_keys(keys);
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    (state, temp_dir)
}

#[tokio::test]
async fn test_list_devices_empty() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], "ok");
    assert_eq!(json["data"]["total"], 0);
}

#[tokio::test]
async fn test_list_devices_with_data() {
    let (state, _temp, api_key) = create_test_app().await;

    let device = Device::new(
        "test-phone".to_string(),
        "Test Phone".to_string(),
        DeviceType::Phone,
        7,
    );
    state.registry.add(device).await.unwrap();

    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], "ok");
    assert_eq!(json["data"]["total"], 1);
    assert_eq!(json["data"]["devices"][0]["id"], "test-phone");
}

#[tokio::test]
async fn test_list_plugins() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/plugins")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], "ok");
    let plugins = json["data"]["plugins"].as_array().unwrap();
    assert!(plugins.len() >= 6);
}

#[tokio::test]
async fn test_api_key_auth_rejects_no_key() {
    let (state, _temp) = create_test_app_with_keys(vec!["secret-key".to_string()]).await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_api_key_auth_accepts_valid_key() {
    let (state, _temp) = create_test_app_with_keys(vec!["secret-key".to_string()]).await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices")
                .header("X-API-Key", "secret-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_api_key_auth_empty_key_list_rejects() {
    // Fail closed: with no keys configured, any presented key must be rejected.
    let (state, _temp) = create_test_app_with_keys(vec![]).await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices")
                .header("X-API-Key", "any-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_cors_denied_by_default() {
    // allowed_origins defaults to empty: no cross-origin access is granted.
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header("Origin", "https://evil.example")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("Access-Control-Allow-Origin")
        .is_none());
}

#[tokio::test]
async fn test_cors_wildcard_when_configured() {
    // An explicit "*" must keep working for callers that opt in.
    let (state, _temp, api_key) = create_test_app().await;
    let mut settings = state.settings.clone();
    settings.allowed_origins = vec!["*".to_string()];
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    state.init_plugins().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header("Origin", "https://example.com")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response
            .headers()
            .get("Access-Control-Allow-Origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}

#[tokio::test]
async fn test_get_device_returns_404() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices/does-not-existaaaaaaaaaaaaaaaaaa")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_device_returns_device() {
    let (state, _temp, api_key) = create_test_app().await;

    let device = Device::new(
        "testphoneaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        "Test Phone".to_string(),
        DeviceType::Phone,
        8,
    );
    state.registry.add(device.clone()).await.unwrap();

    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices/testphoneaaaaaaaaaaaaaaaaaaaaaaa")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], "ok");
    assert_eq!(json["data"]["id"], "testphoneaaaaaaaaaaaaaaaaaaaaaaa");
}

#[tokio::test]
async fn test_cannot_bind_same_port_twice() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let result = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await;
    assert!(result.is_err(), "Second bind to same port should fail");
    drop(listener);
}

#[tokio::test]
async fn test_pair_device() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-deviceaaaaaaaaaaaaaaaaaaaaa/pair")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(
        json["data"]["device_id"],
        "test-deviceaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(json["data"]["status"], "pairing_initiated");

    assert!(
        state
            .pairing_handler
            .has_pending_request(&"test-deviceaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
    );
}

#[tokio::test]
async fn test_pair_device_invalid_id() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/invalid%20device/pair")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_unpair_device() {
    let (state, _temp, api_key) = create_test_app().await;

    state
        .pairing_handler
        .receive_pair_request(
            &"unpair-meaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            Some(1_700_000_000),
        )
        .await
        .unwrap();
    state
        .pairing_handler
        .accept_pairing(&"unpair-meaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .unwrap();
    assert!(
        state
            .pairing_handler
            .is_paired(&"unpair-meaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
    );

    let app = build_router(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/devices/unpair-meaaaaaaaaaaaaaaaaaaaaaaa/unpair")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["status"], "unpaired");

    assert!(
        !state
            .pairing_handler
            .is_paired(&"unpair-meaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
    );
}

/// Unpair must release SFTP credentials and any tracked mount, even
/// when the device is not currently connected. Mirrors the lifecycle
/// matrix in the lane brief.
#[tokio::test]
async fn test_unpair_drops_sftp_credentials() {
    let (state, _temp, api_key) = create_test_app().await;
    let device_id = "unpair-sftp-aaaaaaaaaaaaaaaaaaaaa";

    // Plant pairing + SFTP creds as if the device had connected and
    // sent an sftp packet.
    state
        .pairing_handler
        .receive_pair_request(&device_id.to_string(), Some(1_700_000_000))
        .await
        .unwrap();
    state
        .pairing_handler
        .accept_pairing(&device_id.to_string())
        .await
        .unwrap();
    let pkt = rust_connect::protocol::types::Packet::new(
        "kdeconnect.sftp".to_string(),
        serde_json::json!({
            "ip": "192.168.1.50",
            "port": 1740,
            "user": "kdeconnect",
            "password": "device-secret-7c3",
            "path": "/storage/emulated/0"
        }),
    );
    state
        .plugins
        .sftp
        .handle_packet(device_id, pkt)
        .await
        .expect("handle sftp packet");
    assert!(state.plugins.sftp.get_connection(device_id).is_some());

    let app = build_router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/devices/{device_id}/unpair"))
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // SFTP credentials and mount state must be gone after the unpair.
    assert!(
        state.plugins.sftp.get_connection(device_id).is_none(),
        "unpair must drop SFTP credentials"
    );
    assert_eq!(
        state.plugins.sftp.get_mount_status(device_id).state,
        rust_connect::plugins::sftp::MountState::Unmounted
    );
}

/// Daemon-shutdown cleanup: cleanup_all must release every tracked
/// SFTP mount + drop every stored credential.
#[tokio::test]
async fn test_daemon_shutdown_releases_sftp() {
    let (state, _temp, _api_key) = create_test_app().await;
    let device_id = "shutdown-sftp-aaaaaaaaaaaaaaaaaa";
    let pkt = rust_connect::protocol::types::Packet::new(
        "kdeconnect.sftp".to_string(),
        serde_json::json!({
            "ip": "192.168.1.51",
            "port": 1740,
            "user": "kdeconnect",
            "password": "shutdown-secret",
            "path": "/"
        }),
    );
    state
        .plugins
        .sftp
        .handle_packet(device_id, pkt)
        .await
        .expect("handle sftp packet");
    assert!(state.plugins.sftp.get_connection(device_id).is_some());

    state.plugins.sftp.cleanup_all().await;
    assert!(state.plugins.sftp.get_connection(device_id).is_none());
    assert_eq!(
        state.plugins.sftp.get_mount_status(device_id).state,
        rust_connect::plugins::sftp::MountState::Unmounted
    );
}

/// POST /api/v1/devices/{id}/sftp/mount without creds → 4xx. The
/// specific code depends on whether sshfs is on PATH (400 with a
/// credentials hint, or 503 for backend unavailable). Both are
/// legitimate "client must /sftp/request first OR install sshfs"
/// outcomes per the lane brief.
#[tokio::test]
async fn test_sftp_mount_without_credentials_returns_4xx() {
    let (state, _temp, api_key) = create_test_app().await;
    let device_id = "no-creds-sftp-device-aaaaaaaaaaaa";
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/devices/{device_id}/sftp/mount"))
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::SERVICE_UNAVAILABLE,
        "expected 400 or 503, got {status}"
    );
}

/// DELETE /api/v1/devices/{id}/sftp/mount when nothing is mounted → 404.
#[tokio::test]
async fn test_sftp_unmount_when_not_mounted_returns_404() {
    let (state, _temp, api_key) = create_test_app().await;
    let device_id = "not-mounted-sftp-aaaaaaaaaaaaaaa";
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/devices/{device_id}/sftp/mount"))
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/devices/{id}/sftp response now carries `mounted` and
/// `mount_point` (and `mount_state`).
#[tokio::test]
async fn test_sftp_info_response_includes_mount_state() {
    let (state, _temp, api_key) = create_test_app().await;
    let device_id = "sftp-info-state-aaaaaaaaaaaaaaaa";
    // Plant credentials so the endpoint returns 200 (not 404).
    let pkt = rust_connect::protocol::types::Packet::new(
        "kdeconnect.sftp".to_string(),
        serde_json::json!({
            "ip": "192.168.1.55",
            "port": 1740,
            "user": "kdeconnect",
            "password": "state-secret",
            "path": "/storage/emulated/0"
        }),
    );
    state
        .plugins
        .sftp
        .handle_packet(device_id, pkt)
        .await
        .expect("handle sftp packet");
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/devices/{device_id}/sftp"))
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let data = &body["data"];
    // The mount-state surface is always present.
    assert!(data.get("mounted").is_some(), "missing mounted: {data}");
    assert!(
        data.get("mount_point").is_some(),
        "missing mount_point: {data}"
    );
    assert!(
        data.get("mount_state").is_some(),
        "missing mount_state: {data}"
    );
    // And the password is NOT in the response.
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        !serialized.contains("state-secret"),
        "password leaked in API response: {serialized}"
    );
}

/// Both new endpoints appear in the OpenAPI spec.
#[test]
fn test_sftp_mount_endpoints_in_openapi() {
    let spec = rust_connect::api::openapi::ApiDoc::openapi();
    let paths: Vec<&str> = spec.paths.paths.keys().map(|s| s.as_str()).collect();
    assert!(
        paths.contains(&"/api/v1/devices/{device_id}/sftp/mount"),
        "mount path missing from OpenAPI: {paths:?}"
    );
}

/// /api/v1/tools honesty: the browse_sftp tool's `available` flag MUST
/// match the mounter's live backend probe on whatever host runs the
/// suite. Asserting a fixed true/false would bake in a host assumption
/// (this broke on 2026-08-06 when sshfs was installed on the dev host);
/// the invariant under test is end-to-end agreement between the /tools
/// surface and the probe. Both absolute legs are covered by the
/// fake-runner mounter tests.
#[tokio::test]
async fn test_sftp_tool_availability_matches_backend_probe() {
    let expected = rust_connect::plugins::sftp::mounter::Mounter::new(std::sync::Arc::new(
        rust_connect::plugins::sftp::mounter::SystemCommandRunner::new(),
    ))
    .is_available();
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/tools")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let tools = body["data"]["tools"].as_array().expect("tools array");
    let sftp_tool = tools
        .iter()
        .find(|t| t["name"] == "browse_sftp")
        .expect("browse_sftp tool must appear in /tools");
    // The tool is always listed (discoverability); its availability
    // flag must equal the probe's verdict on this host.
    assert_eq!(
        sftp_tool["available"],
        serde_json::Value::Bool(expected),
        "browse_sftp availability must match the backend probe"
    );
    assert_eq!(
        sftp_tool["endpoint"],
        "/api/v1/devices/{device_id}/sftp/mount"
    );
}

#[tokio::test]
async fn test_unpair_device_not_paired() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/devices/never-pairedaaaaaaaaaaaaaaaaaaaa/unpair")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(response.status() == StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_send_ping_validates_device_id() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let body = serde_json::json!({"device_id": "invalid device!"});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/ping")
                .header("X-API-Key", &api_key)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_send_ping_no_auth() {
    let (state, _temp, _api_key) = create_test_app().await;
    let app = build_router(state);

    let body = serde_json::json!({"device_id": "any-device"});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/ping")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_events_requires_auth() {
    let (state, _temp, _api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_events_returns_stream_content_type() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/events")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "Expected text/event-stream, got: {}",
        content_type
    );
    assert_eq!(
        response
            .headers()
            .get("Cache-Control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
}

#[tokio::test]
async fn test_health_endpoint_no_auth_required() {
    let (state, _temp, _api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_security_headers_present() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("X-Content-Type-Options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        response
            .headers()
            .get("X-Frame-Options")
            .and_then(|v| v.to_str().ok()),
        Some("DENY")
    );
    assert_eq!(
        response
            .headers()
            .get("Referrer-Policy")
            .and_then(|v| v.to_str().ok()),
        Some("strict-origin-when-cross-origin")
    );
    assert_eq!(
        response
            .headers()
            .get("Permissions-Policy")
            .and_then(|v| v.to_str().ok()),
        Some("geolocation=(), microphone=(), camera=()")
    );
}

#[tokio::test]
async fn test_device_endpoints_expose_pair_state_for_incoming_request() {
    let (state, _temp, api_key) = create_test_app().await;

    // Generate and store own certificate so we can compute verification keys
    state
        .cert_manager
        .ensure_own_certificate("test-daemon-aaaaaaaaaaaaaaaaaaaaaa", "Test Daemon")
        .expect("Value expected to be present");

    let device_id = "pairstate-peer-aaaaaaaaaaaaaaaaaa".to_string();
    let device = Device::new(
        device_id.clone(),
        "test phone".to_string(),
        DeviceType::Phone,
        8,
    );
    state.registry.add(device).await.unwrap();

    // Stage an incoming request carrying its peer cert — the connection-loop
    // order — so the verification key is computable while it is pending.
    let (cert_pem, _) = state
        .cert_manager
        .generate_certificate(&device_id, "Peer")
        .unwrap();
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .unwrap()
        .to_der()
        .unwrap();
    state
        .pairing_handler
        .receive_pair_request_with_cert(
            &device_id,
            Some(chrono::Utc::now().timestamp()),
            Some(cert_der),
        )
        .await
        .unwrap();

    let app = build_router(state.clone());

    // Detail endpoint: pair_state + verification_key both present.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/devices/{device_id}"))
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let data = &json["data"];
    assert_eq!(data["pair_state"], "requested_by_peer");
    assert!(
        data["verification_key"].is_string(),
        "SAS must be exposed while the incoming request is pending: {data}"
    );

    // List endpoint: same two fields on the summary.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let listed = &json["data"]["devices"][0];
    assert_eq!(listed["pair_state"], "requested_by_peer");
    assert!(listed["verification_key"].is_string());
}

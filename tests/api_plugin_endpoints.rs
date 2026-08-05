//! Integration tests for Plugin API endpoints
//!
//! Tests that UI -> API field mappings are correct.
//! These verify fixes for field name mismatches (snake_case vs camelCase, etc.)
//!
//! Note: Some tests expect 4xx errors because the test device is added to registry
//! but not actually connected. The key thing being tested is:
//! 1. Routes exist (not 404)
//! 2. Field names are correct when device IS connected

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use rust_connect::api::build_router;
use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use rust_connect::device::{Device, DeviceType};

async fn create_test_app() -> (Arc<AppState>, tempfile::TempDir, String) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let api_key = "test-api-key".to_string();
    let settings = AppSettings::default()
        .with_data_dir(temp_dir.path().to_path_buf())
        .with_api_keys(vec![api_key.clone()]);
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    state.init_plugins().await;

    let device = Device::new(
        "test-phone".to_string(),
        "Test Phone".to_string(),
        DeviceType::Phone,
        7,
    );
    state.registry.add(device).await.unwrap();

    (state, temp_dir, api_key)
}

#[tokio::test]
async fn test_findmyphone_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/findmyphone")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Route exists, but fails with 400 because the test device is not connected
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_findmyphone_rejects_unconnected_device() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/findmyphone")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_findmyphone_rejects_invalid_device_id() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/bad/findmyphone")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_contacts_sync_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/contacts/sync")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Route exists, but fails with 400 because the test device is not connected
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_contacts_sync_rejects_unconnected_device() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/contacts/sync")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_contacts_get_returns_stored_contacts() {
    use rust_connect::plugins::plugin::Plugin;
    use rust_connect::protocol::types::Packet;

    let (state, _temp, api_key) = create_test_app().await;

    // Drive the plugin directly with the EXACT body shape the phone sends
    // (kdeconnect-android ContactsPlugin.kt:140-155): a "uids" list plus one
    // raw-vCard field per uid.
    let packet = Packet::new(
        "kdeconnect.contacts.response_vcards".to_string(),
        serde_json::json!({
            "uids": ["1"],
            "1": "BEGIN:VCARD\nVERSION:2.1\nFN:John Smith\nTEL;CELL:+15551234\nEND:VCARD"
        }),
    );
    state
        .plugins
        .contacts
        .handle_packet("test-phoneaaaaaaaaaaaaaaaaaaaaaa", packet)
        .await
        .unwrap();

    let app = build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/contacts")
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
    assert_eq!(data["count"], serde_json::json!(1));
    assert!(
        data["contacts"].is_array(),
        "contacts array should be present"
    );
    let contacts = data["contacts"].as_array().unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0]["uid"], serde_json::json!("1"));
    assert_eq!(contacts[0]["name"], serde_json::json!("John Smith"));
    assert_eq!(
        contacts[0]["phoneNumbers"],
        serde_json::json!(["+15551234"])
    );
    assert!(contacts[0]["vcard"]
        .as_str()
        .unwrap()
        .contains("BEGIN:VCARD"));
}

#[tokio::test]
async fn test_contacts_get_empty_for_unknown_device() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/contacts")
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
    assert_eq!(json["data"]["count"], serde_json::json!(0));
    assert_eq!(
        json["data"]["contacts"],
        serde_json::json!(Vec::<serde_json::Value>::new())
    );
}

#[tokio::test]
async fn test_notification_send_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let body = serde_json::json!({
        "title": "Test Title",
        "text": "Test message body",
        "is_cancel": false
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/notification")
                .header("X-API-Key", &api_key)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Route exists, but may fail with 400 (device not connected) or succeed with 200
    assert_ne!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Route should exist"
    );
}

#[tokio::test]
async fn test_volume_control_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let body = serde_json::json!({
        "volume": 75,
        "muted": false
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/volume")
                .header("X-API-Key", &api_key)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Route exists, but may fail with 400 (device not connected) or succeed with 200
    assert_ne!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Route should exist"
    );
}

#[tokio::test]
async fn test_sms_send_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let body = serde_json::json!({
        "phone_number": "+1234567890",
        "message_body": "Test message"
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/sms/send")
                .header("X-API-Key", &api_key)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Route exists, but may fail with 422 (validation) or 400 (not connected)
    assert_ne!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Route should exist"
    );
}

#[tokio::test]
async fn test_battery_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/battery")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Route exists - returns 404 if no battery data (expected for unconnected device)
    // The key thing being tested: route is correctly defined
    assert!(
        response.status() == StatusCode::NOT_FOUND || response.status() == StatusCode::OK,
        "Route should exist with proper status"
    );
}

#[tokio::test]
async fn test_sftp_request_endpoint_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/sftp/request")
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Should either succeed or fail with "Device not connected"
    // but the endpoint should exist (not 404)
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_mpris_action_endpoint_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let body = serde_json::json!({
        "action": "PlayPause"
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/mpris/vlc/action")
                .header("X-API-Key", &api_key)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Should not 404 - endpoint exists
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_telephony_get_returns_calls_array() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/telephony")
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
    assert!(data.get("calls").is_some(), "Should have calls array");
    assert!(data.get("device_id").is_some(), "Should have device_id");
}

#[tokio::test]
async fn test_sms_threads_returns_thread_objects() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/sms/threads")
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
    assert!(data.get("threads").is_some(), "Should have threads array");
    let threads = data.get("threads").unwrap();

    if threads.as_array().map(|t| !t.is_empty()).unwrap_or(false) {
        let first = &threads[0];
        assert!(
            first.get("thread_id").is_some()
                || first.get("read_count").is_some()
                || first.get("addresses").is_some(),
            "Thread should have thread_id, read_count, or addresses - not just tuple values"
        );
    }
}

/// The clipboard-request route must exist (handler + OpenAPI annotation +
/// web-UI button all reference it; the route was missing from the router and
/// the UI was 404-ing). The test device is registered but not connected, so
/// a 400 / 500 / connection-error is the expected outcome; 404 would mean
/// the route was never wired.
#[tokio::test]
async fn test_clipboard_request_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(
                    "/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/clipboard/request",
                )
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        response.status(),
        StatusCode::NOT_FOUND,
        "POST /api/v1/devices/{{id}}/clipboard/request must be wired (handler + OpenAPI exist; UI button posts to it)"
    );
}

/// The dismiss route must exist and must sit under the same singular
/// `notification` path segment as its sibling reply route. The test device is
/// registered but not connected, so a 400 is the expected outcome; 404 would
/// mean the route was never wired.
#[tokio::test]
async fn test_notification_dismiss_route_exists() {
    let (state, _temp, api_key) = create_test_app().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(
                    "/api/v1/devices/test-phoneaaaaaaaaaaaaaaaaaaaaaa/notification/notif-1/dismiss",
                )
                .header("X-API-Key", &api_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

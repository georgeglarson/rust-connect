//! Standalone verification test for clipboard sync
//! This simulates a device connecting and sending a clipboard packet.

use anyhow::Context;
use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use rust_connect::protocol::Packet;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_clipboard_sync_flow() -> anyhow::Result<()> {
    // 1. Setup local daemon state
    let temp_dir = tempfile::TempDir::new().context("Failed to create temp dir")?;
    let settings = AppSettings::default()
        .with_data_dir(temp_dir.path().to_path_buf())
        .with_api_keys(vec!["test-key".to_string()]);
    let state = Arc::new(AppState::new(settings).context("Failed to init state")?);
    state.init_plugins().await;

    // 2. Start a TCP listener for the daemon to mimic the real protocol listener
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _addr = listener.local_addr().unwrap();

    // We'll manually route packets to the state to avoid the complexity of full TLS handshake in this mock
    // This tests the Plugin -> Router -> AppState flow.

    let device_id = "mock-phone-id".to_string();

    // 3. Create a clipboard packet
    let clipboard_packet = Packet::new(
        "kdeconnect.clipboard".to_string(),
        serde_json::json!({
            "content": "2026-04-08 Verification Date"
        }),
    );

    // 4. Route it through the daemon's router
    println!("Routing mock clipboard packet...");
    state
        .packet_router
        .route(&device_id, clipboard_packet)
        .await
        .context("Routing failed")?;

    // 5. Verify the plugin state
    let content = state.plugins.clipboard.get_content();
    println!("Plugin clipboard content: {:?}", content);

    assert_eq!(content.as_deref(), Some("2026-04-08 Verification Date"));
    println!("SUCCESS: Backend plugin state updated correctly.");

    // 6. Verify the API response
    use axum::body::Body;
    use rust_connect::api::build_router;
    use tower::ServiceExt;

    let app = build_router(state.clone());
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/v1/clipboard")
                .header("X-API-Key", "test-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    println!("API Response: {}", json);
    assert_eq!(json["data"]["content"], "2026-04-08 Verification Date");
    println!("SUCCESS: API returned correct clipboard content.");
    Ok(())
}

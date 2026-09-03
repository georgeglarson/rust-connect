use crate::api::extractors::api_err;
use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    get,
    path = "/api/v1/clipboard",
    tag = "clipboard",
    responses(
        (status = 200, description = "Get current clipboard content", body = GenericResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_clipboard(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    let content = state.plugins.clipboard.get_content();
    Ok(Json(ApiResponse::ok(serde_json::json!({
        "content": content,
    }))))
}

#[utoipa::path(
    post,
    path = "/api/v1/clipboard",
    tag = "clipboard",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Set clipboard content on connected devices", body = GenericResponse),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn set_clipboard(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| api_err(Error::InvalidRequest("content field required".to_string())))?;

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.clipboard".to_string(),
        serde_json::json!({ "content": content }),
    );

    let device_ids = state.connection_manager.connected_device_ids().await;
    let mut failed = 0usize;
    for device_id in &device_ids {
        if let Err(e) = state
            .connection_manager
            .send_packet(device_id, &packet)
            .await
        {
            tracing::warn!(
                device_id = %device_id,
                error = %e,
                "Failed to send clipboard packet"
            );
            failed += 1;
        }
    }

    state
        .plugins
        .clipboard
        .update_content(None, content.to_string(), None);

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "sent": true,
        "devices": device_ids.len(),
        "failed": failed,
    }))))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/clipboard/request",
    tag = "clipboard",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Clipboard sync request sent", body = GenericResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_clipboard(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    // timestamp=1 (epoch + 1ms) is a deliberate protocol hack: the phone
    // pushes its clipboard whenever the request's timestamp is NEWER than
    // its last-sent one, and 1 is newer than nothing. This forces a push
    // without knowing the phone's current timestamp.
    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.clipboard.connect".to_string(),
        serde_json::json!({ "timestamp": 1 }),
    );

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "sent": true
    }))))
}

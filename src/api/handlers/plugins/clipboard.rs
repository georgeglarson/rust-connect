use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::ApiJson;
use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[derive(Debug, Serialize, ToSchema)]
pub struct ClipboardContentResponse {
    /// `null` when the daemon has no clipboard content yet (a fresh
    /// process); the pre-typed literal emitted `\"content\": null` and
    /// consumers branch on it.
    pub content: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetClipboardRequest {
    pub content: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClipboardSetResponse {
    pub sent: bool,
    pub devices: usize,
    pub failed: usize,
}

#[utoipa::path(
    get,
    path = "/api/v1/clipboard",
    tag = "clipboard",
    responses(
        (status = 200, description = "Get current clipboard content", body = ClipboardContentResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_clipboard(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<ClipboardContentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    let content = state.plugins.clipboard.get_content();
    Ok(Json(ApiResponse::ok(ClipboardContentResponse { content })))
}

#[utoipa::path(
    post,
    path = "/api/v1/clipboard",
    tag = "clipboard",
    request_body = SetClipboardRequest,
    responses(
        (status = 200, description = "Set clipboard content on connected devices", body = ClipboardSetResponseWrapper),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn set_clipboard(
    State(state): State<Arc<AppState>>,
    // Raw value, not `Json<SetClipboardRequest>`: axum's extractor answers a
    // malformed body with its own bare 422, outside the envelope. The
    // struct documents the body in the spec; the handler validates it and
    // answers a 400 in the envelope, as it always has.
    ApiJson(body): ApiJson<serde_json::Value>,
) -> Result<Json<ApiResponse<ClipboardSetResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    let content = match body.get("content") {
        Some(serde_json::Value::String(s)) => s.as_str(),
        None => {
            return Err(api_err(Error::InvalidRequest(
                "content field required".to_string(),
            )))
        }
        Some(_) => {
            return Err(api_err(Error::InvalidRequest(
                "content field must be a string".to_string(),
            )))
        }
    };

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

    Ok(Json(ApiResponse::ok(ClipboardSetResponse {
        sent: true,
        devices: device_ids.len(),
        failed,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/clipboard/request",
    tag = "clipboard",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Clipboard sync request sent", body = SentResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_clipboard(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<SentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

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

    Ok(Json(ApiResponse::ok(SentResponse {
        device_id,
        sent: true,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_clipboard_content_response_matches_legacy_shape() {
        let resp = ClipboardContentResponse {
            content: Some("remember the milk".to_string()),
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({ "content": "remember the milk" });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_clipboard_set_response_matches_legacy_shape() {
        let resp = ClipboardSetResponse {
            sent: true,
            devices: 3,
            failed: 1,
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({
            "sent": true,
            "devices": 3,
            "failed": 1,
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_set_clipboard_request_parses() {
        let body: SetClipboardRequest =
            serde_json::from_str(r#"{"content":"x"}"#).expect("request body must parse");
        assert_eq!(body.content, "x");
    }
}

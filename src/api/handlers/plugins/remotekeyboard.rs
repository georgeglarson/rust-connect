#[allow(unused_imports)]
// utoipa `body = …` resolves schema names, not paths; the import keeps the name in scope for readers
use crate::api::types::GenericResponse;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    api::{
        extractors::validate_device_id,
        types::{ApiError, ApiResponse},
    },
    app::AppState,
    protocol::types::Packet,
};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SendKeypressRequest {
    pub key: String,
    #[serde(default)]
    pub special_key: i32,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    /// Meta/Windows key. kdeconnect-kde sends it on every request
    /// (plugins/remotekeyboard/remotekeyboardplugin.cpp:92) and reads it back
    /// off the echo at :74.
    #[serde(default)]
    pub super_key: bool,
    #[serde(default)]
    pub send_ack: bool,
}

/// The `kdeconnect.mousepad.request` body.
///
/// Key set and order follow kdeconnect-kde
/// plugins/remotekeyboard/remotekeyboardplugin.cpp:86-93.
fn keypress_payload(req: &SendKeypressRequest) -> serde_json::Value {
    serde_json::json!({
        "key": req.key,
        "specialKey": req.special_key,
        "shift": req.shift,
        "ctrl": req.ctrl,
        "alt": req.alt,
        "super": req.super_key,
        "sendAck": req.send_ack,
    })
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/remotekeyboard/keypress",
    tag = "remotekeyboard",
    request_body = SendKeypressRequest,
    responses(
        (status = 200, description = "Keypress sent successfully", body = GenericResponse),
        (status = 400, description = "Invalid device ID", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or not connected", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_remotekeyboard_keypress(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(req): Json<SendKeypressRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(crate::api::extractors::api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(crate::api::extractors::api_err(
            crate::utils::errors::Error::DeviceNotFound(device_id),
        ));
    }

    let payload = keypress_payload(&req);
    let packet = Packet::new("kdeconnect.mousepad.request".to_string(), payload);

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(crate::api::extractors::api_err)?;

    Ok(Json(ApiResponse::ok(
        serde_json::json!({ "status": "sent" }),
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_keypress_payload_carries_super() {
        let req = SendKeypressRequest {
            key: "e".to_string(),
            special_key: 0,
            shift: false,
            ctrl: false,
            alt: false,
            super_key: true,
            send_ack: true,
        };
        let payload = keypress_payload(&req);
        assert_eq!(payload.get("super").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(payload.get("key").and_then(|v| v.as_str()), Some("e"));
        assert_eq!(payload.get("sendAck").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(payload.get("specialKey").and_then(|v| v.as_i64()), Some(0));
    }

    #[test]
    fn test_keypress_payload_defaults_super_false() {
        let req = SendKeypressRequest {
            key: "a".to_string(),
            special_key: 12,
            shift: true,
            ctrl: false,
            alt: false,
            super_key: false,
            send_ack: false,
        };
        let payload = keypress_payload(&req);
        assert_eq!(payload.get("super").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(payload.get("shift").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(payload.get("specialKey").and_then(|v| v.as_i64()), Some(12));
    }
}

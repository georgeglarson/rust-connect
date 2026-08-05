use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/findmyphone",
    tag = "findmyphone",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Ring request sent to device", body = ApiResponse),
        (status = 400, description = "Invalid request or device not connected", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn find_my_phone(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    // Empty body per upstream: kdeconnect-kde
    // plugins/findmyphone/findmyphoneplugin.cpp:17-21, GSConnect
    // src/service/plugins/findmyphone.js:93-98.
    let packet = state.plugins.findmyphone.ring_request();
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "sent": true,
        "message": "Ring request sent. The device will ring until dismissed."
    }))))
}

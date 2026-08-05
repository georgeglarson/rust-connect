use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/lock",
    tag = "lock",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    request_body = LockDeviceRequest,
    responses(
        (status = 200, description = "Lock command sent to device", body = ApiResponse),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn lock_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<LockDeviceRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let action = match body.action.as_str() {
        "lock" => true,
        "unlock" => false,
        _ => {
            return Err(api_err(Error::InvalidRequest(
                "action must be 'lock' or 'unlock'".to_string(),
            )));
        }
    };

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.lock.request".to_string(),
        serde_json::json!({
            "setLocked": action
        }),
    );

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "action": body.action,
        "sent": true
    }))))
}

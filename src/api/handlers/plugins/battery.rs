use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/battery",
    tag = "battery",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get battery info from device", body = GenericResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or no battery data", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_battery(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    match state.plugins.battery.get_battery(&device_id) {
        Some(info) => Ok(Json(ApiResponse::ok(serde_json::json!({
            "current_charge": info.current_charge,
            "is_charging": info.is_charging,
            "threshold_event": info.threshold_event,
        })))),
        None => Err(api_err(Error::not_found("battery data", Some(device_id)))),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/battery/request",
    tag = "battery",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Battery request sent", body = GenericResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_device_battery(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.battery.request".to_string(),
        serde_json::json!({}),
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

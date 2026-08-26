use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/telephony",
    tag = "telephony",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get telephony events from device", body = ApiResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_telephony(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let calls = state.plugins.telephony.get_calls(&device_id);
    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "calls": calls,
    }))))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/telephony/mute",
    tag = "telephony",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Mute request sent", body = ApiResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
/// Mute a ringing call on the paired phone.
///
/// The desktop counterpart of kdeconnect-kde's "Mute Call" notification action
/// (`plugins/telephony/telephonyplugin.cpp:66`). Fire-and-forget, exactly like
/// upstream: the phone sends no reply, so a 200 means the packet went out, not
/// that a call was muted.
pub async fn mute_device_call(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let packet = crate::plugins::TelephonyPlugin::mute_request_packet();
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

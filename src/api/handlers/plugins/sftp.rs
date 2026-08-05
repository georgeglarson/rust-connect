use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/sftp/request",
    tag = "sftp",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "SFTP request sent to device", body = ApiResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_sftp(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let packet = state.plugins.sftp.request_sftp(&device_id);
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "sent": true,
        "message": "SFTP session requested. Poll GET /devices/{device_id}/sftp for connection details."
    }))))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/sftp",
    tag = "sftp",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get SFTP connection info", body = ApiResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or SFTP not available", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_sftp_info(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    match state.plugins.sftp.get_connection(&device_id) {
        Some(info) => Ok(Json(ApiResponse::ok(serde_json::json!({
            "device_id": device_id,
            "ip": info.ip,
            "port": info.port,
            "user": info.user,
            "path": info.path,
            "multi_paths": info.multi_paths,
            "path_names": info.path_names,
            "available": true
        })))),
        None => Err(api_err(Error::not_found(
            "sftp_connection",
            Some(device_id),
        ))),
    }
}

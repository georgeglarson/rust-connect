use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

use crate::plugins::sftp::MountState;
use crate::plugins::Plugin;

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

    let mount = state.plugins.sftp.get_mount_status(&device_id);
    let mounted = matches!(mount.state, MountState::Mounted);
    let mount_point_str = mount.mount_point.as_ref().map(|p| p.display().to_string());

    match state.plugins.sftp.get_connection(&device_id) {
        Some(info) => Ok(Json(ApiResponse::ok(serde_json::json!({
            "device_id": device_id,
            "ip": info.ip,
            "port": info.port,
            "user": info.user,
            "path": info.path,
            "multi_paths": info.multi_paths,
            "path_names": info.path_names,
            "available": true,
            "mounted": mounted,
            "mount_point": mount_point_str,
            "mount_state": state_label(&mount.state)
        })))),
        None => {
            // Even without credentials, surface the mount state so the UI
            // can render "not mounted" honestly rather than 404-ing.
            if mounted {
                Ok(Json(ApiResponse::ok(serde_json::json!({
                    "device_id": device_id,
                    "available": false,
                    "mounted": true,
                    "mount_point": mount_point_str,
                    "mount_state": state_label(&mount.state)
                }))))
            } else {
                Err(api_err(Error::not_found(
                    "sftp_connection",
                    Some(device_id),
                )))
            }
        }
    }
}

fn state_label(s: &MountState) -> &'static str {
    match s {
        MountState::Unmounted => "unmounted",
        MountState::Mounting => "mounting",
        MountState::Mounted => "mounted",
        MountState::Failed(_) => "failed",
    }
}

/// Mount the device's filesystem. The mount point is server-determined
/// (`<data_dir>/mounts/sftp-<device_id>`) and any client-supplied
/// `mountPoint` field in the body is ignored. Idempotent: an
/// already-mounted device returns its current state with 200.
#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/sftp/mount",
    tag = "sftp",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Mount state (mounted or already mounted)", body = ApiResponse),
        (status = 400, description = "No SFTP credentials yet — call POST /sftp/request first", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
        (status = 503, description = "sshfs / fusermount not available on this host", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn mount_sftp(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.plugins.sftp.is_backend_available() {
        return Err(api_err(Error::ServiceUnavailable(
            "sshfs or fusermount not found on PATH — install the fuse/sshfs package".to_string(),
        )));
    }

    let info = state
        .plugins
        .sftp
        .get_connection(&device_id)
        .ok_or_else(|| {
            api_err(Error::InvalidRequest(
                "No SFTP credentials yet — call POST /devices/{id}/sftp/request first".to_string(),
            ))
        })?;

    let status = state
        .plugins
        .sftp
        .mount_device(&device_id, &info)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "mounted": matches!(status.state, MountState::Mounted),
        "mount_state": state_label(&status.state),
        "mount_point": status.mount_point.as_ref().map(|p| p.display().to_string()),
        "error": match &status.state {
            MountState::Failed(msg) => Some(msg),
            _ => None,
        }
    }))))
}

/// Unmount the device's filesystem. 404 if nothing is currently
/// mounted. The mount point is server-determined; client-supplied
/// `?mount_point=` queries are ignored.
#[utoipa::path(
    delete,
    path = "/api/v1/devices/{device_id}/sftp/mount",
    tag = "sftp",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Mount released", body = ApiResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "No mount to release for this device", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn unmount_sftp(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let status = state.plugins.sftp.get_mount_status(&device_id);
    if !matches!(
        status.state,
        MountState::Mounted | MountState::Mounting | MountState::Failed(_)
    ) {
        return Err(api_err(Error::not_found("sftp_mount", Some(device_id))));
    }

    let result = state
        .plugins
        .sftp
        .unmount_device(&device_id)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "mounted": false,
        "mount_state": state_label(&result.state)
    }))))
}

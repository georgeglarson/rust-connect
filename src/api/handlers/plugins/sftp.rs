use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

use crate::plugins::sftp::MountState;
use crate::plugins::Plugin;

/// POST /devices/{id}/sftp/request acknowledgement. Adds the
/// human-readable `message` field the UI shows while it polls for the
/// SFTP credentials; the shared `SentResponse` does not carry that.
#[derive(Debug, Serialize, ToSchema)]
pub struct SftpRequestResponse {
    pub device_id: String,
    pub sent: bool,
    pub message: String,
}

/// GET /devices/{id}/sftp. The connection block (`ip`, `port`, `user`,
/// `path`, `multi_paths`, `path_names`) is omitted when the device has
/// not yet sent credentials — the mount block (`mounted`, `mount_point`,
/// `mount_state`) is always present so the UI can render "not mounted"
/// honestly rather than 404-ing.
#[derive(Debug, Serialize, ToSchema)]
pub struct SftpInfoResponse {
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub multi_paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub path_names: Vec<String>,
    pub available: bool,
    pub mounted: bool,
    /// Always present (`null` when nothing is mounted): the pre-typed
    /// literal emitted it unconditionally and the UI reads it.
    pub mount_point: Option<String>,
    pub mount_state: String,
}

/// POST /devices/{id}/sftp/mount. `error` is set only when the mount
/// attempt failed (mirrors `mount_sftp`'s `Failed(msg)` state). `mount_point`
/// is always present (null on failure, the resolved path on success)
/// because the legacy handler serialised it unconditionally.
#[derive(Debug, Serialize, ToSchema)]
pub struct SftpMountResponse {
    pub device_id: String,
    pub mounted: bool,
    pub mount_state: String,
    pub mount_point: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// DELETE /devices/{id}/sftp/mount. Always reports `mounted = false` on
/// success.
#[derive(Debug, Serialize, ToSchema)]
pub struct SftpUnmountResponse {
    pub device_id: String,
    pub mounted: bool,
    pub mount_state: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/sftp/request",
    tag = "sftp",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "SFTP request sent to device", body = SftpRequestResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_sftp(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<SftpRequestResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(SftpRequestResponse {
        device_id,
        sent: true,
        message:
            "SFTP session requested. Poll GET /devices/{device_id}/sftp for connection details."
                .to_string(),
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/sftp",
    tag = "sftp",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get SFTP connection info", body = SftpInfoResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or SFTP not available", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_sftp_info(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<SftpInfoResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let mount = state.plugins.sftp.get_mount_status(&device_id);
    let mounted = matches!(mount.state, MountState::Mounted);
    let mount_point_str = mount.mount_point.as_ref().map(|p| p.display().to_string());

    match state.plugins.sftp.get_connection(&device_id) {
        Some(info) => Ok(Json(ApiResponse::ok(SftpInfoResponse {
            device_id,
            ip: Some(info.ip),
            port: Some(info.port),
            user: Some(info.user),
            path: Some(info.path),
            multi_paths: info.multi_paths,
            path_names: info.path_names,
            available: true,
            mounted,
            mount_point: mount_point_str,
            mount_state: state_label(&mount.state).to_string(),
        }))),
        None => {
            // Even without credentials, surface the mount state so the UI
            // can render "not mounted" honestly rather than 404-ing.
            if mounted {
                Ok(Json(ApiResponse::ok(SftpInfoResponse {
                    device_id,
                    ip: None,
                    port: None,
                    user: None,
                    path: None,
                    multi_paths: Vec::new(),
                    path_names: Vec::new(),
                    available: false,
                    mounted: true,
                    mount_point: mount_point_str,
                    mount_state: state_label(&mount.state).to_string(),
                })))
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
        (status = 200, description = "Mount state (mounted or already mounted)", body = SftpMountResponseWrapper),
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
) -> Result<Json<ApiResponse<SftpMountResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(SftpMountResponse {
        device_id,
        mounted: matches!(status.state, MountState::Mounted),
        mount_state: state_label(&status.state).to_string(),
        mount_point: status.mount_point.as_ref().map(|p| p.display().to_string()),
        error: match &status.state {
            MountState::Failed(msg) => Some(msg.clone()),
            _ => None,
        },
    })))
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
        (status = 200, description = "Mount released", body = SftpUnmountResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "No mount to release for this device", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn unmount_sftp(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<SftpUnmountResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(SftpUnmountResponse {
        device_id,
        mounted: false,
        mount_state: state_label(&result.state).to_string(),
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_sftp_info_response_with_credentials_matches_legacy_shape() {
        let response = SftpInfoResponse {
            device_id: "phone-1".to_string(),
            ip: Some("10.0.0.2".to_string()),
            port: Some(1739),
            user: Some("kdeconnect".to_string()),
            path: Some("/storage/emulated/0".to_string()),
            multi_paths: vec!["/a".to_string()],
            path_names: vec!["Phone".to_string()],
            available: true,
            mounted: true,
            mount_point: Some("/var/lib/rust-connect/mounts/sftp-phone-1".to_string()),
            mount_state: "mounted".to_string(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "ip": "10.0.0.2",
            "port": 1739,
            "user": "kdeconnect",
            "path": "/storage/emulated/0",
            "multi_paths": ["/a"],
            "path_names": ["Phone"],
            "available": true,
            "mounted": true,
            "mount_point": "/var/lib/rust-connect/mounts/sftp-phone-1",
            "mount_state": "mounted"
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_sftp_info_response_mounted_no_credentials_omits_connection_block() {
        let response = SftpInfoResponse {
            device_id: "phone-1".to_string(),
            ip: None,
            port: None,
            user: None,
            path: None,
            multi_paths: Vec::new(),
            path_names: Vec::new(),
            available: false,
            mounted: true,
            mount_point: Some("/var/lib/rust-connect/mounts/sftp-phone-1".to_string()),
            mount_state: "mounted".to_string(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "available": false,
            "mounted": true,
            "mount_point": "/var/lib/rust-connect/mounts/sftp-phone-1",
            "mount_state": "mounted"
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_sftp_request_response_matches_legacy_shape() {
        let response = SftpRequestResponse {
            device_id: "phone-1".to_string(),
            sent: true,
            message: "SFTP session requested.".to_string(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "sent": true,
            "message": "SFTP session requested."
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_sftp_mount_response_success_matches_legacy_shape() {
        let response = SftpMountResponse {
            device_id: "phone-1".to_string(),
            mounted: true,
            mount_state: "mounted".to_string(),
            mount_point: Some("/var/lib/rust-connect/mounts/sftp-phone-1".to_string()),
            error: None,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "mounted": true,
            "mount_state": "mounted",
            "mount_point": "/var/lib/rust-connect/mounts/sftp-phone-1"
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_sftp_mount_response_failure_includes_error() {
        let response = SftpMountResponse {
            device_id: "phone-1".to_string(),
            mounted: false,
            mount_state: "failed".to_string(),
            mount_point: None,
            error: Some("permission denied".to_string()),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "mounted": false,
            "mount_state": "failed",
            "mount_point": null,
            "error": "permission denied"
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_sftp_unmount_response_matches_legacy_shape() {
        let response = SftpUnmountResponse {
            device_id: "phone-1".to_string(),
            mounted: false,
            mount_state: "unmounted".to_string(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "mounted": false,
            "mount_state": "unmounted"
        });
        assert_eq!(typed, legacy);
    }
}

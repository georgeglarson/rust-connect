// utoipa `body = …` resolves schema names, not paths; the imports keep the
// names in scope for readers.
#[allow(unused_imports)]
use crate::api::types::{GenericResponse, RemoteCommandsResponseWrapper};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    api::{
        extractors::validate_device_id,
        types::{ApiError, ApiResponse},
    },
    app::AppState,
    plugins::remotecommands::RemoteCommand,
    protocol::types::Packet,
};

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RemoteCommandsResponse {
    pub commands: HashMap<String, RemoteCommand>,
    /// The peer accepts an add-command request (kdeconnect-kde
    /// plugins/runcommand/runcommandplugin.cpp:165).
    pub can_add_command: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/remotecommands",
    tag = "remotecommands",
    responses(
        (status = 200, description = "List of remote commands", body = RemoteCommandsResponseWrapper),
        (status = 400, description = "Invalid device ID", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or not connected", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_remotecommands(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<RemoteCommandsResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(crate::api::extractors::api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(crate::api::extractors::api_err(
            crate::utils::errors::Error::DeviceNotFound(device_id),
        ));
    }

    let commands = state
        .plugins
        .remotecommands
        .get_commands(&device_id)
        .unwrap_or_default();

    let can_add_command = state.plugins.remotecommands.can_add_command(&device_id);

    Ok(Json(ApiResponse::ok(RemoteCommandsResponse {
        commands,
        can_add_command,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/remotecommands/{key}/trigger",
    tag = "remotecommands",
    responses(
        (status = 200, description = "Command triggered", body = GenericResponse),
        (status = 400, description = "Invalid device ID", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or not connected", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn trigger_remotecommand(
    State(state): State<Arc<AppState>>,
    Path((device_id, key)): Path<(String, String)>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(crate::api::extractors::api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(crate::api::extractors::api_err(
            crate::utils::errors::Error::DeviceNotFound(device_id),
        ));
    }

    let payload = serde_json::json!({
        "key": key,
    });
    let packet = Packet::new("kdeconnect.runcommand.request".to_string(), payload);

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(crate::api::extractors::api_err)?;

    Ok(Json(ApiResponse::ok(
        serde_json::json!({ "status": "triggered" }),
    )))
}

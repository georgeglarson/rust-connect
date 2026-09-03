use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/contacts/sync",
    tag = "contacts",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Contacts sync requested from device", body = GenericResponse),
        (status = 400, description = "Invalid request or device not connected", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn sync_contacts(
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
    // plugins/contacts/contactsplugin.cpp:169-176. The phone answers with
    // response_uids_timestamps, and the plugin then pulls vCards for new or
    // changed uids automatically.
    let packet = state.plugins.contacts.request_all_uids_timestamps();
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "sent": true,
        "message": "Contacts sync requested. Contacts will appear once the device responds."
    }))))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/contacts",
    tag = "contacts",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Stored contacts for the device", body = GenericResponse),
        (status = 400, description = "Invalid device id", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_contacts(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let contacts = state.plugins.contacts.get_contacts(&device_id);
    let count = contacts.len();

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "contacts": contacts,
        "count": count,
    }))))
}

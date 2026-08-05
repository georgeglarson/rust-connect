use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/volume",
    tag = "volume",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    request_body = VolumeControlRequest,
    responses(
        (status = 200, description = "Volume control command sent to device", body = ApiResponse),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn set_volume(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<VolumeControlRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let mut packet_body = serde_json::json!({
        "name": body.name
    });

    if let Some(volume) = body.volume {
        packet_body["volume"] = serde_json::json!(volume);
    }

    if let Some(muted) = body.muted {
        packet_body["muted"] = serde_json::json!(muted);
    }

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.systemvolume.request".to_string(),
        packet_body,
    );

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "volume": body.volume,
        "muted": body.muted,
        "sent": true
    }))))
}

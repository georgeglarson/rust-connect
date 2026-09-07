use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

/// `volume` and `muted` mirror the request body's optional fields: the
/// desktop asked for the volume to be at integer `volume` (`null` means
/// "don't touch volume") and the muted flag to be `muted` (`null` means
/// "don't touch muted"). The targeted sink `name` goes on the wire packet
/// only; the response never carried it.
#[derive(Debug, Serialize, ToSchema)]
pub struct VolumeControlSentResponse {
    pub device_id: String,
    /// Echo of the request; `null` when the caller did not set it, as
    /// the legacy literal emitted. The legacy body carried no `name`.
    pub volume: Option<i64>,
    pub muted: Option<bool>,
    pub sent: bool,
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/volume",
    tag = "volume",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    request_body = VolumeControlRequest,
    responses(
        (status = 200, description = "Volume control command sent to device", body = VolumeControlSentResponseWrapper),
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
) -> Result<Json<ApiResponse<VolumeControlSentResponse>>, (axum::http::StatusCode, Json<ApiError>)>
{
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

    Ok(Json(ApiResponse::ok(VolumeControlSentResponse {
        device_id,
        volume: body.volume,
        muted: body.muted,
        sent: true,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_volume_control_sent_response_matches_legacy_shape() {
        let response = VolumeControlSentResponse {
            device_id: "phone-1".to_string(),
            volume: Some(50),
            muted: Some(false),
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "volume": 50,
            "muted": false,
            "sent": true
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_volume_control_sent_response_emits_null_for_unset_controls() {
        let response = VolumeControlSentResponse {
            device_id: "phone-1".to_string(),
            volume: Some(50),
            muted: None,
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "volume": 50,
            "muted": null,
            "sent": true
        });
        assert_eq!(typed, legacy);
    }
}

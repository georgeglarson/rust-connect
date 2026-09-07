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
/// desktop asked for the volume to be at integer `volume` (None means
/// "don't touch volume") and the muted flag to be `muted` (None means
/// "don't touch muted"). `name` is the sink the desktop targeted — used
/// to disambiguate when the device has multiple players.
#[derive(Debug, Serialize, ToSchema)]
pub struct VolumeControlSentResponse {
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
        name: if body.name.is_empty() {
            None
        } else {
            Some(body.name)
        },
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
            name: Some("speaker".to_string()),
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "volume": 50,
            "muted": false,
            "name": "speaker",
            "sent": true
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_volume_control_sent_response_omits_empty_name() {
        // body.name defaults to "" in VolumeControlRequest; the legacy
        // json! literal serialised it as "", but the spec's intent is
        // "if the caller didn't pick a sink, don't surface an empty
        // string in the response" — wire-side the packet still carries
        // the empty string.
        let response = VolumeControlSentResponse {
            device_id: "phone-1".to_string(),
            volume: Some(50),
            muted: None,
            name: None,
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "volume": 50,
            "sent": true
        });
        assert_eq!(typed, legacy);
    }
}

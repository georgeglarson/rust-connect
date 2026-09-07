use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

/// Snake-case API view of [`crate::plugins::battery::BatteryInfo`].
///
/// The wire struct is `camelCase` (the KDE Connect protocol's envelope
/// casing); the API envelope is `snake_case` (every other response key on
/// the surface), so the conversion happens here, not in the wire type.
/// The conversion is pinned by `test_battery_response_matches_legacy_shape`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BatteryResponse {
    pub current_charge: i32,
    pub is_charging: bool,
    pub threshold_event: i32,
}

impl From<&crate::plugins::battery::BatteryInfo> for BatteryResponse {
    fn from(info: &crate::plugins::battery::BatteryInfo) -> Self {
        Self {
            current_charge: info.current_charge,
            is_charging: info.is_charging,
            threshold_event: info.threshold_event,
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/battery",
    tag = "battery",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get battery info from device", body = BatteryResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or no battery data", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_battery(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<BatteryResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    match state.plugins.battery.get_battery(&device_id) {
        Some(info) => Ok(Json(ApiResponse::ok(BatteryResponse::from(&info)))),
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
        (status = 200, description = "Battery request sent", body = SentResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_device_battery(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<SentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(SentResponse {
        device_id,
        sent: true,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_battery_response_matches_legacy_shape() {
        // Pin: the typed struct must serialize to the same JSON the
        // json! literal used to produce. If this drifts, the spec is
        // describing one shape and the wire another.
        let info = crate::plugins::battery::BatteryInfo {
            current_charge: 85,
            is_charging: true,
            threshold_event: 0,
        };
        let response = BatteryResponse::from(&info);
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "current_charge": 85,
            "is_charging": true,
            "threshold_event": 0,
        });
        assert_eq!(typed, legacy);
    }
}

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectivityResponse {
    pub device_id: String,
    pub signal_strength: i32,
    pub network_type: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/connectivity",
    tag = "connectivity",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get connectivity report from device", body = ConnectivityResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or no connectivity data", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_connectivity(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<ConnectivityResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    match state.plugins.connectivity.get_report(&device_id) {
        Some(report) => Ok(Json(ApiResponse::ok(ConnectivityResponse {
            device_id,
            signal_strength: report.signal_strength,
            network_type: report.network_type,
        }))),
        None => Err(api_err(Error::not_found(
            "connectivity_report",
            Some(device_id),
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_connectivity_response_matches_legacy_shape() {
        let response = ConnectivityResponse {
            device_id: "phone-1".to_string(),
            signal_strength: 3,
            network_type: "LTE".to_string(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "signal_strength": 3,
            "network_type": "LTE",
        });
        assert_eq!(typed, legacy);
    }
}

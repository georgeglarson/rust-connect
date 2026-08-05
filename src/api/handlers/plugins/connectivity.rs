use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/connectivity",
    tag = "connectivity",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get connectivity report from device", body = ApiResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or no connectivity data", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_connectivity(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    match state.plugins.connectivity.get_report(&device_id) {
        Some(report) => Ok(Json(ApiResponse::ok(serde_json::json!({
            "device_id": device_id,
            "signal_strength": report.signal_strength,
            "network_type": report.network_type,
        })))),
        None => Err(api_err(Error::not_found(
            "connectivity_report",
            Some(device_id),
        ))),
    }
}

use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/telephony",
    tag = "telephony",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get telephony events from device", body = ApiResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_telephony(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let calls = state.plugins.telephony.get_calls(&device_id);
    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "calls": calls,
    }))))
}

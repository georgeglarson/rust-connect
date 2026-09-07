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
pub struct FindMyPhoneResponse {
    pub device_id: String,
    pub sent: bool,
    pub message: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/findmyphone",
    tag = "findmyphone",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Ring request sent to device", body = FindMyPhoneResponseWrapper),
        (status = 400, description = "Invalid request or device not connected", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn find_my_phone(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<FindMyPhoneResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    // Empty body per upstream: kdeconnect-kde
    // plugins/findmyphone/findmyphoneplugin.cpp:17-21, GSConnect
    // src/service/plugins/findmyphone.js:93-98.
    let packet = state.plugins.findmyphone.ring_request();
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(FindMyPhoneResponse {
        device_id,
        sent: true,
        message: "Ring request sent. The device will ring until dismissed.".to_string(),
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_find_myphone_response_matches_legacy_shape() {
        let response = FindMyPhoneResponse {
            device_id: "phone-1".to_string(),
            sent: true,
            message: "Ring request sent.".to_string(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "sent": true,
            "message": "Ring request sent."
        });
        assert_eq!(typed, legacy);
    }
}

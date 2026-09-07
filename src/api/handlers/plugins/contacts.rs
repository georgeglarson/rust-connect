use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;
use crate::plugins::contacts::Contact;

/// POST /devices/{id}/contacts/sync — adds the human-readable `message`
/// the UI shows while it waits for the phone's vCard reply; the shared
/// `SentResponse` does not carry that.
#[derive(Debug, Serialize, ToSchema)]
pub struct ContactsSyncResponse {
    pub device_id: String,
    pub sent: bool,
    pub message: String,
}

/// GET /devices/{id}/contacts — stored contacts the desktop has
/// already pulled from the phone.
#[derive(Debug, Serialize, ToSchema)]
pub struct ContactsListResponse {
    pub device_id: String,
    pub contacts: Vec<Contact>,
    pub count: usize,
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/contacts/sync",
    tag = "contacts",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Contacts sync requested from device", body = ContactsSyncResponseWrapper),
        (status = 400, description = "Invalid request or device not connected", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn sync_contacts(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<ContactsSyncResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(ContactsSyncResponse {
        device_id,
        sent: true,
        message: "Contacts sync requested. Contacts will appear once the device responds."
            .to_string(),
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/contacts",
    tag = "contacts",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Stored contacts for the device", body = ContactsListResponseWrapper),
        (status = 400, description = "Invalid device id", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_contacts(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<ContactsListResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let contacts = state.plugins.contacts.get_contacts(&device_id);
    let count = contacts.len();

    Ok(Json(ApiResponse::ok(ContactsListResponse {
        device_id,
        contacts,
        count,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_contacts_sync_response_matches_legacy_shape() {
        let response = ContactsSyncResponse {
            device_id: "phone-1".to_string(),
            sent: true,
            message: "Contacts sync requested.".to_string(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "sent": true,
            "message": "Contacts sync requested."
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_contacts_list_response_matches_legacy_shape() {
        let response = ContactsListResponse {
            device_id: "phone-1".to_string(),
            contacts: Vec::new(),
            count: 0,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "contacts": [],
            "count": 0
        });
        assert_eq!(typed, legacy);
    }
}
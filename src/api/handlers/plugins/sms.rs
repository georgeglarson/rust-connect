use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::plugins::sms::{SmsMessage, SmsThread};
use crate::utils::errors::Error;

/// GET /devices/{id}/sms/threads — paginated thread list.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsThreadsResponse {
    pub device_id: String,
    pub threads: Vec<SmsThread>,
    pub total: usize,
    pub page: usize,
    pub limit: usize,
}

/// GET /devices/{id}/sms/threads/{thread_id} — single thread messages.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsThreadResponse {
    pub device_id: String,
    pub thread_id: i64,
    pub messages: Vec<SmsMessage>,
    pub total: usize,
}

/// POST /devices/{id}/sms/send — echoed back the SMS the desktop asked
/// the phone to send. `phoneNumber` and `messageBody` are camelCase to
/// match the kdeconnect.sms.request body the phone receives
/// (see `send_sms`'s outgoing packet) — that's the wire convention.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsSentResponse {
    pub device_id: String,
    #[serde(rename = "phoneNumber")]
    pub phone_number: String,
    #[serde(rename = "messageBody")]
    pub message_body: String,
    pub sent: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/sms/threads",
    tag = "sms",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("page" = Option<usize>, Query, description = "Page number (default 1)"),
        ("limit" = Option<usize>, Query, description = "Maximum number of threads to return (default 50)")
    ),
    responses(
        (status = 200, description = "Get SMS threads from device", body = SmsThreadsResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_sms_threads(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<ApiResponse<SmsThreadsResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let page: usize = params.get("page").and_then(|s| s.parse().ok()).unwrap_or(1);
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let all_threads = state.plugins.sms.get_threads(&device_id);
    let total = all_threads.len();
    let start = (page.saturating_sub(1)) * limit;
    let _end = start.saturating_add(limit).min(total);
    let threads: Vec<_> = all_threads.into_iter().skip(start).take(limit).collect();

    Ok(Json(ApiResponse::ok(SmsThreadsResponse {
        device_id,
        threads,
        total,
        page,
        limit,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/sms/request",
    tag = "sms",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "SMS threads request sent", body = SentResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_sms_threads(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<SentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let packet = conversations_request_packet();
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

/// Ask the phone for its conversation list.
///
/// Must NOT be `kdeconnect.sms.request`. On the phone that type is the
/// send-an-SMS path: `SMSPlugin.kt:271-286` reads `messageBody` and `addresses`
/// off the packet and calls `sendMessage(...)`. Asking for conversations with it
/// is at best a phone-side exception and at worst an outbound text.
/// `kdeconnect.sms.request_conversations` is the request-side type
/// (`SMSPlugin.kt:365`, in `supportedPacketTypes`).
fn conversations_request_packet() -> crate::protocol::types::Packet {
    crate::protocol::types::Packet::new(
        "kdeconnect.sms.request_conversations".to_string(),
        serde_json::json!({}),
    )
}

#[cfg(test)]
mod conversations_request_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_conversations_request_does_not_use_the_send_packet_type() {
        let packet = conversations_request_packet();
        assert_ne!(
            packet.packet_type, "kdeconnect.sms.request",
            "kdeconnect.sms.request makes the phone SEND an SMS (SMSPlugin.kt:284 \
             sendMessage). It must never be used to request conversations."
        );
        assert_eq!(packet.packet_type, "kdeconnect.sms.request_conversations");
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/sms/threads/{thread_id}",
    tag = "sms",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("thread_id" = i64, Path, description = "SMS thread ID")
    ),
    responses(
        (status = 200, description = "Get SMS thread messages", body = SmsThreadResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_sms_thread(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((device_id, thread_id)): axum::extract::Path<(String, i64)>,
) -> Result<Json<ApiResponse<SmsThreadResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let messages = state.plugins.sms.get_thread(&device_id, thread_id);
    Ok(Json(ApiResponse::ok(SmsThreadResponse {
        device_id,
        thread_id,
        total: messages.len(),
        messages,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/sms/send",
    tag = "sms",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    request_body = SendSmsRequest,
    responses(
        (status = 200, description = "SMS sent to device", body = SmsSentResponseWrapper),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_sms(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<SendSmsRequest>,
) -> Result<Json<ApiResponse<SmsSentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.sms.request".to_string(),
        serde_json::json!({
            "version": 2,
            "addresses": [{"address": body.phone_number}],
            "messageBody": body.message_body
        }),
    );

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(SmsSentResponse {
        device_id,
        phone_number: body.phone_number,
        message_body: body.message_body,
        sent: true,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_sms_threads_response_matches_legacy_shape() {
        let response = SmsThreadsResponse {
            device_id: "phone-1".to_string(),
            threads: Vec::new(),
            total: 0,
            page: 1,
            limit: 50,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "threads": [],
            "total": 0,
            "page": 1,
            "limit": 50
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_sms_thread_response_matches_legacy_shape() {
        let response = SmsThreadResponse {
            device_id: "phone-1".to_string(),
            thread_id: 42,
            messages: Vec::new(),
            total: 0,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "thread_id": 42,
            "messages": [],
            "total": 0
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_sms_sent_response_uses_camel_case_to_match_wire() {
        let response = SmsSentResponse {
            device_id: "phone-1".to_string(),
            phone_number: "+15551234567".to_string(),
            message_body: "hi".to_string(),
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "phoneNumber": "+15551234567",
            "messageBody": "hi",
            "sent": true
        });
        assert_eq!(typed, legacy);
    }
}

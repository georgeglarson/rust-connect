use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::plugins::notification::NotificationEntry;
use crate::utils::errors::Error;

/// GET /api/v1/notifications — paginated history across all (or one)
/// device. `device_id` lives in the query string, not the path, and is
/// optional, so this response has no `device_id` field; per-entry
/// `device_id` lives on each `NotificationEntry`.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationsListResponse {
    pub notifications: Vec<NotificationEntry>,
    pub total: usize,
    pub page: usize,
    pub limit: usize,
}

/// POST /api/v1/devices/{id}/notification — confirms the desktop pushed
/// a notification. `notification_id` is the agent-generated server id
/// the caller will later use for dismiss.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationSentResponse {
    pub device_id: String,
    pub notification_id: String,
    pub sent: bool,
}

/// POST /api/v1/devices/{id}/notification/{nid}/reply — echoes back the
/// reply text so the UI can confirm what was sent.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationReplyResponse {
    pub device_id: String,
    pub notification_id: String,
    pub message: String,
    pub sent: bool,
}

/// POST /api/v1/devices/{id}/notification/{nid}/action — confirms the
/// desktop triggered one of the phone's declared actions.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationActionTriggeredResponse {
    pub device_id: String,
    pub notification_id: String,
    pub action: String,
    pub sent: bool,
}

/// POST /api/v1/devices/{id}/notification/{nid}/dismiss — confirms the
/// phone got the cancel packet, and whether the desktop's local history
/// still held the entry (a UI test would want both signals).
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationDismissedResponse {
    pub device_id: String,
    pub notification_id: String,
    pub sent: bool,
    pub removed_from_history: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/notifications",
    tag = "notifications",
    params(
        ("device_id" = Option<String>, Query, description = "Filter by device ID"),
        Pagination,
    ),
    responses(
        (status = 200, description = "Notification history", body = NotificationsListResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_notifications(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<ApiResponse<NotificationsListResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    let device_id = params.get("device_id").map(|s| s.as_str());
    let pagination = Pagination::from_query(&params);
    let page = pagination.page();
    let limit = pagination.limit();

    let mut all_entries = state
        .plugins
        .notification
        .get_history(device_id, usize::MAX);
    all_entries.reverse();
    let total = all_entries.len();
    let start = (page.saturating_sub(1)) * limit;
    let _end = start.saturating_add(limit).min(total);
    let entries: Vec<_> = all_entries.into_iter().skip(start).take(limit).collect();

    Ok(Json(ApiResponse::ok(NotificationsListResponse {
        notifications: entries,
        total,
        page,
        limit,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/notification",
    tag = "notifications",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    request_body = SendNotificationRequest,
    responses(
        (status = 200, description = "Notification sent to device", body = NotificationSentResponseWrapper),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_notification(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<SendNotificationRequest>,
) -> Result<Json<ApiResponse<NotificationSentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let notification_id = format!("agent-{}", uuid::Uuid::new_v4());

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.notification".to_string(),
        serde_json::json!({
            "id": notification_id,
            "appName": body.app_name,
            "title": body.title,
            "text": body.text,
            "ticker": body.ticker.unwrap_or_else(|| format!("{}: {}", body.app_name, body.title)),
            "isClearable": body.is_clearable,
            "silent": body.silent
        }),
    );

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(NotificationSentResponse {
        device_id,
        notification_id,
        sent: true,
    })))
}

pub(crate) fn build_notification_reply_packet(
    reply_handle: &str,
    message: &str,
) -> crate::protocol::types::Packet {
    crate::protocol::types::Packet::new(
        "kdeconnect.notification.reply".to_string(),
        serde_json::json!({
            "requestReplyId": reply_handle,
            "message": message,
        }),
    )
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/notification/{notification_id}/reply",
    tag = "notifications",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("notification_id" = String, Path, description = "Notification ID to reply to")
    ),
    request_body = ReplyNotificationRequest,
    responses(
        (status = 200, description = "Notification reply sent to device", body = NotificationReplyResponseWrapper),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn reply_notification(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((device_id, notification_id)): axum::extract::Path<(String, String)>,
    Json(body): Json<ReplyNotificationRequest>,
) -> Result<Json<ApiResponse<NotificationReplyResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    // The phone keys its pendingIntents map by the handle it minted for this
    // notification, not by the notification id (NotificationsPlugin.kt:261, and
    // :534-535 for the lookup on the way back). Sending the notification id here
    // means the phone's lookup misses and the reply is silently dropped.
    let reply_handle = state
        .plugins
        .notification
        .reply_handle(&device_id, &notification_id)
        .ok_or_else(|| {
            api_err(Error::InvalidRequest(format!(
                "Notification {notification_id} is not repliable: the device sent no \
                 requestReplyId for it, which means it carried no reply action"
            )))
        })?;

    let packet = build_notification_reply_packet(&reply_handle, &body.message);

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(NotificationReplyResponse {
        device_id,
        notification_id,
        message: body.message,
        sent: true,
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/notification-icons/{icon_hash}",
    tag = "notifications",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("icon_hash" = String, Path, description = "MD5 hash of the icon payload (Android payloadHash)")
    ),
    responses(
        (status = 200, description = "Cached icon PNG", content_type = "image/png"),
        (status = 400, description = "Invalid device id or hash"),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "No cached icon for that hash"),
    ),
    security(("api_key" = []))
)]
pub async fn get_notification_icon(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((device_id, icon_hash)): axum::extract::Path<(String, String)>,
) -> Result<axum::response::Response, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;
    let Some(path) = state.plugins.notification.icon_path(&device_id, &icon_hash) else {
        return Err(api_err(Error::not_found(
            "notification icon",
            Some(format!("{device_id}/{icon_hash}")),
        )));
    };
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        api_err(Error::io(
            format!("Failed to read cached icon: {error}"),
            Some(path.display().to_string()),
        ))
    })?;
    let response = axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "image/png")
        .body(axum::body::Body::from(bytes))
        .map_err(|error| {
            api_err(Error::Internal(format!(
                "Failed to build icon response: {error}"
            )))
        })?;
    Ok(response)
}

pub(crate) fn build_notification_action_packet(
    notification_id: &str,
    action: &str,
) -> crate::protocol::types::Packet {
    crate::protocol::types::Packet::new(
        "kdeconnect.notification.action".to_string(),
        serde_json::json!({
            "key": notification_id,
            "action": action,
        }),
    )
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/notification/{notification_id}/action",
    tag = "notifications",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("notification_id" = String, Path, description = "Notification ID whose action should run")
    ),
    request_body = NotificationActionRequest,
    responses(
        (status = 200, description = "Notification action sent to device", body = NotificationActionTriggeredResponseWrapper),
        (status = 400, description = "Unknown action or disconnected device", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn activate_notification_action(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((device_id, notification_id)): axum::extract::Path<(String, String)>,
    Json(body): Json<NotificationActionRequest>,
) -> Result<Json<ApiResponse<NotificationActionTriggeredResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    if !state
        .plugins
        .notification
        .has_action(&device_id, &notification_id, &body.action)
    {
        return Err(api_err(Error::InvalidRequest(format!(
            "Notification {notification_id} does not expose action {:?}",
            body.action
        ))));
    }

    let packet = build_notification_action_packet(&notification_id, &body.action);
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(NotificationActionTriggeredResponse {
        device_id,
        notification_id,
        action: body.action,
        sent: true,
    })))
}

#[cfg(test)]
mod task_1_4_wire_tests {
    use super::*;

    #[test]
    fn test_notification_action_packet_matches_upstream_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/notification/action_request.json");
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read action fixture"))
                .expect("parse action fixture");

        let packet = build_notification_action_packet(
            "0|org.thoughtcrime.securesms|42|null|10123",
            "Mark as read",
        );
        assert_eq!(packet.packet_type, "kdeconnect.notification.action");
        assert_eq!(packet.body, expected);
    }

    #[test]
    fn test_notification_reply_packet_matches_upstream_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/notification/reply_request.json");
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read reply fixture"))
                .expect("parse reply fixture");

        let packet =
            build_notification_reply_packet("11111111-2222-3333-4444-555555555555", "see you soon");
        assert_eq!(packet.packet_type, "kdeconnect.notification.reply");
        assert_eq!(packet.body, expected);
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/notification/{notification_id}/dismiss",
    tag = "notifications",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("notification_id" = String, Path, description = "Notification ID to dismiss on the device")
    ),
    responses(
        (status = 200, description = "Dismiss request sent to device", body = NotificationDismissedResponseWrapper),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn dismiss_notification(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((device_id, notification_id)): axum::extract::Path<(String, String)>,
) -> Result<Json<ApiResponse<NotificationDismissedResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if notification_id.is_empty() {
        return Err(api_err(Error::InvalidRequest(
            "notification_id must not be empty".to_string(),
        )));
    }

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    // A desktop-initiated dismiss goes out as `cancel` on
    // kdeconnect.notification.request, carrying the notification id as a STRING.
    // kdeconnect-kde's dismissRequested builds exactly this packet:
    // `np.set<QString>(QStringLiteral("cancel"), internalId)`
    // (plugins/notifications/notificationsplugin.cpp:142-143). The phone reads it
    // back with `np.getString("cancel")` and closes its own notification
    // (kdeconnect-android .../plugins/notifications/NotificationsPlugin.kt:528-533).
    //
    // The id goes out exactly as the phone gave it to us. Upstream strips an
    // `org.kde.kdeconnect_tp::` prefix on the INBOUND side only
    // (notificationsplugin.cpp:41-43); nothing is stripped outbound.
    //
    // No `request` field: Android's handler chain tests `getBoolean("request")`
    // before `has("cancel")` (NotificationsPlugin.kt:522-533), so sending both
    // would take the resend branch and the dismiss would be ignored.
    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.notification.request".to_string(),
        serde_json::json!({ "cancel": notification_id }),
    );

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    // Erase our copy without waiting for the phone to confirm — upstream does the
    // same and says why: "we won't receive a response if we are out of sync and
    // this notification no longer exists" (notificationsplugin.cpp:146-150).
    let removed_from_history = state
        .plugins
        .notification
        .dismiss(&device_id, &notification_id);

    Ok(Json(ApiResponse::ok(NotificationDismissedResponse {
        device_id,
        notification_id,
        sent: true,
        removed_from_history,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_notification_sent_response_matches_legacy_shape() {
        let response = NotificationSentResponse {
            device_id: "phone-1".to_string(),
            notification_id: "agent-abc".to_string(),
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "notification_id": "agent-abc",
            "sent": true
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_notification_reply_response_matches_legacy_shape() {
        let response = NotificationReplyResponse {
            device_id: "phone-1".to_string(),
            notification_id: "agent-abc".to_string(),
            message: "hi".to_string(),
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "notification_id": "agent-abc",
            "message": "hi",
            "sent": true
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_notification_action_response_matches_legacy_shape() {
        let response = NotificationActionTriggeredResponse {
            device_id: "phone-1".to_string(),
            notification_id: "agent-abc".to_string(),
            action: "Mark as read".to_string(),
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "notification_id": "agent-abc",
            "action": "Mark as read",
            "sent": true
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_notification_dismissed_response_matches_legacy_shape() {
        let response = NotificationDismissedResponse {
            device_id: "phone-1".to_string(),
            notification_id: "agent-abc".to_string(),
            sent: true,
            removed_from_history: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "notification_id": "agent-abc",
            "sent": true,
            "removed_from_history": true
        });
        assert_eq!(typed, legacy);
    }
}
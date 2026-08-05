use axum::extract::{Path, State};
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::utils::errors::Error;

#[utoipa::path(
    get,
    path = "/api/v1/notifications",
    tag = "notifications",
    params(
        ("device_id" = Option<String>, Query, description = "Filter by device ID"),
        ("page" = Option<usize>, Query, description = "Page number (default 1)"),
        ("limit" = Option<usize>, Query, description = "Maximum number of notifications to return (default 50)")
    ),
    responses(
        (status = 200, description = "Notification history", body = serde_json::Value),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_notifications(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    let device_id = params.get("device_id").map(|s| s.as_str());
    let page: usize = params.get("page").and_then(|s| s.parse().ok()).unwrap_or(1);
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let mut all_entries = state
        .plugins
        .notification
        .get_history(device_id, usize::MAX);
    all_entries.reverse();
    let total = all_entries.len();
    let start = (page.saturating_sub(1)) * limit;
    let _end = start.saturating_add(limit).min(total);
    let entries: Vec<_> = all_entries.into_iter().skip(start).take(limit).collect();

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "notifications": entries,
        "total": total,
        "page": page,
        "limit": limit,
    }))))
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
        (status = 200, description = "Notification sent to device", body = ApiResponse),
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
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "notification_id": notification_id,
        "sent": true
    }))))
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
        (status = 200, description = "Notification reply sent to device", body = ApiResponse),
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
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.notification.reply".to_string(),
        serde_json::json!({
            "requestReplyId": reply_handle,
            "message": body.message
        }),
    );

    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "notification_id": notification_id,
        "message": body.message,
        "sent": true
    }))))
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
        (status = 200, description = "Dismiss request sent to device", body = ApiResponse),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn dismiss_notification(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((device_id, notification_id)): axum::extract::Path<(String, String)>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "notification_id": notification_id,
        "sent": true,
        "removed_from_history": removed_from_history
    }))))
}

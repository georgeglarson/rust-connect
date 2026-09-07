//! Device API handlers
//!
//! Single Responsibility: Handle device lifecycle operations.

use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::device::types::{Device, DeviceId, DeviceState};
use crate::utils::errors::Error;

async fn reconcile_rendered_connection_state(state: &AppState, device: &mut Device) {
    let live_connected = state.connection_manager.is_connected(&device.id).await;
    let rendered_state = if live_connected {
        DeviceState::Connected
    } else if device.state == DeviceState::Connected {
        DeviceState::Disconnected
    } else {
        device.state
    };

    if rendered_state != device.state {
        tracing::debug!(
            device_id = %device.id,
            registry_state = ?device.state,
            live_connected,
            rendered_state = ?rendered_state,
            event = "device_render_state_reconciled",
            "Rendering live connection state over stale registry state"
        );
        device.state = rendered_state;
    }
}

/// Build the `DeviceListResponse` shape that `/api/v1/devices` returns
/// in its `data` envelope — same overlay the SSE snapshot frame ships —
/// so the two paths cannot drift. Page + limit are taken from the
/// caller; SSE passes page 1 + `usize::MAX` to get the whole list in
/// one frame.
///
/// Pulled out of `list_devices` for the SSE handler to reuse (audit
/// 2026-09-06 item 5: snapshot on connect). Without it the snapshot
/// would reimplement the overlay loop and the two would drift the
/// first time someone added a new overlay field.
pub(crate) async fn render_device_list(
    state: &AppState,
    page: usize,
    limit: usize,
) -> DeviceListResponse {
    let devices = state.registry.list().await;
    let total = devices.len();
    // The registry can shrink between a client's page requests — an
    // out-of-range page must return empty, not panic the slice. Saturating
    // mul: page/limit are user-controlled, ordinary mul can overflow.
    let start = page.saturating_sub(1).saturating_mul(limit).min(total);
    let end = start.saturating_add(limit).min(total);
    let mut page_devices: Vec<DeviceSummary> = Vec::with_capacity(end - start);
    for device in &devices[start..end] {
        // The pairing store owns paired_at (same overlay as get_device);
        // without it the list forces N+1 detail fetches on every client.
        let mut device = device.clone();
        reconcile_rendered_connection_state(state, &mut device).await;
        device.reconcile_paired_at(state.pairing_handler.paired_since(&device.id).await);
        device.set_pair_state(
            state
                .pairing_handler
                .pair_state(&device.id)
                .await
                .as_api_str()
                .to_string(),
        );
        if let Ok(Some(key)) = state.pairing_handler.get_verification_key(&device.id).await {
            device.set_verification_key(key);
        }
        page_devices.push(DeviceSummary::from(&device));
    }

    DeviceListResponse {
        devices: page_devices,
        total,
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/devices",
    tag = "devices",
    params(Pagination),
    responses(
        (status = 200, description = "List all known devices", body = DevicesResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn list_devices(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<ApiResponse<DeviceListResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    let pagination = Pagination::from_query(&params);
    let page = pagination.page();
    let limit = pagination.limit();

    let response = render_device_list(&state, page, limit).await;
    Ok(Json(ApiResponse::ok(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device details", body = DeviceResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<crate::device::Device>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let mut device = state.registry.get(&device_id).await.map_err(api_err)?;

    if let Ok(Some(key)) = state.pairing_handler.get_verification_key(&device_id).await {
        device.set_verification_key(key);
    }

    device.set_pair_state(
        state
            .pairing_handler
            .pair_state(&device_id)
            .await
            .as_api_str()
            .to_string(),
    );

    // The pairing store owns paired_at; the record's own copy could not
    // self-correct because a reconnecting paired device never re-enters the
    // Paired lifecycle state. Same overlay shape as the verification key above.
    device.reconcile_paired_at(state.pairing_handler.paired_since(&device_id).await);
    reconcile_rendered_connection_state(&state, &mut device).await;

    Ok(Json(ApiResponse::ok(device)))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/pair",
    tag = "pairing",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Pairing initiated", body = PairResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
        (status = 409, description = "Pairing refused: no peer certificate presented", body = ApiError),
        (status = 503, description = "Pairing timeout", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn pair_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<PairResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if state.pairing_handler.has_incoming_request(&device_id).await {
        // Stage the peer cert so accept_pairing can persist it
        // (verify-before-write: this is the only write path).
        if let Some(cert_der) = state
            .connection_manager
            .get_peer_certificate(&device_id)
            .await
        {
            state
                .pairing_handler
                .set_pending_peer_cert(&device_id, cert_der)
                .await;
        }

        // Identity anchor pre-check (vk #1056, panel f07ea4a3): the
        // cert-anchor gate inside accept_pairing can REFUSE, and the
        // send below runs first — a post-send refusal would leave the
        // peer believing it is paired while we persist nothing (no
        // unwind exists on that path). Refuse here, before pair:true
        // goes on the wire.
        if !state.pairing_handler.has_identity_anchor(&device_id).await {
            let _ = state.pairing_handler.reject_pairing(&device_id).await;
            return Err(api_err(Error::PairingRejected(format!(
                "Refusing pairing with {}: no peer certificate (pending or pinned) was presented; \
                 cert-less pairings are not accepted",
                device_id
            ))));
        }

        // Android acceptPairing (PairingHandler.kt:174-190): the pairing
        // completes onSend success; a failed send (or an unreachable peer)
        // fails the pairing instead — send FIRST, mark paired after, or a
        // send failure leaves us paired while the peer isn't.
        let send_result = if state.connection_manager.is_connected(&device_id).await {
            let pair_pkt = crate::protocol::types::Packet::pair_response(true);
            state
                .connection_manager
                .send_packet(&device_id, &pair_pkt)
                .await
        } else {
            Err(Error::ConnectionError(format!(
                "Device {} is not connected",
                device_id
            )))
        };
        if let Err(e) = send_result {
            let _ = state.pairing_handler.reject_pairing(&device_id).await;
            return Err(api_err(e));
        }

        state
            .pairing_handler
            .accept_pairing(&device_id)
            .await
            .map_err(api_err)?;

        // late-pairing plugin init: pairing just completed on a connection that was UNPAIRED
        // at connect time, so no connect-time plugin notify ever fired —
        // send the init advertisements (runcommand list, …) now or the
        // phone sees nothing until reconnect.
        //
        // Spawned, like every other caller: the advertisement path waits
        // briefly for a live link, and holding the HTTP response open for
        // that wait would be reporting a pairing that is already complete.
        let state_clone = state.clone();
        let device_id_clone = device_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            state_clone.send_plugin_init_packets(&device_id_clone).await;
        });

        Ok(Json(ApiResponse::ok(PairResponse {
            device_id,
            status: "paired",
        })))
    } else {
        let pair_timestamp = state
            .pairing_handler
            .initiate_pairing(&device_id)
            .await
            .map_err(api_err)?;

        // Stage the peer cert for the pending request — same staging as the
        // accept branch above. Without it get_verification_key has no peer
        // pubkey and the SAS is unsurfaced for DAEMON-initiated pairing
        // (daemon-initiated pairing SAS: phone showed 00F8F3CE, API returned None, 2026-07-30).
        if let Some(cert_der) = state
            .connection_manager
            .get_peer_certificate(&device_id)
            .await
        {
            state
                .pairing_handler
                .set_pending_peer_cert(&device_id, cert_der)
                .await;
        }

        // Send the pair request packet to the device if connected — carrying
        // the SAME timestamp the handler recorded, or the two sides compute
        // different SAS keys.
        if state.connection_manager.is_connected(&device_id).await {
            let pair_pkt =
                crate::protocol::types::Packet::pair_request_with_timestamp(pair_timestamp);
            if let Err(e) = state
                .connection_manager
                .send_packet(&device_id, &pair_pkt)
                .await
            {
                tracing::warn!(
                    device_id = %device_id,
                    error = %e,
                    "Failed to send pair request packet"
                );
            }
        }

        Ok(Json(ApiResponse::ok(PairResponse {
            device_id,
            status: "pairing_initiated",
        })))
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/devices/{device_id}/unpair",
    tag = "pairing",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device unpaired", body = PairResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
        (status = 409, description = "Device is not paired (DEVICE_NOT_PAIRED)", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn unpair_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<PairResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    notify_peer_unpair(&state, &device_id).await;

    // Generation-scoped forced teardown (audit §C). The
    // registry-level guard stands down `notify_disconnected` while a
    // live generation exists for the device; unpair must NEVER skip the
    // trust-boundary teardown, so mirror `delete_device`'s pattern —
    // disconnect owns the slot, the guard then passes, plugins drop
    // what the device sent while trusted.
    if state.connection_manager.is_connected(&device_id).await {
        let generation = state.connection_manager.get_generation(&device_id).await;
        if let Some(gen) = generation {
            let _ = state.connection_manager.disconnect(&device_id, gen).await;
        }
    }

    // Unpair tears down the trust relationship; any SFTP credentials
    // and mount belong to the previous pairing. Drop them on the way
    // out so a fresh pairing starts clean.
    state.plugins.sftp.cleanup_device(&device_id).await;
    // B4 (2026-09-02 audit): every plugin drops what the device sent while
    // trusted (notification history and icons, lock state, …).
    state.plugin_registry.notify_disconnected(&device_id).await;

    // Idempotent: a peer-initiated `pair=false` may already have cleared
    // our local pair state by the time the harness's DELETE arrives — the
    // desired outcome (the device is unpaired) is already achieved, so
    // 200 is the honest response rather than 500. M2 surface (vk #991):
    // the test calls `kde_unpair` first; the rust side's pair_rejected_
    // unpair code path drops state before the harness's own DELETE
    // roundtrips, and the previous 500 broke the M2 dance.
    if state.pairing_handler.is_paired(&device_id).await {
        state
            .pairing_handler
            .unpair(&device_id)
            .await
            .map_err(api_err)?;
    }

    Ok(Json(ApiResponse::ok(PairResponse {
        device_id,
        status: "unpaired",
    })))
}

/// Best-effort `{"pair": false}` to a connected peer before a local unpair,
/// mirroring Android's `PairingHandler.unpair()` (PairingHandler.kt:213-221)
/// — a reachable peer must drop its side of the pairing too, but an
/// unreachable or dead link must never fail the unpair itself.
async fn notify_peer_unpair(state: &AppState, device_id: &DeviceId) {
    if !state.connection_manager.is_connected(device_id).await {
        return;
    }
    let unpair_pkt = crate::protocol::types::Packet::pair_response(false);
    if let Err(e) = state
        .connection_manager
        .send_packet(device_id, &unpair_pkt)
        .await
    {
        tracing::warn!(
            device_id = %device_id,
            error = %e,
            event = "unpair_notify_failed",
            "Failed to notify peer of unpair (pair=false)"
        );
    }
}

/// Acknowledgement for `POST /api/v1/ping`. Distinct from the shared
/// `SentResponse` so the spec names the surface — `data: { device_id, sent }`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PingSentResponse {
    pub device_id: String,
    pub sent: bool,
}

#[utoipa::path(
    post,
    path = "/api/v1/ping",
    tag = "devices",
    request_body = SendPingRequest,
    responses(
        (status = 200, description = "Ping sent to device", body = PingSentResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
        (status = 503, description = "Connection error", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_ping(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SendPingRequest>,
) -> Result<Json<ApiResponse<PingSentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&body.device_id).map_err(api_err)?;

    let packet = crate::protocol::types::Packet::ping();
    state
        .connection_manager
        .send_packet(&body.device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(PingSentResponse {
        device_id: body.device_id,
        sent: true,
    })))
}

/// Acknowledgement for `DELETE /api/v1/devices/{device_id}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceRemovedResponse {
    pub device_id: String,
    pub removed: bool,
}

#[utoipa::path(
    delete,
    path = "/api/v1/devices/{device_id}",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device removed", body = DeviceRemovedResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn delete_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<DeviceRemovedResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.registry.contains(&device_id).await {
        return Err(api_err(Error::not_found("device", Some(device_id))));
    }

    // Notify the peer BEFORE tearing the link down (same as unpair_device).
    notify_peer_unpair(&state, &device_id).await;

    // Clean up SFTP state before disconnect fires on_disconnected. The
    // disconnect path also runs cleanup via the plugin's on_disconnected,
    // but doing it here means the device's mount point is released even
    // if the link teardown races with the registry removal.
    state.plugins.sftp.cleanup_device(&device_id).await;

    if state.connection_manager.is_connected(&device_id).await {
        let generation = state.connection_manager.get_generation(&device_id).await;
        if let Some(gen) = generation {
            let _ = state.connection_manager.disconnect(&device_id, gen).await;
        }
    }

    let _ = state.pairing_handler.unpair(&device_id).await;
    let _ = state.registry.remove(&device_id).await;
    state.lifecycle.remove(&device_id).await;

    Ok(Json(ApiResponse::ok(DeviceRemovedResponse {
        device_id,
        removed: true,
    })))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ConnectDeviceRequest {
    /// `host:port` of the device's TCP listener (kdeconnect-android
    /// opens 1716 on the desktop-visible address).
    pub address: String,
}

/// Acknowledgement for `POST /api/v1/devices/{device_id}/connect`. The
/// returned `device_id` is the resolved identifier the daemon paired to;
/// the caller-supplied path parameter is treated as a hint, not an
/// authoritative identity.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceConnectedResponse {
    pub device_id: String,
    pub connected: bool,
    pub generation: u64,
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/connect",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    request_body = ConnectDeviceRequest,
    responses(
        (status = 200, description = "Connection established", body = DeviceConnectedResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn connect_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<ConnectDeviceRequest>,
) -> Result<Json<ApiResponse<DeviceConnectedResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is already connected".to_string(),
        )));
    }

    let addr: std::net::SocketAddr = body.address.parse().map_err(|_| {
        api_err(Error::InvalidRequest(format!(
            "Invalid address: {}",
            body.address
        )))
    })?;

    let identity = state
        .connection_manager
        .get_identity()
        .ok_or_else(|| api_err(Error::Internal("No device identity configured".to_string())))?;

    let (connected_id, generation) =
        crate::services::connection_orchestrator::connect_and_spawn_loop(state, identity, addr)
            .await
            .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(DeviceConnectedResponse {
        device_id: connected_id,
        connected: true,
        generation,
    })))
}

/// Acknowledgement for `POST /api/v1/devices/{device_id}/disconnect`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceDisconnectedResponse {
    pub device_id: String,
    pub disconnected: bool,
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/disconnect",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device disconnected", body = DeviceDisconnectedResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn disconnect_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<DeviceDisconnectedResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let generation = state.connection_manager.get_generation(&device_id).await;

    if let Some(gen) = generation {
        // Teardown only when the disconnect actually owned the link: a
        // same-cert redial between get_generation and disconnect makes it
        // return false, and the live replacement owns lifecycle/plugin
        // state (same ownership gate as run_packet_loop's exit arms).
        if let Ok(true) = state.connection_manager.disconnect(&device_id, gen).await {
            state
                .lifecycle
                .try_transition(&device_id, DeviceState::Disconnected)
                .await;
            state.plugin_registry.notify_disconnected(&device_id).await;
        }
    }

    Ok(Json(ApiResponse::ok(DeviceDisconnectedResponse {
        device_id,
        disconnected: true,
    })))
}

/// Body for `GET /api/v1/devices/{device_id}/state`. `state` and
/// `state_since` are nullable: a brand-new device never recorded any
/// transition has no rendered state to surface.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceStateResponse {
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_since: Option<DateTime<Utc>>,
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/state",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device state", body = DeviceStateResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_state(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<DeviceStateResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let device_state = state.lifecycle.get_state(&device_id).await.ok();
    let state_since = state.lifecycle.get_state_since(&device_id).await.ok();

    Ok(Json(ApiResponse::ok(DeviceStateResponse {
        device_id,
        state: device_state.map(|s| format!("{:?}", s)),
        state_since,
    })))
}

/// One connected device. The `generation` is the same monotonic counter
/// that `disconnect` carries: a redial bumps it, so the pair
/// `(device_id, generation)` names the specific link the daemon has
/// open right now.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectedDeviceEntry {
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectedDevicesResponse {
    pub connected_devices: Vec<ConnectedDeviceEntry>,
    pub count: usize,
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/connected",
    tag = "devices",
    responses(
        (status = 200, description = "List of connected devices", body = ConnectedDevicesResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn list_connected_devices(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<ConnectedDevicesResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    let device_ids = state.connection_manager.connected_device_ids().await;
    let mut devices = Vec::with_capacity(device_ids.len());
    for id in device_ids {
        let generation = state.connection_manager.get_generation(&id).await;
        devices.push(ConnectedDeviceEntry {
            device_id: id,
            generation,
        });
    }
    let count = devices.len();
    Ok(Json(ApiResponse::ok(ConnectedDevicesResponse {
        connected_devices: devices,
        count,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_ping_sent_response_matches_legacy_shape() {
        let resp = PingSentResponse {
            device_id: "phone-1".to_string(),
            sent: true,
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "sent": true,
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_device_removed_response_matches_legacy_shape() {
        let resp = DeviceRemovedResponse {
            device_id: "phone-1".to_string(),
            removed: true,
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "removed": true,
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_connect_device_request_parses() {
        let body: ConnectDeviceRequest = serde_json::from_str(r#"{"address":"10.0.0.2:1716"}"#)
            .expect("connect request must parse");
        assert_eq!(body.address, "10.0.0.2:1716");
    }

    #[test]
    fn test_device_connected_response_round_trip() {
        let resp = DeviceConnectedResponse {
            device_id: "phone-1".to_string(),
            connected: true,
            generation: 7,
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "connected": true,
            "generation": 7,
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_device_disconnected_response_matches_legacy_shape() {
        let resp = DeviceDisconnectedResponse {
            device_id: "phone-1".to_string(),
            disconnected: true,
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "disconnected": true,
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_device_state_response_omits_null_state() {
        let resp = DeviceStateResponse {
            device_id: "phone-1".to_string(),
            state: None,
            state_since: None,
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({ "device_id": "phone-1" });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_connected_devices_response_matches_legacy_shape() {
        let resp = ConnectedDevicesResponse {
            connected_devices: vec![ConnectedDeviceEntry {
                device_id: "phone-1".to_string(),
                generation: Some(3),
            }],
            count: 1,
        };
        let typed = serde_json::to_value(&resp).expect("typed serialization");
        let legacy = serde_json::json!({
            "connected_devices": [{
                "device_id": "phone-1",
                "generation": 3,
            }],
            "count": 1,
        });
        assert_eq!(typed, legacy);
    }
}

//! Device API handlers
//!
//! Single Responsibility: Handle device lifecycle operations.

use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

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

#[utoipa::path(
    get,
    path = "/api/v1/devices",
    tag = "devices",
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
    let page: usize = params.get("page").and_then(|s| s.parse().ok()).unwrap_or(1);
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

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
        reconcile_rendered_connection_state(&state, &mut device).await;
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

    Ok(Json(ApiResponse::ok(DeviceListResponse {
        devices: page_devices,
        total,
    })))
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

#[utoipa::path(
    post,
    path = "/api/v1/ping",
    tag = "devices",
    request_body = SendPingRequest,
    responses(
        (status = 200, description = "Ping sent to device", body = PingResponse),
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
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&body.device_id).map_err(api_err)?;

    let packet = crate::protocol::types::Packet::ping();
    state
        .connection_manager
        .send_packet(&body.device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": body.device_id,
        "sent": true
    }))))
}

#[utoipa::path(
    delete,
    path = "/api/v1/devices/{device_id}",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device removed", body = serde_json::Value),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn delete_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "removed": true,
    }))))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/connect",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Connection established", body = serde_json::Value),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn connect_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is already connected".to_string(),
        )));
    }

    let address = body
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| api_err(Error::InvalidRequest("address field required".to_string())))?;

    let addr: std::net::SocketAddr = address.parse().map_err(|_| {
        api_err(Error::InvalidRequest(format!(
            "Invalid address: {}",
            address
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

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": connected_id,
        "connected": true,
        "generation": generation,
    }))))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/disconnect",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device disconnected", body = serde_json::Value),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn disconnect_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
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

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "disconnected": true,
    }))))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/state",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Device state", body = serde_json::Value),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_state(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let device_state = state.lifecycle.get_state(&device_id).await.ok();
    let state_since = state.lifecycle.get_state_since(&device_id).await.ok();

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "state": device_state.map(|s| format!("{:?}", s)),
        "state_since": state_since.map(|t| t.to_rfc3339()),
    }))))
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/connected",
    tag = "devices",
    responses(
        (status = 200, description = "List of connected devices", body = serde_json::Value),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn list_connected_devices(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    let device_ids = state.connection_manager.connected_device_ids().await;
    let mut devices = Vec::new();
    for id in device_ids {
        let generation = state.connection_manager.get_generation(&id).await;
        devices.push(serde_json::json!({
            "device_id": id,
            "generation": generation,
        }));
    }

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "connected_devices": devices,
        "count": devices.len(),
    }))))
}

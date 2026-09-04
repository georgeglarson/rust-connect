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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! F-1: a local unpair must tell a reachable peer (`{"pair": false}`),
    //! like Android's PairingHandler.unpair() — and must still succeed when
    //! the peer is unreachable.

    use super::*;
    use crate::config::settings::AppSettings;
    use crate::protocol::{CertificateManager, ConnectionManager};

    const OUR_ID: &str = "clientaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PEER_ID: &str = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
        let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
        let state =
            Arc::new(AppState::new_without_input(settings).expect("Value expected to be present"));
        (state, temp_dir)
    }

    /// In-process TLS link: `state` connected (as OUR_ID) to a peer
    /// connection manager holding the server side (as PEER_ID).
    async fn connect_peer(
        state: &Arc<AppState>,
    ) -> (
        Arc<ConnectionManager>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let peer_temp = tempfile::TempDir::new().expect("Value expected to be present");
        let peer_certs = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
        peer_certs.init().expect("Value expected to be present");
        let peer_cm = Arc::new(
            ConnectionManager::new(peer_certs.clone()).expect("Value expected to be present"),
        );
        peer_cm.set_device_identity(PEER_ID, "Peer");
        peer_certs
            .ensure_certificate(PEER_ID, "Peer")
            .expect("Value expected to be present");
        state
            .cert_manager
            .ensure_certificate(OUR_ID, "Us")
            .expect("Value expected to be present");
        peer_certs
            .ensure_certificate(OUR_ID, "Us")
            .expect("Value expected to be present");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        let server_cm = peer_cm.clone();
        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            server_cm
                .accept_test(OUR_ID.to_string(), stream)
                .await
                .expect("Value expected to be present");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        state.connection_manager.set_device_identity(OUR_ID, "Us");
        state
            .connection_manager
            .connect(&PEER_ID.to_string(), addr)
            .await
            .expect("Value expected to be present");

        (peer_cm, server_handle, peer_temp)
    }

    #[tokio::test]
    async fn test_list_devices_renders_live_link_as_connected() {
        let (state, _t) = test_state();
        let (_peer_cm, server_handle, _pt) = connect_peer(&state).await;
        state
            .registry
            .upsert_device(crate::device::Device::new(
                PEER_ID.to_string(),
                "Peer".to_string(),
                crate::device::DeviceType::Phone,
                8,
            ))
            .await
            .expect("Value expected to be present");

        let response = list_devices(
            State(state.clone()),
            axum::extract::Query(std::collections::HashMap::new()),
        )
        .await
        .expect("Value expected to be present");

        assert_eq!(response.0.data.devices.len(), 1);
        assert_eq!(response.0.data.devices[0].state, DeviceState::Connected);
        assert_eq!(
            state
                .registry
                .get(&PEER_ID.to_string())
                .await
                .expect("Value expected to be present")
                .state,
            DeviceState::Discovered,
            "render-time reconciliation must not mutate the registry"
        );
        server_handle.abort();
    }

    #[tokio::test]
    async fn test_get_device_renders_dead_link_as_disconnected() {
        let (state, _t) = test_state();
        let mut device = crate::device::Device::new(
            PEER_ID.to_string(),
            "Peer".to_string(),
            crate::device::DeviceType::Phone,
            8,
        );
        device.state = DeviceState::Connected;
        state
            .registry
            .upsert_device(device)
            .await
            .expect("Value expected to be present");

        let response = get_device(State(state.clone()), Path(PEER_ID.to_string()))
            .await
            .expect("Value expected to be present");

        assert_eq!(response.0.data.state, DeviceState::Disconnected);
        assert_eq!(
            state
                .registry
                .get(&PEER_ID.to_string())
                .await
                .expect("Value expected to be present")
                .state,
            DeviceState::Connected,
            "render-time reconciliation must not mutate the registry"
        );
    }

    async fn pair_locally(state: &Arc<AppState>, device_id: &str) {
        state
            .pairing_handler
            .receive_pair_request(&device_id.to_string(), Some(1_700_000_000))
            .await
            .expect("Value expected to be present");
        // Stage a synthetic peer cert so the cert-anchor gate (vk #1056)
        // lets the accept through. Production code paths stage via
        // receive_pair_request_with_cert; this helper is the unit-test
        // surface and the cert is generated in a throwaway cert dir.
        let cert_der = make_test_peer_cert_der(device_id);
        state
            .pairing_handler
            .set_pending_peer_cert(&device_id.to_string(), cert_der)
            .await;
        state
            .pairing_handler
            .accept_pairing(&device_id.to_string())
            .await
            .expect("Value expected to be present");
        assert!(
            state
                .pairing_handler
                .is_paired(&device_id.to_string())
                .await
        );
    }

    /// Generate a peer cert DER for `device_id` in a throwaway cert dir.
    /// The cert is generated via the manager's existing
    /// `generate_certificate` so the resulting PEM round-trips through
    /// `store_peer_certificate` cleanly when the accept fires.
    fn make_test_peer_cert_der(device_id: &str) -> Vec<u8> {
        let cm_for_cert = std::sync::Arc::new(crate::protocol::crypto::CertificateManager::new(
            tempfile::TempDir::new()
                .expect("Value expected to be present")
                .path()
                .to_path_buf(),
        ));
        let (cert_pem, _) = cm_for_cert
            .generate_certificate(device_id, "Peer")
            .expect("Value expected to be present");
        openssl::x509::X509::from_pem(&cert_pem)
            .expect("Value expected to be present")
            .to_der()
            .expect("Value expected to be present")
    }
    #[tokio::test]
    async fn test_unpair_connected_peer_sends_pair_false() {
        let (state, _t) = test_state();
        let (peer_cm, server_handle, _pt) = connect_peer(&state).await;
        pair_locally(&state, PEER_ID).await;

        let result = unpair_device(State(state.clone()), Path(PEER_ID.to_string())).await;
        assert!(result.is_ok(), "unpair should succeed: {:?}", result.err());
        assert!(
            !state.pairing_handler.is_paired(&PEER_ID.to_string()).await,
            "local pairing state must be cleared"
        );

        // The reachable peer must have been told: {"pair": false}.
        let packet = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            peer_cm.recv_packet(&OUR_ID.to_string()),
        )
        .await
        .expect("pair=false must arrive on the live link")
        .expect("Value expected to be present");
        assert!(packet.is_pair());
        assert_eq!(
            packet.body.get("pair").and_then(|v| v.as_bool()),
            Some(false),
            "peer must receive a pair=false notification"
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_unpair_unreachable_device_still_succeeds() {
        let (state, _t) = test_state();
        let device_id = "unreachableaaaaaaaaaaaaaaaaaaaaaa".to_string();
        pair_locally(&state, &device_id).await;
        assert!(
            !state.connection_manager.is_connected(&device_id).await,
            "fixture: device must not be connected"
        );

        let result = unpair_device(State(state.clone()), Path(device_id.clone())).await;
        assert!(
            result.is_ok(),
            "unpair of an unreachable device must succeed: {:?}",
            result.err()
        );
        assert!(!state.pairing_handler.is_paired(&device_id).await);
    }

    /// Android acceptPairing (PairingHandler.kt:176-189): the pairing
    /// completes onSend success — over a live link the accept packet goes
    /// out and we end paired.
    #[tokio::test]
    async fn test_pair_accept_sends_response_then_marks_paired() {
        let (state, _t) = test_state();
        let (peer_cm, server_handle, _pt) = connect_peer(&state).await;
        state
            .pairing_handler
            .receive_pair_request(&PEER_ID.to_string(), Some(1_700_000_000))
            .await
            .expect("Value expected to be present");

        let result = pair_device(State(state.clone()), Path(PEER_ID.to_string())).await;
        assert!(
            result.is_ok(),
            "pair accept should succeed: {:?}",
            result.err()
        );
        assert!(
            state.pairing_handler.is_paired(&PEER_ID.to_string()).await,
            "a successful send completes the pairing"
        );

        // The peer must have received the accept BEFORE we marked paired.
        let packet = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            peer_cm.recv_packet(&OUR_ID.to_string()),
        )
        .await
        .expect("pair=true must arrive on the live link")
        .expect("Value expected to be present");
        assert!(packet.is_pair());
        assert_eq!(
            packet.body.get("pair").and_then(|v| v.as_bool()),
            Some(true)
        );

        server_handle.abort();
    }

    /// late-pairing plugin init: when pairing COMPLETES on an established connection, plugins
    /// must get their connect-time advertisement out (runcommand's command
    /// list, …) — the orchestrator/listener notify fires only for devices
    /// that were ALREADY paired at connect time, so a phone-initiated pair
    /// the user accepts here is the only chance until reconnect.
    #[tokio::test]
    async fn test_pair_accept_sends_plugin_init_packets() {
        let (state, _t) = test_state();
        state
            .initialize()
            .await
            .expect("Value expected to be present");
        let (peer_cm, server_handle, _pt) = connect_peer(&state).await;
        state
            .pairing_handler
            .receive_pair_request(&PEER_ID.to_string(), Some(1_700_000_000))
            .await
            .expect("Value expected to be present");

        let result = pair_device(State(state.clone()), Path(PEER_ID.to_string())).await;
        assert!(
            result.is_ok(),
            "pair accept should succeed: {:?}",
            result.err()
        );

        // First packet on the link is the pair accept…
        let packet = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            peer_cm.recv_packet(&OUR_ID.to_string()),
        )
        .await
        .expect("pair=true must arrive on the live link")
        .expect("Value expected to be present");
        assert!(packet.is_pair());

        // …then plugin init advertisements (on_connected fires on every
        // connect-and-paired path). Several plugins advertise; read until
        // runcommand's shows up. A single generous deadline instead of
        // per-window timeouts: the old 8×5s loop bailed on the FIRST empty
        // window, and under full-suite parallel load a packet can take >5s
        // to traverse the link (flaky ~1/8 in CI before the change).
        // Drain until the deadline; only a link error
        // stops the wait early.
        let mut saw_runcommand = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                deadline - tokio::time::Instant::now(),
                peer_cm.recv_packet(&OUR_ID.to_string()),
            )
            .await
            {
                Ok(Ok(pkt)) if pkt.packet_type == "kdeconnect.runcommand" => {
                    saw_runcommand = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(
            saw_runcommand,
            "plugin init packets must follow a completed pairing"
        );

        server_handle.abort();
    }

    /// daemon-initiated pairing SAS: a DAEMON-initiated pairing must surface the SAS too — the
    /// initiate branch stages the peer cert, so get_verification_key has a
    /// peer pubkey and get_device returns the key while the request is
    /// pending (previously None; the phone showed the SAS, we couldn't).
    #[tokio::test]
    async fn test_pair_initiate_surfaces_verification_key() {
        let (state, _t) = test_state();
        // get_verification_key reads the fixed own.crt identity — the test
        // state only has the id-keyed cert from connect_peer.
        state
            .cert_manager
            .ensure_own_certificate(OUR_ID, "Us")
            .expect("Value expected to be present");
        let (_peer_cm, server_handle, _pt) = connect_peer(&state).await;

        let result = pair_device(State(state.clone()), Path(PEER_ID.to_string())).await;
        assert!(
            result.is_ok(),
            "pair initiate should succeed: {:?}",
            result.err()
        );

        let key = state
            .pairing_handler
            .get_verification_key(&PEER_ID.to_string())
            .await
            .expect("Value expected to be present");
        assert!(
            key.as_ref().is_some_and(|k| k.len() == 8 && k.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())),
            "a pending daemon-initiated pairing must surface an 8-char uppercase-hex SAS, got {key:?}"
        );

        // And it reaches the API surface via get_device.
        state
            .registry
            .upsert_device(crate::device::Device::new(
                PEER_ID.to_string(),
                "Peer".to_string(),
                crate::device::DeviceType::Phone,
                8,
            ))
            .await
            .expect("Value expected to be present");
        let device = get_device(State(state.clone()), Path(PEER_ID.to_string()))
            .await
            .expect("Value expected to be present");
        assert_eq!(device.0.data.verification_key, key);

        server_handle.abort();
    }

    /// The failure leg: Android's onSend failure leaves BOTH sides unpaired.
    /// An unreachable peer must not leave us paired while the peer isn't.
    #[tokio::test]
    async fn test_pair_accept_unreachable_peer_does_not_mark_paired() {
        let (state, _t) = test_state();
        let (_peer_cm, server_handle, _pt) = connect_peer(&state).await;
        state
            .pairing_handler
            .receive_pair_request(&PEER_ID.to_string(), Some(1_700_000_000))
            .await
            .expect("Value expected to be present");

        // Drop the link so the accept packet cannot be sent.
        let generation = state
            .connection_manager
            .get_generation(&PEER_ID.to_string())
            .await
            .expect("Value expected to be present");
        state
            .connection_manager
            .disconnect(&PEER_ID.to_string(), generation)
            .await
            .expect("Value expected to be present");

        let result = pair_device(State(state.clone()), Path(PEER_ID.to_string())).await;
        assert!(
            result.is_err(),
            "accept with an unreachable peer must fail (Android: pairingFailed)"
        );
        assert!(
            !state.pairing_handler.is_paired(&PEER_ID.to_string()).await,
            "a failed send must not leave us paired while the peer isn't"
        );
        assert!(
            !state
                .pairing_handler
                .has_incoming_request(&PEER_ID.to_string())
                .await,
            "the pending request must be cleared (Android: state NotPaired)"
        );

        server_handle.abort();
    }

    /// disconnect_device against a REPLACED connection: the handler owns the
    /// current generation, so teardown must run (Disconnected lifecycle, link
    /// gone). The not-owned branch (disconnect returning false) is reachable
    /// only inside the get_generation→disconnect race window and is covered
    /// by the manager-level ownership tests.
    #[tokio::test]
    async fn test_disconnect_device_after_replacement_tears_down_current_link() {
        let (state, _t) = test_state();
        let (_peer1, server1, _pt1) = connect_peer(&state).await;
        // Same-cert redial: a second link replaces the first (new generation).
        let (_peer2, server2, _pt2) = connect_peer(&state).await;
        assert!(
            state
                .connection_manager
                .is_connected(&PEER_ID.to_string())
                .await
        );

        // Mark Connected, as the packet-loop path would have.
        let identity = crate::protocol::types::Identity::new(
            PEER_ID.to_string(),
            "Peer".to_string(),
            crate::device::types::DeviceType::Phone,
            vec![],
            vec![],
        );
        state
            .lifecycle
            .ensure_and_transition(&PEER_ID.to_string(), &identity, DeviceState::Connected)
            .await
            .expect("Value expected to be present");

        let result = disconnect_device(State(state.clone()), Path(PEER_ID.to_string())).await;
        assert!(
            result.is_ok(),
            "disconnect should succeed: {:?}",
            result.err()
        );
        assert!(
            !state
                .connection_manager
                .is_connected(&PEER_ID.to_string())
                .await,
            "the current link must be torn down"
        );
        assert!(
            state
                .lifecycle
                .get_state(&PEER_ID.to_string())
                .await
                .is_ok_and(|s| s == DeviceState::Disconnected),
            "owning the disconnect must mark the device Disconnected"
        );

        server1.abort();
        server2.abort();
    }

    /// Audit §C, R5: the forced path. Unpair is a trust-boundary
    /// teardown — the registry-level guard stands down
    /// `notify_disconnected` while a live generation exists for the
    /// device; without the ordering fix in `unpair_device`, an
    /// unpair of a still-connected device would skip the plugin
    /// teardown entirely. Half-state (guard added, ordering not
    /// fixed) makes this red: a recording mock plugin registered
    /// alongside the live generation sees NO `on_disconnected`
    /// call. Complete state (mirror `delete_device`'s
    /// `disconnect`-before-`notify` pattern) makes it green: the
    /// unpair removes the entry, the guard then sees None and
    /// dispatches to every plugin.
    #[tokio::test]
    async fn test_unpair_teardown_runs_even_while_connected() {
        let (state, _t) = test_state();
        pair_locally(&state, PEER_ID).await;
        // Mark a live generation WITHOUT the real TLS pair — the
        // registry guard reads `get_generation`, the unpair's
        // `disconnect` returns true on its own generation, and the
        // guard then sees None when `notify_disconnected` runs.
        state
            .connection_manager
            .mark_generation_for_test(PEER_ID, 7);

        // Recording mock plugin: the assertion target. It records
        // every device_id on_disconnected fires for; nothing else
        // depends on it.
        let recorder = Arc::new(UnpairRecorderPlugin::new());
        state
            .plugin_registry
            .register(recorder.clone() as Arc<dyn crate::plugins::Plugin>)
            .await;

        let result = unpair_device(State(state.clone()), Path(PEER_ID.to_string())).await;
        assert!(result.is_ok(), "unpair must succeed: {:?}", result.err());

        // Trust-boundary teardown MUST have run. With a live
        // generation before the unpair, the guard would skip
        // notify_disconnected unless the unpair removed the entry
        // first.
        let disconnected = recorder.disconnected_for_test().await;
        assert_eq!(
            disconnected,
            vec![PEER_ID.to_string()],
            "recording plugin saw no on_disconnected during unpair of a connected device; the registry-level guard skipped the trust-boundary teardown"
        );
    }

    /// Recording mock plugin for R5. Async `on_disconnected` to match
    /// the real `Plugin` trait shape.
    struct UnpairRecorderPlugin {
        disconnected: tokio::sync::Mutex<Vec<String>>,
    }

    impl UnpairRecorderPlugin {
        fn new() -> Self {
            Self {
                disconnected: tokio::sync::Mutex::new(Vec::new()),
            }
        }

        async fn disconnected_for_test(&self) -> Vec<String> {
            self.disconnected.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::plugins::Plugin for UnpairRecorderPlugin {
        fn name(&self) -> &str {
            "unpair-recorder-test"
        }

        fn incoming_capabilities(&self) -> Vec<String> {
            vec![]
        }

        fn outgoing_capabilities(&self) -> Vec<String> {
            vec![]
        }

        async fn handle_packet(
            &self,
            _device_id: &str,
            _packet: crate::protocol::types::Packet,
        ) -> crate::utils::errors::Result<Option<Vec<crate::protocol::types::Packet>>> {
            Ok(None)
        }

        async fn on_disconnected(&self, device_id: &str) {
            self.disconnected.lock().await.push(device_id.to_string());
        }
    }
}

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::plugins::mpris::{LocalPlayerState, MprisInfo};
use crate::utils::errors::Error;

/// GET /devices/{id}/mpris — phone-as-player-host snapshot list.
#[derive(Debug, Serialize, ToSchema)]
pub struct MprisPlayersResponse {
    pub device_id: String,
    pub players: Vec<MprisInfo>,
}

/// GET /mpris/local-players — control-role snapshot list (no device_id:
/// the local session is the host).
#[derive(Debug, Serialize, ToSchema)]
pub struct MprisLocalPlayersResponse {
    pub players: Vec<LocalPlayerState>,
}

/// POST /devices/{id}/mpris/{player}/action — echoes back the action
/// the desktop asked the phone to perform, so the caller can log the
/// {device_id, player, action} tuple against the side effect.
#[derive(Debug, Serialize, ToSchema)]
pub struct MprisActionResponse {
    pub device_id: String,
    pub player: String,
    pub action: String,
    pub sent: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/devices/{device_id}/mpris",
    tag = "mpris",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "Get MPRIS players from device", body = MprisPlayersResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_device_mpris(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<MprisPlayersResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let players = state.plugins.mpris.get_players(&device_id);
    Ok(Json(ApiResponse::ok(MprisPlayersResponse {
        device_id,
        players,
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/mpris/local-players",
    tag = "mpris",
    responses(
        (status = 200, description = "Get local (control-role) MPRIS players on this machine", body = MprisLocalPlayersResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_local_players(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<MprisLocalPlayersResponse>>, (axum::http::StatusCode, Json<ApiError>)>
{
    // Control role: this machine's own players as tracked from the session
    // D-Bus (empty when no session backend is enabled).
    let players = state.plugins.mpris.local_players();
    Ok(Json(ApiResponse::ok(MprisLocalPlayersResponse { players })))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/mpris/request",
    tag = "mpris",
    params(
        ("device_id" = String, Path, description = "Device unique identifier")
    ),
    responses(
        (status = 200, description = "MPRIS request sent", body = SentResponseWrapper),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn request_mpris(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<SentResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.mpris.request".to_string(),
        serde_json::json!({ "requestPlayerList": true }),
    );
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

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/mpris/{player}/action",
    tag = "mpris",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("player" = String, Path, description = "MPRIS player name")
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "MPRIS action sent", body = MprisActionResponseWrapper),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn mpris_action(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((device_id, player)): axum::extract::Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ApiResponse<MprisActionResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    let action = body
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| api_err(Error::InvalidRequest("action field required".to_string())))?
        .to_string();

    // MPRIS actions travel as kdeconnect.mpris.request with {player, action}
    // in the body — there is no kdeconnect.mpris.action packet type (the old
    // code invented one; the phone silently dropped it).
    let packet = crate::protocol::types::Packet::new(
        "kdeconnect.mpris.request".to_string(),
        serde_json::json!({
            "player": player,
            "action": action,
        }),
    );
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(MprisActionResponse {
        device_id,
        player,
        action,
        sent: true,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::config::settings::AppSettings;

    #[tokio::test]
    async fn test_get_local_players_without_session_backend_is_empty() {
        // AppState::new never enables the session D-Bus backend (the
        // production-only gate), so the control-role list degrades to
        // empty — the endpoint must still answer 200.
        let temp_dir = tempfile::tempdir().expect("Value expected to be present");
        let settings = AppSettings::new().with_data_dir(temp_dir.path().to_path_buf());
        let state = Arc::new(AppState::new(settings).expect("Value expected to be present"));

        let result = get_local_players(State(state)).await;
        let Json(response) = result.expect("local-players must answer even without a backend");
        let body = serde_json::to_value(&response).expect("Value expected to be present");
        let players = body
            .pointer("/data/players")
            .and_then(|v| v.as_array())
            .expect("players array must be present");
        assert!(players.is_empty());
    }

    #[test]
    fn test_mpris_players_response_matches_legacy_shape() {
        let response = MprisPlayersResponse {
            device_id: "phone-1".to_string(),
            players: Vec::new(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "players": []
        });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_mpris_local_players_response_matches_legacy_shape() {
        let response = MprisLocalPlayersResponse {
            players: Vec::new(),
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({ "players": [] });
        assert_eq!(typed, legacy);
    }

    #[test]
    fn test_mpris_action_response_matches_legacy_shape() {
        let response = MprisActionResponse {
            device_id: "phone-1".to_string(),
            player: "spotify".to_string(),
            action: "PlayPause".to_string(),
            sent: true,
        };
        let typed = serde_json::to_value(&response).expect("typed serialization");
        let legacy = serde_json::json!({
            "device_id": "phone-1",
            "player": "spotify",
            "action": "PlayPause",
            "sent": true
        });
        assert_eq!(typed, legacy);
    }
}

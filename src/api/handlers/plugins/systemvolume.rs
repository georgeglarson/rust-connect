//! REST handlers for the local systemvolume provider
//!
//! Single Responsibility: expose the local sink list + volume/mute/default
//! controls over HTTP, matching the existing REST conventions. The
//! provider side of systemvolume is optional (gated on a backend), so
//! every handler surfaces `503 Service Unavailable` when
//! `is_backend_available()` is false — the same honesty rule the
//! capability advertisement follows.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::*;
use crate::app::AppState;
use crate::plugins::plugin::Plugin;
use crate::plugins::systemvolume::SinkState;
use crate::utils::errors::Error;

#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct LocalSinksResponse {
    pub sinks: Vec<SinkState>,
    pub default_sink: Option<String>,
    pub available: bool,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalSinkControlRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub volume: Option<i64>,
    #[serde(default)]
    pub muted: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/api/v1/systemvolume/sinks",
    tag = "systemvolume",
    responses(
        (status = 200, description = "Local audio sinks", body = LocalSinksResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 503, description = "No audio backend available", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_local_sinks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<LocalSinksResponse>>, (StatusCode, Json<ApiError>)> {
    let plugin = &state.plugins.systemvolume;
    if !plugin.is_backend_available() {
        return Err(api_err(unavailable_error()));
    }
    let sinks = plugin.get_local_sinks();
    let default_sink = plugin.get_default_sink();
    Ok(Json(ApiResponse::ok(LocalSinksResponse {
        sinks,
        default_sink,
        available: true,
    })))
}

/// Apply a volume/mute/default change to a local sink.
///
/// Path parameter: the sink name. Body: `{ name?, volume?, muted?, enabled? }`.
/// The `name` field in the body is allowed for symmetry with the
/// per-device volume set endpoint but is ignored when the path sink is
/// authoritative.
#[utoipa::path(
    post,
    path = "/api/v1/systemvolume/sinks/{name}/control",
    tag = "systemvolume",
    params(
        ("name" = String, Path, description = "Sink name (PA sink identifier)")
    ),
    request_body = LocalSinkControlRequest,
    responses(
        (status = 200, description = "Control command applied", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 503, description = "No audio backend available", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn set_local_sink_control(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<LocalSinkControlRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (StatusCode, Json<ApiError>)> {
    if name.is_empty() {
        return Err(api_err(Error::InvalidRequest(
            "sink name cannot be empty".to_string(),
        )));
    }
    let plugin = &state.plugins.systemvolume;
    if !plugin.is_backend_available() {
        return Err(api_err(unavailable_error()));
    }
    let backend = plugin
        .backend()
        .ok_or_else(|| api_err(unavailable_error()))?;

    if let Some(volume) = body.volume {
        if let Err(e) = backend.set_volume(&name, volume).await {
            return Err(api_err(e));
        }
        // pulse.cpp:46 also un-mutes on volume change.
        if let Err(e) = backend.set_muted(&name, false).await {
            return Err(api_err(e));
        }
    }
    if let Some(muted) = body.muted {
        if let Err(e) = backend.set_muted(&name, muted).await {
            return Err(api_err(e));
        }
    }
    if let Some(enabled) = body.enabled {
        if let Err(e) = backend.set_default(&name, enabled).await {
            return Err(api_err(e));
        }
    }
    Ok(Json(ApiResponse::ok(serde_json::json!({
        "name": name,
        "volume": body.volume,
        "muted": body.muted,
        "enabled": body.enabled,
        "sent": true
    }))))
}

fn unavailable_error() -> Error {
    Error::PluginError {
        plugin: "systemvolume".to_string(),
        message: "backend unavailable".to_string(),
    }
}

// Reference validate_device_id so the import isn't dead.
#[allow(dead_code)]
fn _keep_validate_device_id_import() {
    let _ = validate_device_id;
}

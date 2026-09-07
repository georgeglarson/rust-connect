use axum::extract::State;
use axum::Json;
use std::sync::Arc;

use crate::api::types::*;
use crate::app::AppState;
use crate::plugins::Tool;

pub mod battery;
pub mod clipboard;
pub mod connectivity;
pub mod contacts;
pub mod findmyphone;
pub mod lock;
pub mod mpris;
pub mod notification;
pub mod remotecommands;
pub mod remotecontrol;
pub mod remotekeyboard;
pub mod sftp;
pub mod sms;
pub mod systemvolume;
pub mod telephony;
pub mod volume;

pub use battery::*;
pub use clipboard::*;
pub use connectivity::*;
pub use contacts::*;
pub use findmyphone::*;
pub use lock::*;
pub use mpris::*;
pub use notification::*;
pub use remotecommands::*;
pub use remotecontrol::*;
pub use remotekeyboard::*;
pub use sftp::*;
pub use sms::*;
pub use systemvolume::*;
pub use telephony::*;
pub use volume::*;

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ToolsResponse {
    pub tools: Vec<Tool>,
    pub count: usize,
}

#[utoipa::path(
    get,
    path = "/api/v1/plugins",
    tag = "plugins",
    responses(
        (status = 200, description = "List registered plugins", body = PluginsResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 500, description = "Internal error", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn list_plugins(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<PluginListResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    let plugins = state.plugin_registry.list().await;
    Ok(Json(ApiResponse::ok(PluginListResponse { plugins })))
}

#[utoipa::path(
    get,
    path = "/api/v1/plugins/capabilities",
    tag = "plugins",
    responses(
        (status = 200, description = "List registered capabilities", body = CapabilitiesResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn get_capabilities(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<CapabilitiesResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    let capabilities = state.packet_router.registered_types().await;
    Ok(Json(ApiResponse::ok(CapabilitiesResponse { capabilities })))
}

#[utoipa::path(
    get,
    path = "/api/v1/tools",
    tag = "tools",
    responses(
        (status = 200, description = "List available agent tools", body = ToolsResponse),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn list_tools(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<ToolsResponse>>, (axum::http::StatusCode, Json<ApiError>)> {
    // Each plugin owns its own tool entries. The default impl is empty;
    // plugins serving a REST route override `tools()` so the catalogue
    // tracks the surface without a hand-written match on capability
    // strings (audit 2026-09-06 §7). `is_backend_available` decides
    // whether each plugin's tools are servable right now; one lookup
    // gives us both pieces of state for plugins that need it.
    let plugin_names = state.plugin_registry.list().await;
    let mut tools = Vec::new();

    for name in plugin_names {
        let Some(plugin) = state.plugin_registry.get(&name).await else {
            continue;
        };
        let available = plugin.is_backend_available();
        for mut tool in plugin.tools() {
            if !available {
                tool.available = false;
            }
            tools.push(tool);
        }
    }

    // Two plugins can advertise the same tool name (e.g. telephony and
    // pausemusic both wrap a telephony route); collapse by name, taking
    // any-unavailable-is-servable: a degraded secondary consumer must
    // not shadow a healthy primary. Sort so the catalog is stable across
    // requests.
    tools.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    tools.dedup_by(|dup, kept| {
        if dup.name == kept.name {
            kept.available = kept.available || dup.available;
            true
        } else {
            false
        }
    });

    let count = tools.len();
    Ok(Json(ApiResponse::ok(ToolsResponse { tools, count })))
}

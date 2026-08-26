use axum::extract::State;
use axum::Json;
use std::sync::Arc;

use crate::api::types::*;
use crate::app::AppState;

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub capability: String,
    pub endpoint: String,
    pub method: String,
    pub parameters: Vec<ToolParameter>,
    /// Whether the plugin's backend is currently operational. Plugins
    /// without a separable backend always report `true`; plugins that
    /// detect a session-bus / portal / clipboard backend at runtime
    /// (clipboard, mpris, …) report the live state. `false` means the
    /// tool is listed for discoverability but cannot service a request
    /// right now — callers should not invoke the endpoint.
    pub available: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolParameter {
    pub name: String,
    pub param_type: String,
    pub required: bool,
    pub description: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolsResponse {
    pub tools: Vec<Tool>,
    pub count: usize,
}

fn capability_to_tool(cap: &str, _incoming: bool) -> Option<Tool> {
    let (name, description, endpoint, method, params, available) = match cap {
        "kdeconnect.ping" => (
            "ping_device".to_string(),
            "Send a ping to a device to check connectivity".to_string(),
            "/api/v1/ping".to_string(),
            "POST".to_string(),
            vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            true,
        ),
        "kdeconnect.battery" => (
            "get_battery".to_string(),
            "Get battery status of a device".to_string(),
            "/api/v1/devices/{device_id}/battery".to_string(),
            "GET".to_string(),
            vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            true,
        ),
        "kdeconnect.clipboard" => (
            "get_clipboard".to_string(),
            "Get clipboard content from any connected device".to_string(),
            "/api/v1/clipboard".to_string(),
            "GET".to_string(),
            vec![],
            // availability is overridden by list_tools once it has the
            // owning plugin in hand; this default keeps the lookup
            // callable in isolation.
            true,
        ),
        "kdeconnect.sms" => (
            "get_sms".to_string(),
            "Get SMS message threads from a device".to_string(),
            "/api/v1/devices/{device_id}/sms/threads".to_string(),
            "GET".to_string(),
            vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            true,
        ),
        "kdeconnect.mpris" => (
            "get_media".to_string(),
            "Get MPRIS media player status from a device".to_string(),
            "/api/v1/devices/{device_id}/mpris".to_string(),
            "GET".to_string(),
            vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            // overridden by list_tools; see kdeconnect.clipboard note.
            true,
        ),
        "kdeconnect.telephony" => (
            "get_telephony".to_string(),
            "Get recent telephony events from a device".to_string(),
            "/api/v1/devices/{device_id}/telephony".to_string(),
            "GET".to_string(),
            vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            true,
        ),
        "kdeconnect.notification" => (
            "get_notifications".to_string(),
            "Get notification history".to_string(),
            "/api/v1/notifications".to_string(),
            "GET".to_string(),
            vec![],
            true,
        ),
        "kdeconnect.share" => (
            "share_file".to_string(),
            "Share a file with a connected device".to_string(),
            "/api/v1/devices/{device_id}/share/send".to_string(),
            "POST".to_string(),
            vec![
                ToolParameter {
                    name: "device_id".to_string(),
                    param_type: "string".to_string(),
                    required: true,
                    description: "Target device ID".to_string(),
                },
                ToolParameter {
                    name: "file_path".to_string(),
                    param_type: "string".to_string(),
                    required: true,
                    description: "Path to file to share".to_string(),
                },
            ],
            true,
        ),
        "kdeconnect.runcommand" => (
            "get_remotecommands".to_string(),
            "Get remote commands from a connected device".to_string(),
            "/api/v1/devices/{device_id}/remotecommands".to_string(),
            "GET".to_string(),
            vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            true,
        ),
        "kdeconnect.systemvolume" => (
            "list_local_sinks".to_string(),
            "List local audio sinks (provider)".to_string(),
            "/api/v1/systemvolume/sinks".to_string(),
            "GET".to_string(),
            vec![],
            // overridden by list_tools once it has the owning plugin
            // in hand; the default keeps the lookup callable in
            // isolation.
            true,
        ),
        "kdeconnect.sftp" => (
            "browse_sftp".to_string(),
            "Mount the device's filesystem locally via sshfs and browse it".to_string(),
            "/api/v1/devices/{device_id}/sftp/mount".to_string(),
            "POST".to_string(),
            vec![ToolParameter {
                name: "device_id".to_string(),
                param_type: "string".to_string(),
                required: true,
                description: "Target device ID".to_string(),
            }],
            // Overridden by list_tools via SftpPlugin::is_backend_available
            // — false when sshfs / fusermount are missing on PATH so the
            // tool is never advertised as servable when it isn't.
            true,
        ),
        _ => return None,
    };

    Some(Tool {
        name,
        description,
        capability: cap.to_string(),
        endpoint,
        method,
        parameters: params,
        available,
    })
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
    let plugins = state.plugin_registry.list_with_capabilities().await;
    let mut tools = Vec::new();

    for plugin in plugins {
        // One lookup, both pieces of state — availability is a trait
        // method on the same Plugin trait that incoming_capabilities
        // lives on, so a generic hook covers any future backend-bearing
        // plugin (sendnotifications, pausemusic, screensaver_inhibit)
        // without per-plugin special cases in this handler.
        let available = state
            .plugin_registry
            .get(&plugin.name)
            .await
            .map(|p| p.is_backend_available())
            .unwrap_or(true);

        for cap in &plugin.incoming_capabilities {
            if let Some(mut tool) = capability_to_tool(cap, true) {
                if !available {
                    tool.available = false;
                }
                tools.push(tool);
            }
        }
    }

    // Two plugins can declare the same incoming capability (pausemusic and
    // telephony both consume kdeconnect.telephony), which pushes the same
    // tool twice — from a HashMap-ordered registry walk, so the duplicates
    // land in nondeterministic order and can disagree on `available` (each
    // copy reflects its own plugin's backend). Collapse by name: the tool
    // is available if ANY plugin serving that capability is (a degraded
    // secondary consumer must not shadow a healthy primary), and sort so
    // the catalog is stable across requests.
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

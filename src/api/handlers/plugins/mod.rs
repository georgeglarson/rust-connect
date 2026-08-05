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
pub mod remotekeyboard;
pub mod sftp;
pub mod sms;
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
pub use remotekeyboard::*;
pub use sftp::*;
pub use sms::*;
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
    let (name, description, endpoint, method, params) = match cap {
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
        ),
        "kdeconnect.clipboard" => (
            "get_clipboard".to_string(),
            "Get clipboard content from any connected device".to_string(),
            "/api/v1/clipboard".to_string(),
            "GET".to_string(),
            vec![],
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
        ),
        "kdeconnect.notification" => (
            "get_notifications".to_string(),
            "Get notification history".to_string(),
            "/api/v1/notifications".to_string(),
            "GET".to_string(),
            vec![],
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
        for cap in &plugin.incoming_capabilities {
            if let Some(tool) = capability_to_tool(cap, true) {
                tools.push(tool);
            }
        }
    }

    let count = tools.len();
    Ok(Json(ApiResponse::ok(ToolsResponse { tools, count })))
}

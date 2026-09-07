//! Tool catalogue entries
//!
//! The agent-facing surface at `GET /api/v1/tools` is a projection of each
//! plugin's `tools()` impl. A plugin describes the REST routes it serves;
//! the registry walk and JSON shape live in
//! `src/api/handlers/plugins/mod.rs::list_tools`. This module defines the
//! wire-shape of one entry, byte-identical to the previous in-handler
//! definition so the OpenAPI spec and `GET /api/v1/tools` JSON are
//! unchanged.

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
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

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ToolParameter {
    pub name: String,
    pub param_type: String,
    pub required: bool,
    pub description: String,
}

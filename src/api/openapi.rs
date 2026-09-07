use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::api::handlers;
use crate::api::sse;
use crate::api::types::*;
use crate::device::types::{Device, DeviceState, DeviceType};

#[derive(OpenApi)]
#[openapi(
    paths(
        handlers::health,
        handlers::list_devices,
        handlers::get_device,
        handlers::get_device_state,
        handlers::list_connected_devices,
        handlers::delete_device,
        handlers::connect_device,
        handlers::disconnect_device,
        handlers::pair_device,
        handlers::unpair_device,
        handlers::send_ping,
        sse::sse_events,
        handlers::get_remotecommands,
        handlers::trigger_remotecommand,
        handlers::send_remotekeyboard_keypress,
        handlers::list_plugins,
        handlers::get_capabilities,
        handlers::list_tools,
        handlers::get_device_battery,
        handlers::request_device_battery,
        handlers::mute_device_call,
        handlers::send_remotecontrol_pointer,
        handlers::get_sms_threads,
        handlers::get_sms_thread,
        handlers::request_sms_threads,
        handlers::send_sms,
        handlers::get_clipboard,
        handlers::set_clipboard,
        handlers::request_clipboard,
        handlers::get_device_mpris,
        handlers::get_local_players,
        handlers::request_mpris,
        handlers::mpris_action,
        handlers::get_device_telephony,
        handlers::lock_device,
        handlers::find_my_phone,
        handlers::sync_contacts,
        handlers::get_contacts,
        handlers::set_volume,
        handlers::get_local_sinks,
        handlers::set_local_sink_control,
        handlers::get_notifications,
        handlers::send_notification,
        handlers::reply_notification,
        handlers::dismiss_notification,
        handlers::activate_notification_action,
        handlers::get_notification_icon,
        handlers::get_device_connectivity,

        handlers::request_sftp,
        handlers::mount_sftp,
        handlers::unmount_sftp,
        handlers::get_sftp_info,
        handlers::send_file_to_device,
        handlers::send_text_to_device,
        handlers::send_url_to_device,
        handlers::list_share_files,
    ),
    components(schemas(
        ApiError,
        ApiErrorBody,
        ResponseMetadata,
        DeviceListResponse,
        DeviceSummary,
        Device,
        DeviceState,
        DeviceType,
        PairRequest,
        PairResponse,
        SendPingRequest,
        PluginListResponse,
        DevicesResponse,
        DeviceResponse,
        PairResponseWrapper,
        PingResponse,
        GenericResponse,
        PluginsResponse,
        CapabilitiesResponse,
        handlers::plugins::ToolsResponse,
        crate::plugins::Tool,
        crate::plugins::ToolParameter,
        RemoteCommandsResponseWrapper,
        crate::plugins::systemvolume::SinkState,
        crate::plugins::remotecommands::RemoteCommand,
        handlers::plugins::remotecontrol::PointerAction,
        SendSmsRequest,
        SendNotificationRequest,
        LockDeviceRequest,
        VolumeControlRequest,
        handlers::LocalSinksResponse,
        handlers::LocalSinkControlRequest,
        ReplyNotificationRequest,
        NotificationActionRequest,
        ShareTextRequest,
        ShareUrlRequest,
        handlers::SendKeypressRequest,
        handlers::RemoteCommandsResponse,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "health", description = "Service health check"),
        (name = "devices", description = "Device discovery and management"),
        (name = "pairing", description = "Device pairing operations"),
        (name = "events", description = "Server-Sent Events stream"),
        (name = "plugins", description = "Plugin information"),
        (name = "battery", description = "Battery monitoring"),
        (name = "sms", description = "SMS messaging"),
        (name = "clipboard", description = "Clipboard synchronization"),
        (name = "mpris", description = "Media player control"),
        (name = "telephony", description = "Telephony events"),
        (name = "lock", description = "Device lock control"),
        (name = "findmyphone", description = "Ring a paired device to locate it"),
        (name = "contacts", description = "Contacts synchronization"),
        (name = "volume", description = "Volume control"),
        (name = "systemvolume", description = "Local audio sink (systemvolume provider)"),
        (name = "notifications", description = "Notification management"),
        (name = "connectivity", description = "Connectivity report"),
        (name = "sftp", description = "SFTP filesystem browsing"),
        (name = "share", description = "File sharing"),
        (name = "remotecommands", description = "Remote commands management"),
        (name = "remotekeyboard", description = "Send keypresses to device"),
    ),
    security(
        ("api_key" = [])
    ),
    info(
        title = "Rust Connect API",
        version = "0.1.0",
        description = "REST API for KDE Connect-compatible device management. Every endpoint except `GET /api/v1/health` requires the API key in an `x-api-key` header; `GET /api/v1/events` also accepts it as the `api_key` query parameter, for `EventSource` clients that cannot set headers.\n\n\
                       # Server-Sent Events\n\n\
                       `GET /api/v1/events` is a `text/event-stream` of four frame shapes:\n\n\
                       - **`event: snapshot`** (named) — first frame on every (re)connect. Data is the same JSON `GET /api/v1/devices` returns in its `data` envelope (full `pair_state` + `verification_key` overlay). Use it to render the device pane on connect and after a `lagged` frame, without a follow-up REST call.\n\n\
                       - **unnamed data frame** — `data: <json>\\n\\n` carrying one device or plugin event. The JSON object includes a `kind` discriminator (snake-cased `<enum>.<variant>`), the existing legacy `event_type`/`type` field for backward compatibility, and the original payload keys. Possible `kind` values: `device.discovered`, `device.state_changed`, `device.paired`, `device.unpaired`, `device.pair_requested`, `device.connected`, `device.disconnected`, `device.removed`, `plugin.notification`, `plugin.battery`, `plugin.mpris_update`, `plugin.telephony_update`, `plugin.clipboard_update`, `plugin.sftp_update`, `plugin.remote_keyboard_echo`, `plugin.remote_keyboard_state`, `plugin.remote_commands_update`, `plugin.share_text`, `plugin.share_url`, `plugin.share_progress`, `plugin.system_volume_update`.\n\n\
                       - **`event: lagged`** (named) — broadcast channel dropped `N` events before delivery. Data is `{\"dropped\": N}`, and the frame is followed in the same chunk by a fresh `snapshot`, so the client re-renders without a REST call.\n\n\
                       - **keepalive comment** — `: keepalive\\n\\n` every 15s. SSE consumers ignore comment lines; the wire sees traffic so a dead upstream surfaces as a closed connection within ~15s rather than a half-open socket.\n\n\
                       Snapshot, event, and lagged frames carry an `id: <n>` line: a per-connection counter that starts at 1 and has no gaps (keepalives carry none and consume none), for correlating frames in a client log. The drop signal is the `lagged` frame, not an id gap. `Last-Event-ID` resume is not implemented.",
    )
)]
pub struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "api_key",
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-api-key"))),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_openapi_spec_is_valid_json() {
        let spec = ApiDoc::openapi();
        let json = serde_json::to_value(&spec).expect("OpenAPI spec should serialize to JSON");
        assert!(json.is_object());
    }

    #[test]
    fn test_openapi_spec_contains_expected_paths() {
        let spec = ApiDoc::openapi();
        let paths = spec.paths.paths.keys().cloned().collect::<Vec<_>>();

        assert!(
            paths.iter().any(|p| p == "/api/v1/devices"),
            "missing /api/v1/devices"
        );
        assert!(
            paths.iter().any(|p| p == "/api/v1/devices/{device_id}"),
            "missing device detail path"
        );
        assert!(
            paths
                .iter()
                .any(|p| p == "/api/v1/devices/{device_id}/pair"),
            "missing pair path"
        );
        assert!(
            paths
                .iter()
                .any(|p| p == "/api/v1/devices/{device_id}/unpair"),
            "missing unpair path"
        );
        assert!(
            paths.iter().any(|p| p == "/api/v1/ping"),
            "missing /api/v1/ping"
        );
        assert!(
            paths.iter().any(|p| p == "/api/v1/plugins"),
            "missing /api/v1/plugins"
        );
    }

    #[test]
    fn test_openapi_spec_documents_sse() {
        // Audit 2026-09-06 item 5: SSE moved from a deliberately
        // undocumented channel to a documented contract — the four
        // frame shapes (snapshot, data-with-kind, lagged, keepalive)
        // are listed in the OpenAPI info.description so a generated
        // TypeScript / OpenAPI consumer can wire the stream.
        let spec = ApiDoc::openapi();
        let description = spec
            .info
            .description
            .as_deref()
            .expect("OpenAPI info.description must include the SSE frame-shape contract");

        // The description must enumerate every frame shape and the
        // `kind` vocabulary — checked by the markers below.
        let markers = ["snapshot", "data", "kind", "lagged", "keepalive", "api_key"];
        for marker in markers
            .iter()
            .copied()
            .chain(crate::api::sse::ALL_KINDS.iter().copied())
        {
            assert!(
                description.contains(marker),
                "info.description must mention `{marker}` for the SSE contract; got: {description}"
            );
        }

        // The path itself must be in the spec (was deliberately excluded
        // before this audit; route_table_lint.rs was updated alongside).
        assert!(
            spec.paths.paths.contains_key("/api/v1/events"),
            "/api/v1/events must be in the OpenAPI spec's paths"
        );
    }

    #[test]
    fn test_openapi_spec_has_security_scheme() {
        let spec = ApiDoc::openapi();
        let components = spec
            .components
            .as_ref()
            .expect("spec should have components");
        assert!(
            components.security_schemes.contains_key("api_key"),
            "missing api_key security scheme"
        );
    }

    #[test]
    fn test_openapi_spec_title_and_version() {
        let spec = ApiDoc::openapi();
        assert_eq!(spec.info.title, "Rust Connect API");
        assert_eq!(spec.info.version, "0.1.0");
    }

    #[test]
    fn test_openapi_spec_has_all_tags() {
        let spec = ApiDoc::openapi();
        let tags = spec.tags.as_ref().expect("spec should have tags");
        let tag_names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        assert!(tag_names.contains(&"devices"));
        assert!(tag_names.contains(&"pairing"));
        assert!(tag_names.contains(&"plugins"));
    }

    #[test]
    fn test_openapi_spec_contains_share_text_and_url_paths() {
        let spec = ApiDoc::openapi();
        let paths = spec.paths.paths.keys().cloned().collect::<Vec<_>>();
        assert!(
            paths
                .iter()
                .any(|p| p == "/api/v1/devices/{device_id}/share/text"),
            "missing share/text path"
        );
        assert!(
            paths
                .iter()
                .any(|p| p == "/api/v1/devices/{device_id}/share/url"),
            "missing share/url path"
        );
    }
}

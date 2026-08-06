//! API router
//!
//! Single Responsibility: Route mapping only.

use std::sync::Arc;

use axum::http::HeaderValue;
use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api::handlers;
use crate::api::middleware::request_logger;
use crate::api::openapi::ApiDoc;
use crate::app::AppState;

pub fn build_router(state: Arc<AppState>) -> Router {
    // CORS is deny-by-default: an empty allowed_origins list builds a layer
    // that matches no origin, so browsers get no Access-Control-Allow-Origin.
    // "*" opts in to any origin (see the field docs in settings.rs).
    let cors = if state.settings.allowed_origins.iter().any(|o| o == "*") {
        CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods(AllowMethods::list([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::DELETE,
                axum::http::Method::PUT,
                axum::http::Method::PATCH,
            ]))
            .allow_headers(AllowHeaders::list([
                axum::http::header::HeaderName::from_static("x-api-key"),
                axum::http::header::HeaderName::from_static("content-type"),
            ]))
    } else {
        let origins: Vec<HeaderValue> = state
            .settings
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse::<HeaderValue>().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods(AllowMethods::list([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::DELETE,
                axum::http::Method::PUT,
                axum::http::Method::PATCH,
            ]))
            .allow_headers(AllowHeaders::list([
                axum::http::header::HeaderName::from_static("x-api-key"),
                axum::http::header::HeaderName::from_static("content-type"),
            ]))
    };

    let api_router = Router::new()
        .route("/api/v1/devices", get(handlers::list_devices))
        .route("/api/v1/devices/:device_id", get(handlers::get_device))
        .route(
            "/api/v1/devices/:device_id/pair",
            post(handlers::pair_device),
        )
        .route(
            "/api/v1/devices/:device_id/unpair",
            delete(handlers::unpair_device),
        )
        .route("/api/v1/ping", post(handlers::send_ping))
        .route("/api/v1/plugins", get(handlers::list_plugins))
        .route(
            "/api/v1/plugins/capabilities",
            get(handlers::get_capabilities),
        )
        .route("/api/v1/tools", get(handlers::list_tools))
        .route("/api/v1/share/files", get(handlers::list_share_files))
        .route("/api/v1/clipboard", get(handlers::get_clipboard))
        .route("/api/v1/clipboard", post(handlers::set_clipboard))
        .route(
            "/api/v1/devices/:device_id/clipboard/request",
            post(handlers::request_clipboard),
        )
        .route(
            "/api/v1/devices/:device_id/battery",
            get(handlers::get_device_battery),
        )
        .route(
            "/api/v1/devices/:device_id/battery/request",
            post(handlers::request_device_battery),
        )
        .route(
            "/api/v1/devices/:device_id/sms/threads",
            get(handlers::get_sms_threads),
        )
        .route(
            "/api/v1/devices/:device_id/sms/threads/:thread_id",
            get(handlers::get_sms_thread),
        )
        .route(
            "/api/v1/devices/:device_id/sms/request",
            post(handlers::request_sms_threads),
        )
        .route(
            "/api/v1/devices/:device_id/sms/send",
            post(handlers::send_sms),
        )
        .route(
            "/api/v1/mpris/local-players",
            get(handlers::get_local_players),
        )
        .route(
            "/api/v1/devices/:device_id/mpris",
            get(handlers::get_device_mpris),
        )
        .route(
            "/api/v1/devices/:device_id/mpris/request",
            post(handlers::request_mpris),
        )
        .route(
            "/api/v1/devices/:device_id/remotecommands",
            get(handlers::get_remotecommands),
        )
        .route(
            "/api/v1/devices/:device_id/remotecommands/:key/trigger",
            post(handlers::trigger_remotecommand),
        )
        .route(
            "/api/v1/devices/:device_id/remotekeyboard/keypress",
            post(handlers::send_remotekeyboard_keypress),
        )
        .route(
            "/api/v1/devices/:device_id/mpris/:player/action",
            post(handlers::mpris_action),
        )
        .route(
            "/api/v1/devices/:device_id/telephony",
            get(handlers::get_device_telephony),
        )
        .route(
            "/api/v1/devices/:device_id/lock",
            post(handlers::lock_device),
        )
        .route(
            "/api/v1/devices/:device_id/findmyphone",
            post(handlers::find_my_phone),
        )
        .route(
            "/api/v1/devices/:device_id/contacts",
            get(handlers::get_contacts),
        )
        .route(
            "/api/v1/devices/:device_id/contacts/sync",
            post(handlers::sync_contacts),
        )
        .route(
            "/api/v1/devices/:device_id/volume",
            post(handlers::set_volume),
        )
        .route("/api/v1/systemvolume/sinks", get(handlers::get_local_sinks))
        .route(
            "/api/v1/systemvolume/sinks/:name/control",
            post(handlers::set_local_sink_control),
        )
        .route(
            "/api/v1/devices/:device_id/sftp/request",
            post(handlers::request_sftp),
        )
        .route(
            "/api/v1/devices/:device_id/sftp/mount",
            post(handlers::mount_sftp).delete(handlers::unmount_sftp),
        )
        .route(
            "/api/v1/devices/:device_id/sftp",
            get(handlers::get_sftp_info),
        )
        .route(
            "/api/v1/devices/:device_id/connectivity",
            get(handlers::get_device_connectivity),
        )
        .route(
            "/api/v1/devices/:device_id",
            delete(handlers::delete_device),
        )
        .route(
            "/api/v1/devices/:device_id/connect",
            post(handlers::connect_device),
        )
        .route(
            "/api/v1/devices/:device_id/disconnect",
            post(handlers::disconnect_device),
        )
        .route(
            "/api/v1/devices/:device_id/state",
            get(handlers::get_device_state),
        )
        .route(
            "/api/v1/devices/connected",
            get(handlers::list_connected_devices),
        )
        .route(
            "/api/v1/devices/:device_id/share/send",
            // The handler's 100 MiB upload cap is dead code without this:
            // axum's default 2 MiB DefaultBodyLimit rejects larger bodies
            // (413) before parse_upload runs. +1 MiB slack: multipart
            // framing inflates the body beyond the file bytes.
            post(handlers::send_file_to_device)
                .route_layer(axum::extract::DefaultBodyLimit::max(101 * 1024 * 1024)),
        )
        .route(
            "/api/v1/devices/:device_id/share/text",
            post(handlers::send_text_to_device),
        )
        .route(
            "/api/v1/devices/:device_id/share/url",
            post(handlers::send_url_to_device),
        )
        .route(
            "/api/v1/devices/:device_id/notification",
            post(handlers::send_notification),
        )
        .route(
            "/api/v1/devices/:device_id/notification/:notification_id/reply",
            post(handlers::reply_notification),
        )
        .route(
            "/api/v1/devices/:device_id/notification/:notification_id/action",
            post(handlers::activate_notification_action),
        )
        .route(
            "/api/v1/devices/:device_id/notification/:notification_id/dismiss",
            post(handlers::dismiss_notification),
        )
        .route("/api/v1/notifications", get(handlers::get_notifications))
        .route(
            "/api/v1/devices/:device_id/notification-icons/:icon_hash",
            get(handlers::get_notification_icon),
        )
        .route("/api/v1/events", get(crate::api::sse::sse_events))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::api::middleware::auth_middleware,
        ));

    let mut router = Router::new()
        .route("/api/v1/health", get(handlers::health))
        .merge(api_router)
        .merge(SwaggerUi::new("/docs").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(cors)
        .layer(middleware::from_fn(
            crate::api::middleware::security_headers,
        ))
        .layer(middleware::from_fn(crate::api::middleware::rate_limiter))
        .layer(middleware::from_fn(request_logger));

    if state.settings.ui_enabled {
        router = router
            .route("/", get(handlers::redirect_to_ui))
            .route("/ui", get(handlers::serve_ui))
            .route("/ui/", get(handlers::serve_ui))
            .route("/ui/index.html", get(handlers::serve_ui));
    }

    router.with_state(state)
}

//! Desktop -> phone POINTER producer (vk #1040).
//!
//! kdeconnect-kde exposes this as its `remotecontrol` plugin: a D-Bus adaptor
//! that PRODUCES `kdeconnect.mousepad.request` to drive the peer's pointer
//! (`plugins/remotecontrol/remotecontrolplugin.cpp:21-33` — `moveCursor` sends
//! `{dx, dy}`). rust-connect had the consume side only.
//!
//! The wire shapes are built by `MousepadRequest`'s constructors, which
//! serialize the SAME struct the consume side deserializes, so the producer
//! cannot drift from the parser.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    api::{
        extractors::{api_err, validate_device_id},
        types::{ApiError, ApiResponse},
    },
    app::AppState,
    plugins::mousepad::{MousepadRequest, PointerClick},
    utils::errors::Error,
};

/// One pointer action. Exactly one of the four shapes, mirroring how upstream
/// sends one field set per packet (MousePadPlugin.kt:77-186).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PointerAction {
    /// Relative motion (remotecontrolplugin.cpp:23).
    Move { dx: f64, dy: f64 },
    /// Absolute position.
    MoveAbsolute { x: f64, y: f64 },
    /// `single`, `double`, `middle`, `right`, `hold`, `release`.
    Click { button: String },
    /// dx/dy are WHEEL deltas here, not motion (x11remoteinput.cpp:103).
    Scroll { dx: f64, dy: f64 },
}

fn build(action: &PointerAction) -> Result<MousepadRequest, Error> {
    Ok(match action {
        PointerAction::Move { dx, dy } => MousepadRequest::move_relative(*dx, *dy),
        PointerAction::MoveAbsolute { x, y } => MousepadRequest::move_absolute(*x, *y),
        PointerAction::Scroll { dx, dy } => MousepadRequest::scroll(*dx, *dy),
        PointerAction::Click { button } => MousepadRequest::click(match button.as_str() {
            "single" => PointerClick::Single,
            "double" => PointerClick::Double,
            "middle" => PointerClick::Middle,
            "right" => PointerClick::Right,
            "hold" => PointerClick::Hold,
            "release" => PointerClick::Release,
            other => {
                return Err(Error::InvalidRequest(format!(
                    "button: expected one of single/double/middle/right/hold/release, got {other:?}"
                )))
            }
        }),
    })
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/remotecontrol/pointer",
    tag = "remotecontrol",
    request_body = PointerAction,
    params(("device_id" = String, Path, description = "Device unique identifier")),
    responses(
        (status = 200, description = "Pointer action sent", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Invalid device ID or button", body = ApiError),
        (status = 401, description = "Invalid or missing API key", body = ApiError),
        (status = 404, description = "Device not found or not connected", body = ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_remotecontrol_pointer(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(action): Json<PointerAction>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (axum::http::StatusCode, Json<ApiError>)> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::DeviceNotFound(device_id)));
    }

    let packet = build(&action)
        .map_err(api_err)?
        .into_packet()
        .map_err(api_err)?;
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(
        serde_json::json!({ "status": "sent" }),
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn body_of(json: serde_json::Value) -> serde_json::Value {
        let action: PointerAction = serde_json::from_value(json).expect("action parses");
        build(&action)
            .expect("builds")
            .into_packet()
            .expect("serializes")
            .body
    }

    #[test]
    fn test_move_action_produces_upstream_relative_shape() {
        assert_eq!(
            body_of(serde_json::json!({"action": "move", "dx": 5.0, "dy": -2.0})),
            serde_json::json!({"dx": 5.0, "dy": -2.0})
        );
    }

    #[test]
    fn test_scroll_action_sets_the_scroll_flag() {
        assert_eq!(
            body_of(serde_json::json!({"action": "scroll", "dx": 0.0, "dy": 3.0})),
            serde_json::json!({"scroll": true, "dx": 0.0, "dy": 3.0})
        );
    }

    #[test]
    fn test_every_documented_button_name_is_accepted() {
        for b in ["single", "double", "middle", "right", "hold", "release"] {
            let body = body_of(serde_json::json!({"action": "click", "button": b}));
            assert_eq!(
                body.as_object().expect("object").len(),
                1,
                "{b} must set exactly one flag, got {body}"
            );
        }
    }

    #[test]
    fn test_unknown_button_is_a_400_not_a_silent_noop() {
        let action: PointerAction =
            serde_json::from_value(serde_json::json!({"action": "click", "button": "wheel3"}))
                .expect("parses");
        let err = build(&action).expect_err("unknown button must be rejected");
        assert_eq!(err.code().http_status(), 400, "got {err}");
    }
}

//! API handlers
//!
//! Single Responsibility: Re-export handler modules and provide shared utilities.

mod device;
pub mod plugins;
pub mod share;
mod ui;

pub use device::*;
pub use plugins::*;
pub use share::*;
pub use ui::*;

use axum::extract::State;
use axum::Json;
use std::sync::Arc;

use crate::app::AppState;

#[utoipa::path(
    get,
    path = "/api/v1/health",
    tag = "health",
    responses(
        (status = 200, description = "Service liveness probe", body = serde_json::Value),
    )
    // intentionally no `security(("api_key" = []))` — health is mounted
    // outside the auth middleware in src/api/router.rs.
)]
pub async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "uptime_seconds": state.started_at.elapsed().as_secs()
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use crate::api::extractors::{api_err, validate_device_id};
    use crate::utils::errors::Error;
    use axum::http::StatusCode;

    #[test]
    fn test_valid_device_ids() {
        // Wire-spec ids: 32–38 chars of [a-zA-Z0-9_-] (DeviceInfo.kt).
        assert!(validate_device_id(&"a".repeat(32)).is_ok());
        assert!(validate_device_id(&"a".repeat(38)).is_ok());
        assert!(validate_device_id("0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_device_id("9f1cb61f-2cbf-4608-9ba6-b4576f03553a").is_ok());
        assert!(validate_device_id("A-B_C-123aaaaaaaaaaaaaaaaaaaaaaaa").is_ok());
    }

    #[test]
    fn test_invalid_device_ids() {
        assert!(validate_device_id("").is_err());
        assert!(validate_device_id("abc123").is_err());
        assert!(validate_device_id("my-phone").is_err());
        assert!(validate_device_id(&"a".repeat(31)).is_err());
        assert!(validate_device_id(&"a".repeat(39)).is_err());
        assert!(validate_device_id(&"a".repeat(129)).is_err());
        assert!(validate_device_id("../etc/passwdaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo/baraaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo\\baraaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo..baraaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("has spacesaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("has.dot.aaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("has@symbolaaaaaaaaaaaaaaaaaaaaaaa").is_err());
    }

    #[test]
    fn test_api_err_maps_to_status_code() {
        let (status, body) = api_err(Error::not_found("thing", None::<String>));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.0.error.message.contains("thing"));

        let (status, body) = api_err(Error::InvalidRequest("bad input".to_string()));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0.error.message.contains("bad input"));

        let (status, _) = api_err(Error::Internal("boom".to_string()));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

        let (status, _) = api_err(Error::Unauthorized("no key".to_string()));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

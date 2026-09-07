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

#[cfg(test)]
mod device_tests;

use axum::extract::State;
use axum::Json;
use std::sync::Arc;

use crate::app::AppState;

/// The running build, so an installed daemon can be compared to
/// `origin/main` (vk #973).
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct BuildInfo {
    pub version: String,
    pub git_sha: String,
    pub dirty: bool,
}

/// `GET /api/v1/health`: the one body outside the `{status, data, metadata}`
/// envelope. It is the public liveness probe (no API key), kept flat so a
/// process supervisor can read `status` without unwrapping anything.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_seconds: u64,
    pub build: BuildInfo,
}

impl HealthResponse {
    pub fn now(state: &AppState) -> Self {
        Self {
            status: "ok".to_string(),
            uptime_seconds: state.started_at.elapsed().as_secs(),
            build: BuildInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                git_sha: crate::GIT_SHA.to_string(),
                dirty: env!("RC_GIT_DIRTY") == "1",
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/health",
    tag = "health",
    responses(
        (status = 200, description = "Service liveness probe", body = HealthResponse),
    )
    // intentionally no `security(("api_key" = []))` — health is mounted
    // outside the auth middleware in src/api/router.rs.
)]
pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse::now(&state))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use crate::api::extractors::{api_err, validate_device_id};

    #[test]
    fn test_health_response_matches_legacy_shape() {
        let resp = super::HealthResponse {
            status: "ok".to_string(),
            uptime_seconds: 42,
            build: super::BuildInfo {
                version: "0.1.0".to_string(),
                git_sha: "abc123".to_string(),
                dirty: false,
            },
        };
        let typed = serde_json::to_value(&resp).unwrap();
        let legacy = serde_json::json!({
            "status": "ok",
            "uptime_seconds": 42,
            "build": {"version": "0.1.0", "git_sha": "abc123", "dirty": false}
        });
        assert_eq!(typed, legacy);
    }
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

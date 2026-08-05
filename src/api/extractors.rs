//! API shared utilities
//!
//! Single Responsibility: Provide shared validation and error helpers for API handlers.

use axum::http::StatusCode;
use axum::Json;

use crate::api::types::ApiError;
use crate::utils::errors::Error;

/// Validates a device_id string against the Android wire requirements.
///
/// Single source of truth: `crate::protocol::crypto::validate_device_id`
/// (32–38 chars of `[a-zA-Z0-9_-]`, no traversal). The API layer used to
/// accept any 1–128 char string, which let non-wire ids deep into the
/// protocol layer before failing.
pub fn validate_device_id(device_id: &str) -> Result<(), Error> {
    crate::protocol::crypto::validate_device_id(device_id)
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

/// Converts an Error into an axum response tuple.
///
/// The full error (which may embed absolute filesystem paths via
/// `Error::io` / `Error::not_found`) is logged server-side; the client body
/// gets the same message with any absolute-path token redacted, so the
/// host's filesystem layout never leaks to API callers.
pub fn api_err(e: Error) -> (StatusCode, Json<ApiError>) {
    let status = e.code().http_status();
    tracing::warn!(error = %e, code = e.code().as_str(), "API request failed");
    let api_error = ApiError::new(e.code(), sanitize_message(&e.to_string()));
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(api_error),
    )
}

/// Redacts whitespace-delimited tokens that look like absolute paths.
fn sanitize_message(message: &str) -> String {
    message
        .split_whitespace()
        .map(|token| {
            if token.starts_with('/') {
                "[path]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_api_err_redacts_filesystem_paths() {
        let path = "/home/alice/.local/share/rust-connect/paired.json";
        let err = Error::io(
            "Failed to persist pairing state".to_string(),
            Some(path.to_string()),
        );
        let (_status, body) = api_err(err);
        let message = &body.0.error.message;
        assert!(
            !message.contains(path),
            "response body must not leak the path: {message}"
        );
        assert!(!message.contains("/home"));
        assert!(message.contains("Failed to persist pairing state"));
        assert!(message.contains("[path]"));
    }

    #[test]
    fn test_api_err_not_found_redacts_path() {
        let err = Error::not_found("config", Some("/home/alice/secret.toml".to_string()));
        let (_status, body) = api_err(err);
        let message = &body.0.error.message;
        assert!(!message.contains("/home"));
        assert!(message.contains("config"));
    }

    #[test]
    fn test_api_err_keeps_pathless_messages_intact() {
        let (_status, body) = api_err(Error::InvalidRequest("bad input".to_string()));
        assert_eq!(body.0.error.message, "Invalid API request: bad input");
    }
}

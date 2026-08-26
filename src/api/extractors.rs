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
    // Pass the error THROUGH. `crypto::validate_device_id` already returns
    // `Error::InvalidRequest`, whose Display is "Invalid API request: {0}", so
    // re-wrapping `e.to_string()` in another `InvalidRequest` applied the
    // prefix twice (audit F-L4: "Invalid API request: Invalid API request:
    // device_id: ...").
    crate::protocol::crypto::validate_device_id(device_id)
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
    fn test_device_not_paired_is_409_not_500() {
        // Audit F-M1. Unpairing a device that is not paired is a benign client
        // condition; 500 made monitoring read it as a server crash, and the
        // error code already said DEVICE_NOT_PAIRED (a client-error code).
        let (status, body) = api_err(Error::DeviceNotPaired("zzzz".to_string()));
        assert_eq!(status, StatusCode::CONFLICT, "body was {:?}", body.0.error);
        assert_eq!(body.0.error.code, "DEVICE_NOT_PAIRED");
    }

    #[test]
    fn test_unpair_status_matches_its_sibling_delete_class() {
        // The audit's actual complaint: unpair was the OUTLIER. Its siblings
        // return 4xx for the same unknown/absent id, so assert the class, not
        // just the number.
        for e in [
            Error::DeviceNotPaired("zzzz".to_string()),
            Error::DeviceAlreadyExists("zzzz".to_string()),
        ] {
            let (status, _) = api_err(e);
            assert!(status.is_client_error(), "expected 4xx, got {status}");
        }
    }

    #[test]
    fn test_invalid_device_id_message_is_not_double_prefixed() {
        // Audit F-L4: the wrapper was applied twice.
        let err = validate_device_id("aaaa").expect_err("4 chars must be rejected");
        let message = err.to_string();
        assert_eq!(
            message.matches("Invalid API request").count(),
            1,
            "prefix applied more than once: {message}"
        );
    }

    #[test]
    fn test_invalid_device_id_still_reports_the_field_and_reason() {
        // Removing the wrapper must not cost the caller the useful part.
        let err = validate_device_id("aaaa").expect_err("4 chars must be rejected");
        let message = err.to_string();
        assert!(message.contains("device_id"), "lost the field name: {message}");
        let (status, _) = api_err(err);
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

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

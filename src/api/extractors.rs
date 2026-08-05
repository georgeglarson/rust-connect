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
pub fn api_err(e: Error) -> (StatusCode, Json<ApiError>) {
    let status = e.code().http_status();
    let api_error = ApiError::new(e.code(), e.to_string());
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(api_error),
    )
}

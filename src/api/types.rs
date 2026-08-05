//! API response types
//!
//! Single Responsibility: Define API request/response structures.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::device::types::{Device, DeviceState, DeviceType};
use crate::utils::errors::ErrorCode;

#[derive(Debug, Serialize, ToSchema)]
#[aliases(
    DevicesResponse = ApiResponse<DeviceListResponse>,
    DeviceResponse = ApiResponse<Device>,
    PairResponseWrapper = ApiResponse<PairResponse>,
    PingResponse = ApiResponse<serde_json::Value>,
    PluginsResponse = ApiResponse<PluginListResponse>,
)]
pub struct ApiResponse<T: Serialize> {
    pub status: &'static str,
    pub data: T,
    pub metadata: ResponseMetadata,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiError {
    pub status: &'static str,
    pub error: ApiErrorBody,
    pub metadata: ResponseMetadata,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResponseMetadata {
    pub timestamp: DateTime<Utc>,
    pub request_id: String,
}

impl Default for ResponseMetadata {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseMetadata {
    pub fn new() -> Self {
        Self {
            timestamp: Utc::now(),
            request_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            status: "ok",
            data,
            metadata: ResponseMetadata::new(),
        }
    }
}

impl ApiError {
    pub fn new(code: ErrorCode, message: String) -> Self {
        Self {
            status: "error",
            error: ApiErrorBody {
                code: code.as_str().to_string(),
                message,
            },
            metadata: ResponseMetadata::new(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceListResponse {
    pub devices: Vec<DeviceSummary>,
    pub total: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub device_type: DeviceType,
    pub state: DeviceState,
    pub last_seen: DateTime<Utc>,
    /// Stamped by the pairing store (authoritative), None when unpaired.
    pub paired_at: Option<DateTime<Utc>>,
}

impl From<&Device> for DeviceSummary {
    fn from(d: &Device) -> Self {
        Self {
            id: d.id.clone(),
            name: d.name.clone(),
            device_type: d.device_type,
            state: d.state,
            last_seen: d.last_seen,
            paired_at: d.paired_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PairRequest {
    pub device_id: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendPingRequest {
    pub device_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PluginListResponse {
    pub plugins: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CapabilitiesResponse {
    pub capabilities: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PairResponse {
    pub device_id: String,
    pub status: &'static str,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_api_response_serialization() {
        let resp = ApiResponse::ok(vec!["a", "b"]);
        let json = serde_json::to_string(&resp).expect("Serialization of known types cannot fail");
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"data\":[\"a\",\"b\"]"));
        assert!(json.contains("\"timestamp\""));
        assert!(json.contains("\"request_id\""));
    }

    #[test]
    fn test_api_error_serialization() {
        let err = ApiError::new(ErrorCode::DeviceNotFound, "not here".to_string());
        let json = serde_json::to_string(&err).expect("Serialization of known types cannot fail");
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"code\":\"DEVICE_NOT_FOUND\""));
        assert!(json.contains("\"message\":\"not here\""));
    }

    #[test]
    fn test_metadata_has_unique_ids() {
        let m1 = ResponseMetadata::new();
        let m2 = ResponseMetadata::new();
        assert_ne!(m1.request_id, m2.request_id);
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendSmsRequest {
    pub phone_number: String,
    pub message_body: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendNotificationRequest {
    pub title: String,
    pub text: String,
    #[serde(default = "default_app_name")]
    pub app_name: String,
    #[serde(default)]
    pub ticker: Option<String>,
    #[serde(default = "default_true")]
    pub is_clearable: bool,
    #[serde(default)]
    pub silent: bool,
}

fn default_app_name() -> String {
    "Agent".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LockDeviceRequest {
    pub action: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VolumeControlRequest {
    /// Absolute integer on the sink's own scale, ceiling `maxVolume`
    /// (kdeconnect-kde plugins/systemvolume/systemvolumeplugin-pulse.cpp:44
    /// reads the request with `np.get<int>("volume")`; the ceiling is
    /// PulseAudioQt::normalVolume() == 65536, :94). Not a 0.0-1.0 fraction.
    pub volume: Option<i64>,
    pub muted: Option<bool>,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplyNotificationRequest {
    pub message: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShareTextRequest {
    pub text: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShareUrlRequest {
    pub url: String,
}

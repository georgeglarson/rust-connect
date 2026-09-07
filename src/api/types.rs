//! API response types
//!
//! Single Responsibility: Define API request/response structures.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::handlers::RemoteCommandsResponse;
use crate::api::handlers::plugins::battery::BatteryResponse;
use crate::api::handlers::plugins::connectivity::ConnectivityResponse;
use crate::api::handlers::plugins::contacts::{ContactsListResponse, ContactsSyncResponse};
use crate::api::handlers::plugins::mpris::{
    MprisActionResponse, MprisLocalPlayersResponse, MprisPlayersResponse,
};
use crate::api::handlers::plugins::notification::{
    NotificationActionTriggeredResponse, NotificationDismissedResponse, NotificationReplyResponse,
    NotificationSentResponse, NotificationsListResponse,
};
use crate::api::handlers::plugins::remotecommands::RemoteCommandTriggerResponse;
use crate::api::handlers::plugins::sftp::{
    SftpInfoResponse, SftpMountResponse, SftpRequestResponse, SftpUnmountResponse,
};
use crate::api::handlers::plugins::sms::{
    SmsSentResponse, SmsThreadResponse, SmsThreadsResponse,
};
use crate::api::handlers::plugins::telephony::TelephonyCallsResponse;
use crate::device::types::{Device, DeviceState, DeviceType};
use crate::utils::errors::ErrorCode;

#[derive(Debug, Serialize, ToSchema)]
#[aliases(
    DevicesResponse = ApiResponse<DeviceListResponse>,
    DeviceResponse = ApiResponse<Device>,
    PairResponseWrapper = ApiResponse<PairResponse>,
    PingResponse = ApiResponse<serde_json::Value>,
    GenericResponse = ApiResponse<serde_json::Value>,
    RemoteCommandsResponseWrapper = ApiResponse<RemoteCommandsResponse>,
    PluginsResponse = ApiResponse<PluginListResponse>,
    SentResponseWrapper = ApiResponse<SentResponse>,
    BatteryResponseWrapper = ApiResponse<BatteryResponse>,
    ConnectivityResponseWrapper = ApiResponse<ConnectivityResponse>,
    TelephonyCallsResponseWrapper = ApiResponse<TelephonyCallsResponse>,
    SftpRequestResponseWrapper = ApiResponse<SftpRequestResponse>,
    SftpInfoResponseWrapper = ApiResponse<SftpInfoResponse>,
    SftpMountResponseWrapper = ApiResponse<SftpMountResponse>,
    SftpUnmountResponseWrapper = ApiResponse<SftpUnmountResponse>,
    SmsThreadsResponseWrapper = ApiResponse<SmsThreadsResponse>,
    SmsThreadResponseWrapper = ApiResponse<SmsThreadResponse>,
    SmsSentResponseWrapper = ApiResponse<SmsSentResponse>,
    MprisPlayersResponseWrapper = ApiResponse<MprisPlayersResponse>,
    MprisLocalPlayersResponseWrapper = ApiResponse<MprisLocalPlayersResponse>,
    MprisActionResponseWrapper = ApiResponse<MprisActionResponse>,
    ContactsSyncResponseWrapper = ApiResponse<ContactsSyncResponse>,
    ContactsListResponseWrapper = ApiResponse<ContactsListResponse>,
    RemoteCommandTriggerResponseWrapper = ApiResponse<RemoteCommandTriggerResponse>,
    NotificationsListResponseWrapper = ApiResponse<NotificationsListResponse>,
    NotificationSentResponseWrapper = ApiResponse<NotificationSentResponse>,
    NotificationReplyResponseWrapper = ApiResponse<NotificationReplyResponse>,
    NotificationActionTriggeredResponseWrapper = ApiResponse<NotificationActionTriggeredResponse>,
    NotificationDismissedResponseWrapper = ApiResponse<NotificationDismissedResponse>,
)]
pub struct ApiResponse<T: Serialize> {
    pub status: &'static str,
    pub data: T,
    pub metadata: ResponseMetadata,
}

/// Aliases of [`ApiResponse`] whose `data` is `serde_json::Value` — i.e.
/// endpoints whose response body the OpenAPI spec does not actually
/// describe. The 2026-09-02 `openapi_lint` only checks that `$ref`s
/// resolve, so an untyped alias passing the lint says nothing about what
/// the endpoint returns.
///
/// The constitution (`docs/constitution.md` § 1) requires every endpoint
/// to be described by an explicit struct. The list below is the source
/// of truth for "what is still untyped"; adding a new entry here means
/// adding another endpoint the spec lies about. Lower it by typing one
/// endpoint and removing its alias from the list.
///
/// `tests/openapi_lint::test_untyped_response_bodies_only_ever_decrease`
/// pins and guards this list.
pub const UNTYPED_API_ALIASES: &[&str] = &["GenericResponse", "PingResponse"];

/// Shared acknowledgement for fire-and-forget "I sent a packet" handlers.
///
/// `kdeconnect.*.request` packets have no reply path on the phone (the
/// phone is too busy to answer), so every request/trigger/send endpoint
/// has historically replied `{"device_id": "...", "sent": true}`. This
/// is that shape, registered as `SentResponseWrapper` for the spec.
///
/// Handlers whose acknowledgement carries more than `device_id`+`sent`
/// (e.g. `mpris_action` adds `player` and `action`, `set_volume` adds
/// `volume`/`muted`) get their own struct; reusing this one would erase
/// information the spec is supposed to describe.
#[derive(Debug, Serialize, ToSchema)]
pub struct SentResponse {
    pub device_id: String,
    pub sent: bool,
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
            // Inside a request: the id the logging middleware minted, so
            // header, body, and log lines agree. Outside one (tests, the
            // CLI rendering an envelope): a fresh id.
            request_id: crate::api::middleware::REQUEST_ID
                .try_with(|id| id.clone())
                .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
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
    /// Pairing lifecycle state, overlaid from the pairing store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair_state: Option<String>,
    /// SAS verification key, present only while a pairing is pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_key: Option<String>,
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
            pair_state: d.pair_state.clone(),
            verification_key: d.verification_key.clone(),
        }
    }
}

/// Query parameters shared by the paginated list endpoints
/// (`GET /api/v1/devices`, `GET /api/v1/notifications`). Parsed leniently:
/// a missing or unparseable value falls back to the default rather than
/// failing the request, so the envelope contract holds (axum's own
/// `Query` rejection is plain text). This struct exists so the two
/// parameters are in the OpenAPI spec (2026-09-06 audit B6: `/devices`
/// honoured them undocumented).
#[derive(Debug, Clone, Copy, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct Pagination {
    /// Page number, 1-based. Default 1.
    pub page: Option<usize>,
    /// Page size. Default 50.
    pub limit: Option<usize>,
}

impl Pagination {
    pub const DEFAULT_PAGE: usize = 1;
    pub const DEFAULT_LIMIT: usize = 50;

    /// Lenient parse from a raw query map: bad values become defaults.
    pub fn from_query(params: &std::collections::HashMap<String, String>) -> Self {
        Self {
            page: params.get("page").and_then(|s| s.parse().ok()),
            limit: params.get("limit").and_then(|s| s.parse().ok()),
        }
    }

    pub fn page(&self) -> usize {
        self.page.unwrap_or(Self::DEFAULT_PAGE)
    }

    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(Self::DEFAULT_LIMIT)
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
pub struct NotificationActionRequest {
    pub action: String,
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

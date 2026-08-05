//! Share API handlers
//!
//! Single Responsibility: Handle file sharing operations.

use axum::extract::{FromRequest, Path, State};
use axum::Json;
use std::sync::Arc;

use crate::api::extractors::{api_err, validate_device_id};
use crate::api::types::ApiResponse;
use crate::app::AppState;
use crate::plugins::share::ReceivedFile;
use crate::protocol::types::Packet;
use crate::utils::errors::Error;

pub async fn list_share_files(
    State(state): State<Arc<AppState>>,
) -> Result<
    Json<ApiResponse<serde_json::Value>>,
    (axum::http::StatusCode, Json<crate::api::types::ApiError>),
> {
    let received: Vec<ReceivedFile> = state.plugins.share.received_files().await;
    let files: Vec<_> = received
        .into_iter()
        .map(|f| {
            serde_json::json!({
                "device_id": f.device_id,
                "filename": f.filename,
                "path": f.path.to_string_lossy(),
                "size": f.size,
            })
        })
        .collect();

    Ok(Json(ApiResponse::ok(serde_json::json!({ "files": files }))))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/share/send",
    tag = "share",
    params(
        ("device_id" = String, Path, description = "Device unique identifier"),
        ("filename" = Option<String>, Query, description = "Filename to send (overrides the multipart part name)")
    ),
    request_body(description = "Raw file bytes, or multipart/form-data with a single file part", content = axum::body::Bytes),
    responses(
        (status = 200, description = "File sent to device", body = serde_json::Value),
        (status = 401, description = "Invalid or missing API key", body = crate::api::types::ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_file_to_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    request: axum::extract::Request,
) -> Result<
    Json<ApiResponse<serde_json::Value>>,
    (axum::http::StatusCode, Json<crate::api::types::ApiError>),
> {
    validate_device_id(&device_id).map_err(api_err)?;

    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    // Each send spawns a payload-transfer server; cap in-flight sends so a
    // burst of API calls cannot spawn unbounded servers (mirrors the
    // receive-side MAX_TRANSFERS_GLOBAL in plugins/share.rs). Held until the
    // transfer completes.
    let _send_permit = SEND_TRANSFER_PERMITS.try_acquire().map_err(|_| {
        api_err(Error::ConnectionError(
            "Too many concurrent file transfers; try again later".to_string(),
        ))
    })?;

    // ?filename= wins over the multipart part name. The body streams
    // straight to a private temp file with the cap enforced on the write
    // path — no full-file buffering in RAM (a 100 MiB upload × 8 send
    // permits must not OOM the daemon). The temp dir's random name avoids
    // same-name request races and makes pre-planted /tmp symlinks
    // irrelevant; keep it alive until the transfer task finishes.
    let temp_dir = tempfile::Builder::new()
        .prefix("rust-connect-share-")
        .tempdir()
        .map_err(|e| {
            api_err(Error::io(
                format!("Failed to create temp directory: {}", e),
                None,
            ))
        })?;
    let temp_path = temp_dir.path().join("payload");
    // A slow-or-stalled client must not hold a send permit forever: the
    // whole upload read gets a 5-minute budget (the phone-side transfer
    // has its own deadline downstream).
    let upload = tokio::time::timeout(
        UPLOAD_READ_TIMEOUT,
        stream_upload_to_file(request, &temp_path),
    )
    .await
    .map_err(|_| api_err(Error::ConnectionError("Upload timed out".to_string())))??;

    let filename = params
        .get("filename")
        .cloned()
        .or(upload.name)
        .unwrap_or_else(|| "shared_file".to_string());

    let safe_filename = crate::plugins::share::SharePlugin::sanitize_filename(&filename)
        .ok_or_else(|| api_err(Error::InvalidRequest("Invalid filename".to_string())))?;

    if upload.size == 0 {
        return Err(api_err(Error::InvalidRequest(
            "Request body is empty".to_string(),
        )));
    }

    let cert_manager = state.cert_manager.clone();
    let transfer =
        crate::protocol::payload_transfer::PayloadTransfer::new(cert_manager, device_id.clone());

    let (transfer_info, handle) = match transfer.send_file(&temp_path).await {
        Ok(result) => result,
        Err(e) => {
            return Err(api_err(Error::Internal(format!(
                "Failed to initiate file transfer: {}",
                e
            ))));
        }
    };

    // payloadSize / payloadTransferInfo are TOP-LEVEL packet fields
    // (NetworkPacket.kt), not body keys.
    let share_packet = Packet::new(
        "kdeconnect.share.request".to_string(),
        serde_json::json!({
            "filename": safe_filename,
        }),
    )
    .with_payload_size(upload.size)
    .with_payload_transfer_info(serde_json::json!({
        "ip": transfer_info.ip,
        "port": transfer_info.port,
        "availableStreams": transfer_info.available_streams,
        "totalStreams": transfer_info.total_streams,
    }));

    if let Err(e) = state
        .connection_manager
        .send_packet(&device_id, &share_packet)
        .await
    {
        handle.abort();
        return Err(api_err(Error::Internal(format!(
            "Failed to send share request: {}",
            e
        ))));
    }

    let result = handle.await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(api_err(Error::Internal(format!(
                "File transfer failed: {}",
                e
            ))));
        }
        Err(e) => {
            return Err(api_err(Error::Internal(format!(
                "File transfer task failed: {}",
                e
            ))));
        }
    }

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "filename": safe_filename,
        "size": upload.size,
        "sent": true,
    }))))
}

/// Explicit upload cap (matches the receive-side default file cap); the raw
/// path previously inherited axum's 2 MiB DefaultBodyLimit, silently
/// rejecting larger sends.
const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

/// A burst of concurrent share sends each spawns a payload-transfer server;
/// cap in-flight sends (same bound as the receive-side MAX_TRANSFERS_GLOBAL).
const MAX_SEND_TRANSFERS: usize = 8;

/// Total budget for reading one upload body to disk; a stalled client must
/// not hold a send permit (and its temp staging) indefinitely.
const UPLOAD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

static SEND_TRANSFER_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_SEND_TRANSFERS);

/// Result of streaming an upload to disk: the multipart part filename
/// (raw bodies: None — the caller falls back to ?filename= or a default)
/// and the bytes written.
#[derive(Debug)]
pub struct UploadMeta {
    pub name: Option<String>,
    pub size: u64,
}

/// Stream an upload body straight to `dest`, enforcing the cap on the
/// write path — no full-file buffering in RAM (a 100 MiB upload x 8 send
/// permits must not OOM the daemon). Two shapes: raw bytes and
/// multipart/form-data (curl -F / browser forms). For multipart, the
/// first part WITH a filename wins: browser forms often lead with plain
/// text fields, and sending the boundary framing as file content is the
/// bug this path exists to prevent.
pub async fn stream_upload_to_file(
    request: axum::extract::Request,
    dest: &std::path::Path,
) -> Result<UploadMeta, (axum::http::StatusCode, Json<crate::api::types::ApiError>)> {
    use tokio::io::AsyncWriteExt;

    // RFC 2045 media types are case-insensitive.
    let is_multipart = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_ascii_lowercase().starts_with("multipart/form-data"));

    let mut file = tokio::fs::File::create(dest).await.map_err(|e| {
        api_err(Error::io(
            format!("Failed to create temp file: {}", e),
            Some(dest.display().to_string()),
        ))
    })?;
    let mut written: u64 = 0;

    let meta = if is_multipart {
        // Multipart's FromRequest impl is state-agnostic (S: Send + Sync).
        let mut multipart = axum::extract::Multipart::from_request(request, &())
            .await
            .map_err(|e| {
                api_err(Error::InvalidRequest(format!(
                    "Invalid multipart body: {}",
                    e
                )))
            })?;
        let field = loop {
            let Some(field) = multipart.next_field().await.map_err(|e| {
                api_err(Error::InvalidRequest(format!(
                    "Invalid multipart field: {}",
                    e
                )))
            })?
            else {
                return Err(api_err(Error::InvalidRequest(
                    "Multipart body has no file part".to_string(),
                )));
            };
            if field.file_name().is_some() {
                break field;
            }
        };
        let name = field.file_name().map(|s| s.to_string());
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    written += chunk.len() as u64;
                    if written > MAX_UPLOAD_BYTES as u64 {
                        return Err(api_err(Error::InvalidRequest(
                            "File exceeds 100 MiB upload cap".to_string(),
                        )));
                    }
                    file.write_all(&chunk).await.map_err(|e| {
                        api_err(Error::io(format!("Failed to write temp file: {}", e), None))
                    })?;
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(api_err(Error::InvalidRequest(format!(
                        "Failed to read upload: {}",
                        e
                    ))));
                }
            }
        }
        UploadMeta {
            name,
            size: written,
        }
    } else {
        use futures::StreamExt;
        let mut stream = request.into_body().into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                api_err(Error::InvalidRequest(format!("Failed to read body: {}", e)))
            })?;
            written += chunk.len() as u64;
            if written > MAX_UPLOAD_BYTES as u64 {
                return Err(api_err(Error::InvalidRequest(
                    "File exceeds 100 MiB upload cap".to_string(),
                )));
            }
            file.write_all(&chunk).await.map_err(|e| {
                api_err(Error::io(format!("Failed to write temp file: {}", e), None))
            })?;
        }
        UploadMeta {
            name: None,
            size: written,
        }
    };

    // tokio's fs::File buffers writes internally (tokio 1.50) — a drop
    // without shutdown can discard buffered data. Flush explicitly before
    // the caller reads the staged file back.
    file.shutdown()
        .await
        .map_err(|e| api_err(Error::io(format!("Failed to flush temp file: {}", e), None)))?;
    Ok(meta)
}

/// A `kdeconnect.share.request` carrying text. Body key from
/// kdeconnect-kde shareplugin.cpp:296-298 and Android SharePlugin.java:340.
fn build_share_text_packet(text: &str) -> Packet {
    Packet::new(
        "kdeconnect.share.request".to_string(),
        serde_json::json!({ "text": text }),
    )
}

/// A `kdeconnect.share.request` carrying a URL. Body key from
/// shareplugin.cpp:282 and SharePlugin.java:340. Upstream chooses between the
/// two keys by whether the shared string parses as a URL
/// (SharePlugin.java:332-341); we let the caller choose by endpoint.
fn build_share_url_packet(url: &str) -> Packet {
    Packet::new(
        "kdeconnect.share.request".to_string(),
        serde_json::json!({ "url": url }),
    )
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/share/text",
    tag = "share",
    params(("device_id" = String, Path, description = "Device unique identifier")),
    request_body = crate::api::types::ShareTextRequest,
    responses(
        (status = 200, description = "Text sent to device", body = serde_json::Value),
        (status = 400, description = "Invalid request", body = crate::api::types::ApiError),
        (status = 401, description = "Invalid or missing API key", body = crate::api::types::ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_text_to_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<crate::api::types::ShareTextRequest>,
) -> Result<
    Json<ApiResponse<serde_json::Value>>,
    (axum::http::StatusCode, Json<crate::api::types::ApiError>),
> {
    validate_device_id(&device_id).map_err(api_err)?;

    if body.text.is_empty() {
        return Err(api_err(Error::InvalidRequest(
            "text must not be empty".to_string(),
        )));
    }
    if body.text.len() > crate::plugins::share::MAX_SHARE_TEXT_BYTES {
        return Err(api_err(Error::InvalidRequest(format!(
            "text exceeds the {} byte cap",
            crate::plugins::share::MAX_SHARE_TEXT_BYTES
        ))));
    }
    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let packet = build_share_text_packet(&body.text);
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "bytes": body.text.len(),
        "sent": true,
    }))))
}

#[utoipa::path(
    post,
    path = "/api/v1/devices/{device_id}/share/url",
    tag = "share",
    params(("device_id" = String, Path, description = "Device unique identifier")),
    request_body = crate::api::types::ShareUrlRequest,
    responses(
        (status = 200, description = "URL sent to device", body = serde_json::Value),
        (status = 400, description = "Invalid or disallowed URL", body = crate::api::types::ApiError),
        (status = 401, description = "Invalid or missing API key", body = crate::api::types::ApiError),
    ),
    security(("api_key" = []))
)]
pub async fn send_url_to_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(body): Json<crate::api::types::ShareUrlRequest>,
) -> Result<
    Json<ApiResponse<serde_json::Value>>,
    (axum::http::StatusCode, Json<crate::api::types::ApiError>),
> {
    validate_device_id(&device_id).map_err(api_err)?;

    let Some(scheme) = crate::plugins::share::allowed_url_scheme(&body.url) else {
        return Err(api_err(Error::InvalidRequest(
            "url must carry an http, https, ftp, ftps, mailto or tel scheme".to_string(),
        )));
    };
    if !state.connection_manager.is_connected(&device_id).await {
        return Err(api_err(Error::InvalidRequest(
            "Device is not connected".to_string(),
        )));
    }

    let packet = build_share_url_packet(&body.url);
    state
        .connection_manager
        .send_packet(&device_id, &packet)
        .await
        .map_err(api_err)?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "device_id": device_id,
        "scheme": scheme,
        "sent": true,
    }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multipart_body(filename: &str, content: &str) -> String {
        format!(
            "--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n{}\r\n--BOUNDARY--\r\n",
            filename, content
        )
    }

    fn multipart_with_leading_text_field(filename: &str, content: &str) -> String {
        format!(
            "--BOUNDARY\r\nContent-Disposition: form-data; name=\"comment\"\r\n\r\njust text\r\n--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n{}\r\n--BOUNDARY--\r\n",
            filename, content
        )
    }

    async fn run_upload(
        request: axum::extract::Request,
    ) -> (UploadMeta, Vec<u8>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let dest = dir.path().join("payload");
        let meta = stream_upload_to_file(request, &dest)
            .await
            .expect("upload streams");
        let bytes = std::fs::read(&dest).expect("read staged file");
        (meta, bytes, dir)
    }

    fn build_request(content_type: &str, body: String) -> axum::extract::Request {
        axum::http::Request::builder()
            .header(axum::http::header::CONTENT_TYPE, content_type)
            .body(axum::body::Body::from(body))
            .expect("build request")
    }

    #[tokio::test]
    async fn test_stream_upload_multipart_extracts_part_filename_and_content() {
        let request = build_request(
            "multipart/form-data; boundary=BOUNDARY",
            multipart_body("holiday photo.jpg", "real file bytes"),
        );
        let (meta, bytes, _dir) = run_upload(request).await;
        assert_eq!(meta.name.as_deref(), Some("holiday photo.jpg"));
        assert_eq!(bytes, b"real file bytes");
        assert_eq!(meta.size, bytes.len() as u64);
    }

    #[tokio::test]
    async fn test_stream_upload_multipart_skips_leading_text_fields() {
        let request = build_request(
            "multipart/form-data; boundary=BOUNDARY",
            multipart_with_leading_text_field("a.txt", "the actual file"),
        );
        let (meta, bytes, _dir) = run_upload(request).await;
        assert_eq!(meta.name.as_deref(), Some("a.txt"));
        assert_eq!(bytes, b"the actual file");
    }

    #[tokio::test]
    async fn test_stream_upload_multipart_content_type_is_case_insensitive() {
        let request = build_request(
            "Multipart/Form-Data; boundary=BOUNDARY",
            multipart_body("a.txt", "bytes"),
        );
        let (meta, bytes, _dir) = run_upload(request).await;
        assert_eq!(meta.name.as_deref(), Some("a.txt"));
        assert_eq!(bytes, b"bytes");
    }

    #[tokio::test]
    async fn test_stream_upload_raw_body_has_no_filename() {
        let request = build_request("application/octet-stream", "raw bytes".to_string());
        let (meta, bytes, _dir) = run_upload(request).await;
        assert_eq!(meta.name, None);
        assert_eq!(bytes, b"raw bytes");
    }

    #[tokio::test]
    async fn test_stream_upload_enforces_100mib_cap() {
        // One byte past the cap: the write path must stop at the limit.
        let oversized = vec![0u8; MAX_UPLOAD_BYTES + 1];
        let request = axum::http::Request::builder()
            .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
            .body(axum::body::Body::from(oversized))
            .expect("build request");
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let dest = dir.path().join("payload");
        let err = stream_upload_to_file(request, &dest)
            .await
            .expect_err("over-cap upload must be rejected");
        let message = format!("{:?}", err.1);
        assert!(message.contains("100 MiB"), "{message}");
    }

    #[tokio::test]
    async fn test_stream_upload_empty_multipart_rejected() {
        let request = build_request(
            "multipart/form-data; boundary=BOUNDARY",
            "--BOUNDARY--\r\n".to_string(),
        );
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let dest = dir.path().join("payload");
        assert!(stream_upload_to_file(request, &dest).await.is_err());
    }

    #[test]
    fn test_share_text_request_deserializes() {
        let body: crate::api::types::ShareTextRequest =
            serde_json::from_str(r#"{"text":"remember the milk"}"#)
                .expect("request body must parse");
        assert_eq!(body.text, "remember the milk");
    }

    #[test]
    fn test_share_url_request_deserializes() {
        let body: crate::api::types::ShareUrlRequest =
            serde_json::from_str(r#"{"url":"https://kde.org/"}"#).expect("request body must parse");
        assert_eq!(body.url, "https://kde.org/");
    }

    #[test]
    fn test_outgoing_text_packet_matches_upstream_shape() {
        let packet = build_share_text_packet("remember the milk");
        assert_eq!(packet.packet_type, "kdeconnect.share.request");
        assert_eq!(packet.body["text"], "remember the milk");
        assert!(
            packet.body.get("filename").is_none(),
            "a text share must not look like a file share"
        );
        assert!(packet.payload_size.is_none());
        assert!(packet.payload_transfer_info.is_none());
    }

    #[test]
    fn test_outgoing_url_packet_matches_upstream_shape() {
        let packet = build_share_url_packet("https://kde.org/");
        assert_eq!(packet.packet_type, "kdeconnect.share.request");
        assert_eq!(packet.body["url"], "https://kde.org/");
        assert!(packet.body.get("text").is_none());
    }

    #[test]
    fn test_outgoing_packets_survive_a_wire_round_trip() {
        for packet in [
            build_share_text_packet("hello"),
            build_share_url_packet("https://kde.org/"),
        ] {
            let bytes = crate::protocol::packet::PacketSerializer::serialize(&packet)
                .expect("serialization of known types cannot fail");
            let parsed = crate::protocol::packet::PacketSerializer::deserialize(&bytes)
                .expect("our own packet must parse");
            assert_eq!(parsed.packet_type, "kdeconnect.share.request");
        }
    }

    #[test]
    fn test_outgoing_url_allowlist_is_the_same_one() {
        assert!(crate::plugins::share::allowed_url_scheme("https://kde.org/").is_some());
        assert!(crate::plugins::share::allowed_url_scheme("file:///etc/passwd").is_none());
    }
}

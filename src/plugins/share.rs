//! Share plugin
//!
//! Single Responsibility: Handle kdeconnect.share.request packets — a file
//! (received via payload transfer), a block of text (staged in the download
//! dir), or a URL (handed to xdg-open behind a scheme allowlist) — plus the
//! kdeconnect.share.request.update batch-progress packet.
//!
//! Upstream reference: kdeconnect-kde plugins/share/shareplugin.cpp:97-239
//! (receive) and :262-299 (send); kdeconnect-android
//! plugins/share/SharePlugin.java:205-346.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tracing::{error, info, warn};

use crate::protocol::payload_transfer::{PayloadTransfer, PayloadTransferInfo};
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;
use super::{PluginEvent, PluginEventBroadcaster};

/// Batch-progress metadata for a multi-file transfer. Both upstream senders
/// put these two keys on every `kdeconnect.share.request` in a batch
/// (CompositeUploadFileJob.java:148-152; compositeuploadjob.cpp:109-110) and
/// also send them alone as `kdeconnect.share.request.update` when files are
/// appended to a batch already in flight
/// (CompositeUploadFileJob.java:186-193; compositeuploadjob.cpp:209-217).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchTotals {
    pub number_of_files: u32,
    pub total_payload_size: u64,
}

/// Read the batch-progress keys off a packet body. Either key may be absent:
/// kde's receiver checks them with two independent `has()` calls
/// (compositefiletransferjob.cpp:59-66), so a packet carrying only one is
/// legal and the missing half reads as zero. `None` means neither was present.
pub fn parse_batch_totals(body: &serde_json::Value) -> Option<BatchTotals> {
    let number_of_files = body.get("numberOfFiles").and_then(|v| v.as_u64());
    let total_payload_size = body.get("totalPayloadSize").and_then(|v| v.as_u64());
    if number_of_files.is_none() && total_payload_size.is_none() {
        return None;
    }
    Some(BatchTotals {
        number_of_files: number_of_files
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        total_payload_size: total_payload_size.unwrap_or(0),
    })
}

/// Cap on the `text` body of a share request, applied in both directions.
/// The wire packet reader caps a whole line at 512 KiB
/// (protocol/packet.rs:76, mirroring Android's MAX_IDENTITY_PACKET_SIZE at
/// LanLinkProvider.java:68) and JSON escaping inflates text, so half of that
/// keeps anything we send comfortably inside the peer's reader.
pub const MAX_SHARE_TEXT_BYTES: usize = 256 * 1024;

/// Schemes we are willing to hand to the desktop's URL handler. Upstream
/// opens whatever it is given (shareplugin.cpp:232-235 calls
/// QDesktopServices::openUrl unconditionally; SharePlugin.java:236-245 fires
/// an unconditional ACTION_VIEW). Handing an arbitrary scheme to the desktop's
/// handler registry on the say-so of a paired device is a
/// remote-code-execution shape we decline to reproduce, so the set is closed.
const ALLOWED_URL_SCHEMES: &[&str] = &["http", "https", "ftp", "ftps", "mailto", "tel"];

/// The lowercased scheme of `url` when it is one we will open, `None`
/// otherwise. Rejects: an empty string, any control character, a missing or
/// empty scheme, an empty remainder, a scheme that does not match RFC 3986's
/// `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`, and anything outside
/// ALLOWED_URL_SCHEMES.
pub fn allowed_url_scheme(url: &str) -> Option<String> {
    if url.is_empty() || url.chars().any(|c| c.is_control()) {
        return None;
    }
    let (scheme, rest) = url.split_once(':')?;
    if rest.is_empty() {
        return None;
    }
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    ALLOWED_URL_SCHEMES
        .contains(&scheme.as_str())
        .then_some(scheme)
}

/// How a shared URL reaches the desktop. A trait so the plugin's decision
/// logic is testable without spawning a process — the same shape
/// clipboard.rs:72-84 uses for its session backends.
#[async_trait::async_trait]
pub trait UrlOpener: Send + Sync {
    /// Hand `url` to the desktop. Returns whether the handoff started; a
    /// missing opener binary is a degrade, never an error.
    async fn open(&self, url: &str) -> bool;
}

/// Production opener: `xdg-open`, the freedesktop equivalent of upstream's
/// QDesktopServices::openUrl (shareplugin.cpp:234).
pub struct XdgOpenUrlOpener;

#[async_trait::async_trait]
impl UrlOpener for XdgOpenUrlOpener {
    async fn open(&self, url: &str) -> bool {
        match tokio::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                // xdg-open forks to the real handler and exits; reap it
                // rather than leaving the child for tokio's orphan queue.
                tokio::spawn(async move {
                    if let Err(e) = child.wait().await {
                        warn!(error = %e, event = "share_url_wait_failed", "xdg-open wait failed");
                    }
                });
                true
            }
            Err(e) => {
                warn!(
                    error = %e,
                    event = "share_url_open_failed",
                    "Failed to spawn xdg-open — surfacing the URL without opening it"
                );
                false
            }
        }
    }
}

pub struct SharePlugin {
    download_dir: PathBuf,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    max_file_size_bytes: u64,
    cert_manager: Option<Arc<crate::protocol::crypto::CertificateManager>>,
    connection_manager: Option<Arc<crate::protocol::ConnectionManager>>,
    global_transfer_permits: Arc<Semaphore>,
    device_transfer_permits: Arc<RwLock<HashMap<String, Arc<Semaphore>>>>,
    plugin_events: Arc<PluginEventBroadcaster>,
    url_opener: Arc<dyn UrlOpener>,
    batch_totals: Arc<std::sync::RwLock<HashMap<String, BatchTotals>>>,
}

#[derive(Debug, Clone)]
pub struct ReceivedFile {
    pub device_id: String,
    pub filename: String,
    pub path: PathBuf,
    pub size: u64,
}

const DEFAULT_MAX_FILE_SIZE_MB: u64 = 100;
/// A paired device can fire unbounded concurrent share requests, each up to
/// the size cap — a disk-fill DoS. Cap in-flight incoming transfers; the
/// share protocol has no rejection reply, so excess requests are logged and
/// dropped.
const MAX_TRANSFERS_PER_DEVICE: usize = 3;
const MAX_TRANSFERS_GLOBAL: usize = 8;
/// Cap on distinct per-device semaphore entries: the map is keyed by
/// peer-supplied device ID, so without a bound a flood of spoofed IDs grows
/// it forever. A full map rejects transfers from unknown devices — 64
/// concurrent devices is far past any real deployment.
const MAX_TRANSFER_DEVICES: usize = 64;
/// Bound on the in-memory received-file history: only the most recent
/// `MAX_RECEIVED_FILES` entries are kept (oldest dropped first), like
/// runcommand's `MAX_EXECUTED_RECORDS`.
const MAX_RECEIVED_FILES: usize = 100;

impl Default for SharePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SharePlugin {
    pub fn new() -> Self {
        Self {
            download_dir: dirs::download_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp/rust-connect-downloads")),
            received_files: Arc::new(RwLock::new(Vec::new())),
            max_file_size_bytes: DEFAULT_MAX_FILE_SIZE_MB * 1024 * 1024,
            cert_manager: None,
            connection_manager: None,
            global_transfer_permits: Arc::new(Semaphore::new(MAX_TRANSFERS_GLOBAL)),
            device_transfer_permits: Arc::new(RwLock::new(HashMap::new())),
            plugin_events: Arc::new(PluginEventBroadcaster::new(16, "plugin")),
            url_opener: Arc::new(XdgOpenUrlOpener),
            batch_totals: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Wire the daemon's certificate manager — required for payload TLS.
    /// Without it, incoming file transfers are refused (never silently
    /// plaintext, never a throwaway cert store).
    pub fn with_cert_manager(
        mut self,
        cert_manager: Arc<crate::protocol::crypto::CertificateManager>,
    ) -> Self {
        self.cert_manager = Some(cert_manager);
        self
    }

    /// Wire the daemon's connection manager — used to resolve the sender's
    /// address when Android sends a `{port}`-only payloadTransferInfo.
    pub fn with_connection_manager(
        mut self,
        connection_manager: Arc<crate::protocol::ConnectionManager>,
    ) -> Self {
        self.connection_manager = Some(connection_manager);
        self
    }

    /// Wire the daemon's plugin-event broadcaster. Without it the plugin
    /// still works; its events just go to a broadcaster nobody subscribed to.
    pub fn with_events(mut self, plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        self.plugin_events = plugin_events;
        self
    }

    /// Replace the URL opener. Production uses `XdgOpenUrlOpener`; tests
    /// substitute a recording one.
    pub fn with_url_opener(mut self, opener: Arc<dyn UrlOpener>) -> Self {
        self.url_opener = opener;
        self
    }

    /// The most recent batch totals reported by `device_id`, if any.
    pub fn batch_totals(&self, device_id: &str) -> Option<BatchTotals> {
        self.batch_totals
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .copied()
    }

    /// Record batch totals if the body carries them, and broadcast the
    /// progress. Called for both `share.request.update` and `share.request`,
    /// since upstream senders put the keys on both.
    fn record_batch_totals(&self, device_id: &str, body: &serde_json::Value) {
        let Some(totals) = parse_batch_totals(body) else {
            return;
        };
        {
            let mut map = self.batch_totals.write().unwrap_or_else(|e| e.into_inner());
            if !map.contains_key(device_id) && map.len() >= MAX_TRANSFER_DEVICES {
                warn!(
                    device_id = %device_id,
                    event = "share_batch_totals_capped",
                    "Too many devices reporting batch totals — dropping this one"
                );
                return;
            }
            map.insert(device_id.to_string(), totals);
        }
        info!(
            device_id = %device_id,
            number_of_files = totals.number_of_files,
            total_payload_size = totals.total_payload_size,
            event = "share_batch_progress",
            "Received share batch totals"
        );
        self.plugin_events.broadcast(PluginEvent::ShareProgress {
            device_id: device_id.to_string(),
            number_of_files: totals.number_of_files,
            total_payload_size: totals.total_payload_size,
        });
    }

    pub fn with_download_dir(mut self, dir: PathBuf) -> Self {
        self.download_dir = dir;
        self
    }

    pub fn with_max_file_size(mut self, bytes: u64) -> Self {
        self.max_file_size_bytes = bytes;
        self
    }

    pub fn download_dir(&self) -> &PathBuf {
        &self.download_dir
    }

    pub fn max_file_size_bytes(&self) -> u64 {
        self.max_file_size_bytes
    }

    pub async fn received_files(&self) -> Vec<ReceivedFile> {
        self.received_files.read().await.clone()
    }

    /// A staging filename in kdeconnect-kde's shape (shareplugin.cpp:201-203 uses
    /// the QTemporaryFile template `kdeconnect-XXXXXX.txt`). Random hex stands in
    /// for mkstemp; the exclusive create in `create_unique_destination` is what
    /// actually guarantees no clobber, so the randomness only keeps the retry
    /// loop short.
    fn text_filename() -> String {
        format!("kdeconnect-{:08x}.txt", rand::random::<u32>())
    }

    pub fn sanitize_filename(filename: &str) -> Option<String> {
        let path = Path::new(filename);

        if path.is_absolute() {
            warn!(filename = %filename, event = "share_path_traversal", "Rejected absolute path");
            return None;
        }

        let components: Vec<_> = path.components().collect();
        for component in &components {
            if matches!(component, std::path::Component::ParentDir) {
                warn!(filename = %filename, event = "share_path_traversal", "Rejected path traversal");
                return None;
            }
        }

        // Flatten to the basename: joining Normal components with '/' would
        // let a peer escape the download dir through a SYMLINKED intermediate
        // component (create_new blocks a symlink only as the final component).
        // kdeconnect-kde flattens the same way.
        let safe_name = components.iter().rev().find_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        });

        match safe_name {
            Some(name) => Some(name.to_string()),
            None => {
                warn!(filename = %filename, event = "share_empty_filename", "Rejected empty filename");
                None
            }
        }
    }

    /// Fill in a missing transfer address from the live link's peer address
    /// (Android may send a `{port}`-only payloadTransferInfo — the sender is
    /// necessarily the connected peer). Returns None, with a warn log, when
    /// no address can be determined.
    async fn resolve_transfer_info(
        &self,
        device_id: &str,
        mut transfer_info: PayloadTransferInfo,
    ) -> Option<PayloadTransferInfo> {
        if transfer_info.ip.is_some() {
            return Some(transfer_info);
        }
        let peer_addr = match &self.connection_manager {
            Some(cm) => cm.get_peer_addr(&device_id.to_string()).await,
            None => None,
        };
        match peer_addr {
            Some(addr) => {
                info!(
                    device_id = %device_id,
                    peer = %addr,
                    event = "share_transfer_ip_fallback",
                    "payloadTransferInfo had no ip — falling back to link peer address"
                );
                transfer_info.ip = Some(addr.ip().to_string());
                Some(transfer_info)
            }
            None => {
                warn!(
                    device_id = %device_id,
                    event = "share_transfer_no_address",
                    "payloadTransferInfo had no ip and no live link address — dropping transfer"
                );
                None
            }
        }
    }

    /// Try to admit one incoming transfer under the global and per-device
    /// concurrency caps. The returned permits must be held by the spawned
    /// receive task for the transfer's lifetime. `None` means the request is
    /// dropped — the share protocol has no rejection reply.
    async fn try_acquire_transfer_permits(
        &self,
        device_id: &str,
    ) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let global = self
            .global_transfer_permits
            .clone()
            .try_acquire_owned()
            .ok()?;
        let device_sem = {
            let mut map = self.device_transfer_permits.write().await;
            match map.get(device_id) {
                Some(sem) => sem.clone(),
                None => {
                    if map.len() >= MAX_TRANSFER_DEVICES {
                        // Drop `global` (releases it) and reject.
                        return None;
                    }
                    map.insert(
                        device_id.to_string(),
                        Arc::new(Semaphore::new(MAX_TRANSFERS_PER_DEVICE)),
                    );
                    map.get(device_id).expect("inserted above").clone()
                }
            }
        };
        // Dropping `global` here releases it if the per-device cap rejects.
        device_sem.try_acquire_owned().ok().map(|d| (global, d))
    }

    // The eight parameters are one logical job; bundling them into a struct
    // would only rename the plumbing. Matches the loader.rs precedent.
    #[allow(clippy::too_many_arguments)]
    async fn receive_file_async(
        received_files: Arc<RwLock<Vec<ReceivedFile>>>,
        download_dir: PathBuf,
        device_id: String,
        filename: String,
        transfer_info: PayloadTransferInfo,
        payload_size: crate::protocol::types::PayloadSize,
        max_file_size_bytes: u64,
        cert_manager: Arc<crate::protocol::crypto::CertificateManager>,
    ) {
        use crate::protocol::types::PayloadSize;

        let safe_filename = match Self::sanitize_filename(&filename) {
            Some(name) => name,
            None => {
                error!(filename = %filename, event = "share_filename_rejected", "Filename rejected for security reasons");
                return;
            }
        };

        // The declared payloadSize is the ONLY pre-flight size bound for a
        // Known transfer. A Stream (payloadSize: -1, parity-checklist.md
        // gap 7) has no declared size to check up front — max_file_size_bytes
        // instead becomes the resource bound the RECEIVE enforces as it
        // reads (see receive_file_unique_streaming).
        if let PayloadSize::Known(size) = payload_size {
            if size > max_file_size_bytes {
                warn!(
                    filename = %filename,
                    size = size,
                    limit = max_file_size_bytes,
                    event = "share_file_too_large",
                    "File exceeds size limit"
                );
                return;
            }
        }

        let dest_path = download_dir.join(&safe_filename);

        if let Err(e) = tokio::fs::create_dir_all(&download_dir).await {
            error!(error = %e, dir = %download_dir.display(), "Failed to create download directory");
            return;
        }

        let transfer = PayloadTransfer::new(cert_manager, device_id.clone());
        let result = match payload_size {
            PayloadSize::Known(size) => {
                transfer
                    .receive_file_unique(&transfer_info, size, &dest_path)
                    .await
            }
            PayloadSize::Stream => {
                info!(
                    filename = %filename,
                    max_bytes = max_file_size_bytes,
                    event = "share_endless_stream_transfer",
                    "Receiving a payloadSize=-1 endless-stream payload (kde sentinel)"
                );
                transfer
                    .receive_file_unique_streaming(&transfer_info, max_file_size_bytes, &dest_path)
                    .await
            }
        };
        match result {
            Ok((bytes, received_path)) => {
                let received_name = received_path
                    .strip_prefix(&download_dir)
                    .unwrap_or(&received_path)
                    .to_string_lossy()
                    .into_owned();
                info!(
                    filename = %received_name,
                    bytes = bytes,
                    path = %received_path.display(),
                    event = "file_received",
                    "File received successfully"
                );
                let mut files = received_files.write().await;
                if files.len() >= MAX_RECEIVED_FILES {
                    files.remove(0);
                }
                files.push(ReceivedFile {
                    device_id,
                    filename: received_name,
                    path: received_path,
                    size: bytes,
                });
            }
            Err(e) => {
                warn!(
                    filename = %safe_filename,
                    error = %e,
                    event = "file_receive_failed",
                    "Failed to receive file"
                );
            }
        }
    }

    /// Stage an incoming shared text block as a file in the download dir and
    /// broadcast it on the plugin event stream.
    ///
    /// kdeconnect-kde also copies the text to the session clipboard
    /// (shareplugin.cpp:161-163) and Android does the same
    /// (SharePlugin.java:249-250). We deliberately do not: our clipboard
    /// backend is Wayland-only (clipboard.rs:97-107), so the behaviour would
    /// vary silently by session type, and the event stream is the surface
    /// this daemon's consumers actually read.
    async fn receive_text(&self, device_id: &str, text: &str) {
        if text.len() > MAX_SHARE_TEXT_BYTES {
            warn!(
                device_id = %device_id,
                size = text.len(),
                limit = MAX_SHARE_TEXT_BYTES,
                event = "share_text_too_large",
                "Shared text exceeds the size cap — dropping"
            );
            return;
        }

        if let Err(e) = tokio::fs::create_dir_all(&self.download_dir).await {
            error!(
                error = %e,
                dir = %self.download_dir.display(),
                event = "share_text_mkdir_failed",
                "Failed to create download directory"
            );
            return;
        }

        let desired = self.download_dir.join(Self::text_filename());
        let (mut file, path) =
            match crate::protocol::payload_transfer::create_unique_destination(&desired).await {
                Ok(created) => created,
                Err(e) => {
                    error!(
                        error = %e,
                        event = "share_text_create_failed",
                        "Failed to create the staging file for shared text"
                    );
                    return;
                }
            };

        use tokio::io::AsyncWriteExt;
        if let Err(e) = file.write_all(text.as_bytes()).await {
            error!(error = %e, event = "share_text_write_failed", "Failed to write shared text");
            let _ = tokio::fs::remove_file(&path).await;
            return;
        }
        // tokio's File buffers internally; a drop without shutdown can
        // discard buffered bytes (same reason api/handlers/share.rs:321-323
        // shuts down explicitly).
        if let Err(e) = file.shutdown().await {
            error!(error = %e, event = "share_text_flush_failed", "Failed to flush shared text");
            let _ = tokio::fs::remove_file(&path).await;
            return;
        }

        let filename = path
            .strip_prefix(&self.download_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        info!(
            device_id = %device_id,
            path = %path.display(),
            bytes = text.len(),
            event = "share_text_received",
            "Received shared text"
        );

        {
            let mut files = self.received_files.write().await;
            if files.len() >= MAX_RECEIVED_FILES {
                files.remove(0);
            }
            files.push(ReceivedFile {
                device_id: device_id.to_string(),
                filename,
                path: path.clone(),
                size: text.len() as u64,
            });
        }

        self.plugin_events.broadcast(PluginEvent::ShareText {
            device_id: device_id.to_string(),
            text: text.to_string(),
            path: path.to_string_lossy().into_owned(),
        });
    }

    /// Hand an incoming shared URL to the desktop and surface it. kde opens
    /// it and stages nothing (shareplugin.cpp:232-235); Android fires
    /// ACTION_VIEW (SharePlugin.java:236-245). A URL outside the scheme
    /// allowlist is still surfaced — the consumer should know a peer tried —
    /// but never opened.
    async fn receive_url(&self, device_id: &str, url: &str) {
        let opened = match allowed_url_scheme(url) {
            Some(scheme) => {
                info!(
                    device_id = %device_id,
                    scheme = %scheme,
                    event = "share_url_received",
                    "Received shared URL"
                );
                self.url_opener.open(url).await
            }
            None => {
                warn!(
                    device_id = %device_id,
                    event = "share_url_scheme_rejected",
                    "Shared URL has no allowed scheme — surfacing without opening"
                );
                false
            }
        };

        self.plugin_events.broadcast(PluginEvent::ShareUrl {
            device_id: device_id.to_string(),
            url: url.to_string(),
            opened,
        });
    }
}

#[async_trait::async_trait]
impl Plugin for SharePlugin {
    fn name(&self) -> &str {
        "share"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.share.request".to_string(),
            // Consumed, never sent: see the module note on why we have no
            // multi-file batch to report on.
            "kdeconnect.share.request.update".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.share.request".to_string()]
    }

    async fn on_disconnected(&self, device_id: &str) {
        self.batch_totals
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type == "kdeconnect.share.request.update" {
            self.record_batch_totals(device_id, &packet.body);
            return Ok(None);
        }
        // Upstream senders also put the totals on every request packet in a
        // batch, so read them here too.
        self.record_batch_totals(device_id, &packet.body);

        let has_file =
            packet.payload_transfer_info.is_some() || packet.body.get("filename").is_some();

        if !has_file {
            if let Some(text) = packet.body.get("text").and_then(|v| v.as_str()) {
                self.receive_text(device_id, text).await;
                return Ok(None);
            }
            if let Some(url) = packet.body.get("url").and_then(|v| v.as_str()) {
                self.receive_url(device_id, url).await;
                return Ok(None);
            }
            warn!(
                device_id = %device_id,
                event = "share_nothing_attached",
                "share.request carried neither a file, text, nor url"
            );
            return Ok(None);
        }

        let filename = packet
            .body
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("shared_file");

        // payloadSize / payloadTransferInfo are TOP-LEVEL packet fields
        // (NetworkPacket.kt), not body keys — reading them from the body is
        // why file transfer never worked.
        let has_payload = packet.payload_transfer_info.is_some();

        info!(
            has_payload = has_payload,
            download_dir = %self.download_dir.display(),
            event = "share_received",
            "Received share request"
        );

        if let Some(transfer_info_val) = packet.payload_transfer_info {
            match serde_json::from_value::<PayloadTransferInfo>(transfer_info_val) {
                Ok(transfer_info) => {
                    let Some(transfer_info) =
                        self.resolve_transfer_info(device_id, transfer_info).await
                    else {
                        return Ok(None);
                    };

                    let Some(payload_size) = packet.payload_size else {
                        warn!(
                            event = "share_missing_payload_size",
                            "payloadTransferInfo present but payloadSize missing — refusing unbounded receive"
                        );
                        return Ok(None);
                    };

                    let Some(cert_manager) = self.cert_manager.clone() else {
                        error!(
                            event = "share_no_cert_manager",
                            "SharePlugin has no certificate manager — refusing payload receive (would be plaintext)"
                        );
                        return Ok(None);
                    };

                    info!(
                        ip = ?transfer_info.ip,
                        port = transfer_info.port,
                        payload_size = %payload_size,
                        event = "share_payload_transfer",
                        "Starting file receive via payload transfer"
                    );

                    let Some(permits) = self.try_acquire_transfer_permits(device_id).await else {
                        warn!(
                            device_id = %device_id,
                            filename = %filename,
                            event = "share_transfer_limit",
                            "Too many concurrent incoming transfers — dropping request"
                        );
                        return Ok(None);
                    };

                    let received_files = self.received_files.clone();
                    let download_dir = self.download_dir.clone();
                    let device_id = device_id.to_string();
                    let filename = filename.to_string();
                    let max_file_size_bytes = self.max_file_size_bytes;

                    tokio::spawn(async move {
                        // Held until the transfer finishes, then released.
                        let _permits = permits;
                        Self::receive_file_async(
                            received_files,
                            download_dir,
                            device_id,
                            filename,
                            transfer_info,
                            payload_size,
                            max_file_size_bytes,
                            cert_manager,
                        )
                        .await;
                    });
                }
                Err(e) => {
                    warn!(error = %e, event = "share_parse_transfer_info", "Failed to parse payloadTransferInfo");
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_share_plugin() {
        let plugin = SharePlugin::new();
        assert_eq!(plugin.name(), "share");
    }

    #[tokio::test]
    async fn test_custom_download_dir() {
        let plugin = SharePlugin::new().with_download_dir(PathBuf::from("/tmp/test-downloads"));
        assert_eq!(plugin.download_dir(), &PathBuf::from("/tmp/test-downloads"));
    }

    #[tokio::test]
    async fn test_handle_share_request() {
        let plugin = SharePlugin::new();
        let packet = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({
                "filename": "photo.jpg",
                "payloadSize": 1024
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());
    }

    #[tokio::test]
    async fn test_handle_share_request_with_transfer_info() {
        let plugin = SharePlugin::new();
        let packet = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({
                "filename": "photo.jpg",
                "payloadSize": 2048,
                "payloadTransferInfo": {
                    "ip": "192.168.1.100",
                    "port": 1740
                }
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());
    }

    #[tokio::test]
    async fn test_handle_share_request_no_filename_defaults() {
        let plugin = SharePlugin::new();
        let packet = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({
                "payloadSize": 512
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());
    }

    #[tokio::test]
    async fn test_sanitize_filename_normal() {
        assert_eq!(
            SharePlugin::sanitize_filename("photo.jpg"),
            Some("photo.jpg".to_string())
        );
        assert_eq!(
            SharePlugin::sanitize_filename("my file.txt"),
            Some("my file.txt".to_string())
        );
    }

    #[tokio::test]
    async fn test_sanitize_filename_flattens_to_basename() {
        // Nested names flatten to the basename so a symlinked intermediate
        // directory in the download dir is never traversed.
        assert_eq!(
            SharePlugin::sanitize_filename("subdir/file.txt"),
            Some("file.txt".to_string())
        );
        assert_eq!(
            SharePlugin::sanitize_filename("a/b/c/evil.txt"),
            Some("evil.txt".to_string())
        );
        assert_eq!(
            SharePlugin::sanitize_filename("./file.txt"),
            Some("file.txt".to_string())
        );
    }

    #[tokio::test]
    async fn test_sanitize_filename_path_traversal() {
        assert!(SharePlugin::sanitize_filename("../etc/passwd").is_none());
        assert!(SharePlugin::sanitize_filename("../../etc/shadow").is_none());
        assert!(SharePlugin::sanitize_filename("foo/../../bar").is_none());
        assert!(SharePlugin::sanitize_filename("..").is_none());
        assert!(SharePlugin::sanitize_filename("./../test").is_none());
    }

    #[tokio::test]
    async fn test_sanitize_filename_absolute() {
        assert!(SharePlugin::sanitize_filename("/etc/passwd").is_none());
        assert!(SharePlugin::sanitize_filename("/tmp/evil").is_none());
    }

    #[tokio::test]
    async fn test_sanitize_filename_empty() {
        assert!(SharePlugin::sanitize_filename("").is_none());
        assert!(SharePlugin::sanitize_filename("/").is_none());
    }

    #[tokio::test]
    async fn test_max_file_size_configurable() {
        let plugin = SharePlugin::new().with_max_file_size(1024);
        assert_eq!(plugin.max_file_size_bytes(), 1024);
    }

    #[tokio::test]
    async fn test_transfer_permits_cap_per_device() {
        let plugin = SharePlugin::new();
        let mut held = Vec::new();
        for _ in 0..MAX_TRANSFERS_PER_DEVICE {
            held.push(
                plugin
                    .try_acquire_transfer_permits("dev1")
                    .await
                    .expect("under the per-device cap"),
            );
        }
        assert!(
            plugin.try_acquire_transfer_permits("dev1").await.is_none(),
            "fourth concurrent transfer from one device must be dropped"
        );
        assert!(
            plugin.try_acquire_transfer_permits("dev2").await.is_some(),
            "a different device is not bound by dev1's cap"
        );
        drop(held);
        assert!(
            plugin.try_acquire_transfer_permits("dev1").await.is_some(),
            "permits are released when the receive task drops them"
        );
    }

    #[tokio::test]
    async fn test_transfer_permits_cap_global() {
        let plugin = SharePlugin::new();
        let mut held = Vec::new();
        // One transfer per device stays under the per-device cap and only
        // exercises the global one.
        for i in 0..MAX_TRANSFERS_GLOBAL {
            held.push(
                plugin
                    .try_acquire_transfer_permits(&format!("dev{i}"))
                    .await
                    .expect("under the global cap"),
            );
        }
        assert!(
            plugin
                .try_acquire_transfer_permits("dev-extra")
                .await
                .is_none(),
            "the ninth concurrent transfer overall must be dropped"
        );
        drop(held);
    }

    // F-2: a `{port}`-only payloadTransferInfo resolves its address from the
    // live link's peer address; the full form is used as-is; with neither an
    // ip nor a link the transfer is dropped.

    const LINK_OUR_ID: &str = "share-client-aaaaaaaaaaaaaaaaaaa";
    const LINK_PEER_ID: &str = "share-peer-aaaaaaaaaaaaaaaaaaaaa";

    /// A ConnectionManager holding a live in-process TLS link to a peer, so
    /// `get_peer_addr` has a real address to hand out.
    async fn cm_with_live_link() -> (
        Arc<crate::protocol::ConnectionManager>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        use crate::protocol::{CertificateManager, ConnectionManager};

        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let certs = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
        certs.init().expect("Value expected to be present");
        let server_cm =
            Arc::new(ConnectionManager::new(certs.clone()).expect("Value expected to be present"));
        server_cm.set_device_identity(LINK_OUR_ID, "Us");
        let client_cm =
            Arc::new(ConnectionManager::new(certs).expect("Value expected to be present"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        let server = server_cm.clone();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("Value expected to be present");
            server
                .accept_test(LINK_PEER_ID.to_string(), stream)
                .await
                .expect("Value expected to be present");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        client_cm
            .connect(&LINK_PEER_ID.to_string(), addr)
            .await
            .expect("Value expected to be present");
        (client_cm, handle, temp)
    }

    fn port_only_info() -> PayloadTransferInfo {
        PayloadTransferInfo {
            ip: None,
            port: 1740,
            available_streams: 0,
            total_streams: 0,
        }
    }

    #[tokio::test]
    async fn test_resolve_transfer_info_port_only_uses_peer_addr() {
        let (cm, server, _t) = cm_with_live_link().await;
        let plugin = SharePlugin::new().with_connection_manager(cm);

        let resolved = plugin
            .resolve_transfer_info(LINK_PEER_ID, port_only_info())
            .await
            .expect("port-only info must resolve against the live link");
        assert_eq!(resolved.ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(resolved.port, 1740);

        server.abort();
    }

    #[tokio::test]
    async fn test_resolve_transfer_info_full_form_used_as_is() {
        // No connection manager wired: the advertised ip must be trusted.
        let plugin = SharePlugin::new();
        let info = PayloadTransferInfo {
            ip: Some("10.0.0.1".to_string()),
            port: 1750,
            available_streams: 1,
            total_streams: 1,
        };
        let resolved = plugin
            .resolve_transfer_info(LINK_PEER_ID, info)
            .await
            .expect("full-form info must pass through");
        assert_eq!(resolved.ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(resolved.port, 1750);
    }

    #[tokio::test]
    async fn test_resolve_transfer_info_no_ip_no_link_drops() {
        let plugin = SharePlugin::new();
        assert!(
            plugin
                .resolve_transfer_info(LINK_PEER_ID, port_only_info())
                .await
                .is_none(),
            "no ip and no live link must drop the transfer"
        );
    }

    /// A real `kdeconnect.share.request` carrying text. Body shape from
    /// kdeconnect-kde shareplugin.cpp:296-298 (`shareText`) and Android
    /// SharePlugin.java:339-341 (the non-URL branch of `share`).
    const WIRE_SHARE_TEXT: &str = r#"{"id":1754179200000,"type":"kdeconnect.share.request","body":{"text":"remember the milk"}}"#;

    fn wire_packet(json: &str) -> Packet {
        crate::protocol::packet::PacketSerializer::deserialize(json.as_bytes())
            .expect("fixture must parse as a wire packet")
    }

    #[tokio::test]
    async fn test_incoming_text_is_written_to_download_dir() {
        let dir = tempfile::TempDir::new().expect("Value expected to be present");
        let plugin = SharePlugin::new().with_download_dir(dir.path().to_path_buf());

        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_TEXT))
            .await
            .expect("handling must not error");

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("download dir exists")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one staged text file");
        let path = entries[0].path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("Value expected to be present");
        assert!(name.starts_with("kdeconnect-"), "{name}");
        assert!(name.ends_with(".txt"), "{name}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("staged file readable"),
            "remember the milk"
        );
    }

    #[tokio::test]
    async fn test_incoming_text_broadcasts_share_text_event() {
        let dir = tempfile::TempDir::new().expect("Value expected to be present");
        let events = Arc::new(crate::plugins::PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = events.subscribe();
        let plugin = SharePlugin::new()
            .with_download_dir(dir.path().to_path_buf())
            .with_events(events);

        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_TEXT))
            .await
            .expect("handling must not error");

        match rx.try_recv().expect("an event must have been broadcast") {
            crate::plugins::PluginEvent::ShareText {
                device_id,
                text,
                path,
            } => {
                assert_eq!(device_id, "phone-1");
                assert_eq!(text, "remember the milk");
                assert!(path.ends_with(".txt"), "{path}");
            }
            other => panic!("Wrong event type: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_incoming_text_appears_in_received_files() {
        let dir = tempfile::TempDir::new().expect("Value expected to be present");
        let plugin = SharePlugin::new().with_download_dir(dir.path().to_path_buf());

        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_TEXT))
            .await
            .expect("handling must not error");

        let files = plugin.received_files().await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].device_id, "phone-1");
        assert_eq!(files[0].size, "remember the milk".len() as u64);
    }

    #[tokio::test]
    async fn test_incoming_text_over_cap_is_refused() {
        let dir = tempfile::TempDir::new().expect("Value expected to be present");
        let plugin = SharePlugin::new().with_download_dir(dir.path().to_path_buf());
        let oversized = "x".repeat(MAX_SHARE_TEXT_BYTES + 1);
        let packet = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({ "text": oversized }),
        );

        plugin
            .handle_packet("phone-1", packet)
            .await
            .expect("handling must not error");

        assert!(
            std::fs::read_dir(dir.path())
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "an over-cap text share must write nothing"
        );
    }

    #[tokio::test]
    async fn test_two_text_shares_do_not_clobber() {
        let dir = tempfile::TempDir::new().expect("Value expected to be present");
        let plugin = SharePlugin::new().with_download_dir(dir.path().to_path_buf());

        for _ in 0..2 {
            plugin
                .handle_packet("phone-1", wire_packet(WIRE_SHARE_TEXT))
                .await
                .expect("handling must not error");
        }

        let count = std::fs::read_dir(dir.path())
            .expect("download dir exists")
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(count, 2, "each text share gets its own file");
    }

    /// A real `kdeconnect.share.request` carrying a URL. Body shape from
    /// kdeconnect-kde shareplugin.cpp:282 (`shareUrl` on a non-local URL)
    /// and Android SharePlugin.java:340 (the `isUrl` branch of `share`).
    const WIRE_SHARE_URL: &str = r#"{"id":1754179200001,"type":"kdeconnect.share.request","body":{"url":"https://kde.org/"}}"#;

    /// Records what it was asked to open instead of spawning anything.
    struct RecordingOpener {
        opened: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl UrlOpener for RecordingOpener {
        async fn open(&self, url: &str) -> bool {
            self.opened
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(url.to_string());
            true
        }
    }

    #[test]
    fn test_allowed_url_scheme_accepts_web_and_contact_schemes() {
        assert_eq!(
            allowed_url_scheme("https://kde.org/"),
            Some("https".to_string())
        );
        assert_eq!(
            allowed_url_scheme("HTTP://Example.COM/x"),
            Some("http".to_string())
        );
        assert_eq!(
            allowed_url_scheme("mailto:someone@example.com"),
            Some("mailto".to_string())
        );
        assert_eq!(allowed_url_scheme("tel:+15551234"), Some("tel".to_string()));
    }

    #[test]
    fn test_allowed_url_scheme_rejects_dangerous_and_malformed() {
        assert_eq!(allowed_url_scheme("file:///etc/passwd"), None);
        assert_eq!(allowed_url_scheme("javascript:alert(1)"), None);
        assert_eq!(allowed_url_scheme("ssh://box/"), None);
        assert_eq!(allowed_url_scheme("kde.org"), None);
        assert_eq!(allowed_url_scheme(""), None);
        assert_eq!(allowed_url_scheme("https:"), None);
        assert_eq!(allowed_url_scheme("1http://x/"), None);
        assert_eq!(allowed_url_scheme("https://kde.org/\nfile:///etc"), None);
    }

    #[tokio::test]
    async fn test_incoming_url_is_opened_and_broadcast() {
        let opened = Arc::new(std::sync::Mutex::new(Vec::new()));
        let events = Arc::new(crate::plugins::PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = events.subscribe();
        let plugin = SharePlugin::new()
            .with_events(events)
            .with_url_opener(Arc::new(RecordingOpener {
                opened: opened.clone(),
            }));

        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_URL))
            .await
            .expect("handling must not error");

        assert_eq!(
            opened.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            &["https://kde.org/".to_string()]
        );
        match rx.try_recv().expect("an event must have been broadcast") {
            crate::plugins::PluginEvent::ShareUrl {
                device_id,
                url,
                opened,
            } => {
                assert_eq!(device_id, "phone-1");
                assert_eq!(url, "https://kde.org/");
                assert!(opened);
            }
            other => panic!("Wrong event type: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_incoming_rejected_url_is_not_opened_but_is_surfaced() {
        let opened = Arc::new(std::sync::Mutex::new(Vec::new()));
        let events = Arc::new(crate::plugins::PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = events.subscribe();
        let plugin = SharePlugin::new()
            .with_events(events)
            .with_url_opener(Arc::new(RecordingOpener {
                opened: opened.clone(),
            }));
        let packet = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({ "url": "file:///etc/passwd" }),
        );

        plugin
            .handle_packet("phone-1", packet)
            .await
            .expect("handling must not error");

        assert!(
            opened.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a rejected scheme must never reach the opener"
        );
        match rx.try_recv().expect("the rejection is still surfaced") {
            crate::plugins::PluginEvent::ShareUrl { url, opened, .. } => {
                assert_eq!(url, "file:///etc/passwd");
                assert!(!opened, "opened must report the truth");
            }
            other => panic!("Wrong event type: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_url_share_writes_no_file() {
        let dir = tempfile::TempDir::new().expect("Value expected to be present");
        let plugin = SharePlugin::new()
            .with_download_dir(dir.path().to_path_buf())
            .with_url_opener(Arc::new(RecordingOpener {
                opened: Arc::new(std::sync::Mutex::new(Vec::new())),
            }));

        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_URL))
            .await
            .expect("handling must not error");

        assert!(
            std::fs::read_dir(dir.path())
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "kde opens a shared URL and stages nothing (shareplugin.cpp:232-235)"
        );
    }

    /// A real `kdeconnect.share.request.update`. Body shape from Android
    /// CompositeUploadFileJob.java:186-193 (`sendUpdatePacket`) and
    /// kdeconnect-kde compositeuploadjob.cpp:209-217.
    const WIRE_SHARE_UPDATE: &str = r#"{"id":1754179200002,"type":"kdeconnect.share.request.update","body":{"numberOfFiles":3,"totalPayloadSize":123456}}"#;

    /// A `kdeconnect.share.request` from inside a multi-file batch.
    const WIRE_SHARE_REQUEST_IN_BATCH: &str = r#"{"id":1754179200003,"type":"kdeconnect.share.request","body":{"filename":"IMG_0001.jpg","numberOfFiles":3,"totalPayloadSize":123456},"payloadSize":41152}"#;

    #[test]
    fn test_parse_batch_totals_reads_both_keys() {
        let body = serde_json::json!({ "numberOfFiles": 3, "totalPayloadSize": 123456u64 });
        assert_eq!(
            parse_batch_totals(&body),
            Some(BatchTotals {
                number_of_files: 3,
                total_payload_size: 123_456
            })
        );
    }

    #[test]
    fn test_parse_batch_totals_tolerates_one_key_missing() {
        assert_eq!(
            parse_batch_totals(&serde_json::json!({ "numberOfFiles": 2 })),
            Some(BatchTotals {
                number_of_files: 2,
                total_payload_size: 0
            })
        );
        assert_eq!(
            parse_batch_totals(&serde_json::json!({ "totalPayloadSize": 99u64 })),
            Some(BatchTotals {
                number_of_files: 0,
                total_payload_size: 99
            })
        );
    }

    #[test]
    fn test_parse_batch_totals_absent_is_none() {
        assert_eq!(
            parse_batch_totals(&serde_json::json!({ "filename": "a.jpg" })),
            None
        );
    }

    #[tokio::test]
    async fn test_update_packet_records_and_broadcasts_progress() {
        let events = Arc::new(crate::plugins::PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = events.subscribe();
        let plugin = SharePlugin::new().with_events(events);

        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_UPDATE))
            .await
            .expect("handling must not error");

        assert_eq!(
            plugin.batch_totals("phone-1"),
            Some(BatchTotals {
                number_of_files: 3,
                total_payload_size: 123_456
            })
        );
        match rx.try_recv().expect("an event must have been broadcast") {
            crate::plugins::PluginEvent::ShareProgress {
                device_id,
                number_of_files,
                total_payload_size,
            } => {
                assert_eq!(device_id, "phone-1");
                assert_eq!(number_of_files, 3);
                assert_eq!(total_payload_size, 123_456);
            }
            other => panic!("Wrong event type: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_totals_on_a_share_request_are_recorded_too() {
        let plugin = SharePlugin::new();

        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_REQUEST_IN_BATCH))
            .await
            .expect("handling must not error");

        assert_eq!(
            plugin.batch_totals("phone-1"),
            Some(BatchTotals {
                number_of_files: 3,
                total_payload_size: 123_456
            })
        );
    }

    #[tokio::test]
    async fn test_batch_totals_cleared_on_disconnect() {
        let plugin = SharePlugin::new();
        plugin
            .handle_packet("phone-1", wire_packet(WIRE_SHARE_UPDATE))
            .await
            .expect("handling must not error");
        assert!(plugin.batch_totals("phone-1").is_some());

        plugin.on_disconnected("phone-1").await;
        assert_eq!(plugin.batch_totals("phone-1"), None);
    }

    #[tokio::test]
    async fn test_update_capability_is_incoming_only() {
        let plugin = SharePlugin::new();
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.share.request.update".to_string()));
        assert!(!plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.share.request.update".to_string()));
    }

    /// Hostile-peer scenario: the peer completes a real link, announces a
    /// file (payloadSize + a `{port}`-only payloadTransferInfo), then never
    /// listens on the payload port. The receive must fail well inside the
    /// designed connect timeout (loopback refuses instantly), the error must
    /// be contained (no panic, no partial file, no recorded file), and the
    /// main link must carry traffic afterwards. The plugin is driven with
    /// the packet exactly as the router would deliver it.
    #[tokio::test]
    async fn test_payload_announced_but_port_never_opens() {
        use crate::protocol::connection::tls;
        use crate::protocol::crypto::CertificateManager;
        use crate::protocol::packet::PacketSerializer;
        use crate::protocol::types::Identity;
        use crate::protocol::ConnectionManager;
        use tokio::io::AsyncWriteExt;

        const DAEMON_ID: &str = "share-daemon-aaaaaaaaaaaaaaaaaaaa";
        const PEER_ID: &str = "share-peer-aaaaaaaaaaaaaaaaaaaaaaa";

        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let cert_manager = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
        cert_manager.init().expect("Value expected to be present");
        let cm = Arc::new(ConnectionManager::new(cert_manager.clone()).expect("cm"));
        cm.set_device_identity(DAEMON_ID, "Us");

        let peer_temp = tempfile::TempDir::new().expect("Value expected to be present");
        let peer_certs = Arc::new(CertificateManager::new(peer_temp.path().to_path_buf()));
        peer_certs.init().expect("Value expected to be present");
        peer_certs
            .ensure_own_certificate(PEER_ID, "Peer")
            .expect("Value expected to be present");

        // The scripted peer dials over real loopback through accept_incoming.
        let listener = Arc::new(
            tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("Value expected to be present"),
        );
        let addr = listener.local_addr().expect("Value expected to be present");
        let accept_cm = cm.clone();
        let accept_listener = listener.clone();
        let accept = tokio::spawn(async move {
            let (stream, _) = accept_listener
                .accept()
                .await
                .expect("Value expected to be present");
            accept_cm.accept_incoming(stream).await
        });
        let peer_identity = Identity::new(
            PEER_ID.to_string(),
            "Peer".to_string(),
            crate::device::DeviceType::Phone,
            vec![],
            vec![],
        );
        let identity_bytes = PacketSerializer::serialize(
            &peer_identity
                .to_tcp_packet()
                .expect("Value expected to be present"),
        )
        .expect("Value expected to be present");
        let mut tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("Value expected to be present");
        tcp.write_all(&identity_bytes)
            .await
            .expect("Value expected to be present");
        tcp.flush().await.expect("Value expected to be present");
        let (mut peer, _) = tls::tls_accept(peer_certs, None, tcp)
            .await
            .expect("Value expected to be present");
        let (device_id, _identity, generation) = accept
            .await
            .expect("Value expected to be present")
            .expect("dial must succeed");

        // A port nothing listens on: bound to learn a free port, then
        // dropped so the connect is refused.
        let dead_port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            probe.local_addr().expect("probe addr").port()
        };

        let download_dir = temp.path().join("downloads");
        std::fs::create_dir_all(&download_dir).expect("Value expected to be present");
        let plugin = SharePlugin::new()
            .with_download_dir(download_dir.clone())
            .with_cert_manager(cert_manager)
            .with_connection_manager(cm.clone());

        // The hostile announcement: payloadSize + `{port}`-only transfer
        // info (the ip falls back to the live link's peer address — the
        // production resolve_transfer_info path).
        let announcement = Packet::new(
            "kdeconnect.share.request".to_string(),
            serde_json::json!({"filename": "evil.bin"}),
        )
        .with_payload_size(4096)
        .with_payload_transfer_info(serde_json::json!({"port": dead_port}));
        let wire =
            PacketSerializer::serialize(&announcement).expect("Value expected to be present");
        peer.write_all(&wire)
            .await
            .expect("Value expected to be present");
        peer.flush().await.expect("Value expected to be present");
        let received = cm
            .recv_packet_current(&device_id, generation)
            .await
            .expect("the announcement must arrive on the link");
        assert_eq!(received.packet_type, "kdeconnect.share.request");

        plugin
            .handle_packet(&device_id, received)
            .await
            .expect("handling the announcement must not error");

        // The spawned receive fails on the refused connect and cleans up:
        // the download dir stays empty and nothing is recorded as received.
        for _ in 0..40 {
            let entries = std::fs::read_dir(&download_dir)
                .expect("Value expected to be present")
                .count();
            assert_eq!(entries, 0, "no partial payload file may survive");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            plugin.received_files().await.is_empty(),
            "a failed transfer must not be recorded as received"
        );

        // The main link is unaffected: traffic still flows.
        let ping =
            PacketSerializer::serialize(&Packet::ping()).expect("Value expected to be present");
        peer.write_all(&ping)
            .await
            .expect("Value expected to be present");
        peer.flush().await.expect("Value expected to be present");
        let packet = cm
            .recv_packet_current(&device_id, generation)
            .await
            .expect("the main link must survive the failed transfer");
        assert_eq!(packet.packet_type, "kdeconnect.ping");
        assert!(cm.is_connected(&device_id).await);
    }
}

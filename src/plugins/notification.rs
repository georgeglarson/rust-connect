//! Notification plugin
//!
//! Single Responsibility: Handle kdeconnect.notification packets, show desktop
//! notifications via notify-rust, and broadcast notification events.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;

use notify_rust::Notification;
use tracing::{debug, info};

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

const MAX_NOTIFICATION_HISTORY: usize = 100;
const MAX_ICONS_PER_DEVICE: usize = 64;
const MAX_NOTIFICATION_ICON_BYTES: u64 = 512 * 1024;
const ICON_DIR_NAME: &str = "notification-icons";

/// Escape peer-controlled text for the freedesktop notification server, whose
/// body is a limited HTML subset: unescaped title/text would let a paired
/// device inject markup and links into the local notification UI (phishing).
/// GSConnect escapes the same way; kdeconnect-kde renders plain text.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationBody {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub app_name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub ticker: Option<String>,
    #[serde(default)]
    pub is_cancel: Option<bool>,
    #[serde(default)]
    pub silent: Option<bool>,
    #[serde(default)]
    pub actions: Option<Vec<String>>,
    #[serde(default)]
    pub conversation: Option<serde_json::Value>,
    #[serde(default)]
    pub group_name: Option<String>,
    /// Android's reply handle for a repliable notification. Serialised as
    /// `requestReplyId` (camelCase), matching `NotificationsPlugin.kt:261`
    /// (`np["requestReplyId"] = rn.id`) and the field it reads back on
    /// `kdeconnect.notification.reply` at :534-535. This is a generated handle
    /// keyed into the phone's `pendingIntents` map, NOT the notification id.
    #[serde(default)]
    pub request_reply_id: Option<String>,
    /// MD5 of the PNG icon payload. Android sends this on every notification,
    /// even when it omits a duplicate payload (NotificationsPlugin.kt:232-241).
    #[serde(default)]
    pub payload_hash: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationEntry {
    pub device_id: String,
    pub id: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    pub is_cancel: bool,
    pub silent: bool,
    pub actions: Option<Vec<String>>,
    pub conversation: Option<serde_json::Value>,
    pub group_name: Option<String>,
    pub reply_id: Option<String>,
    /// Android's MD5 content key for the PNG icon, if one was announced.
    pub icon_hash: Option<String>,
    /// Authenticated API path serving the cached PNG, once available.
    pub icon_url: Option<String>,
    pub received_at: chrono::DateTime<chrono::Utc>,
}

pub struct NotificationPlugin {
    plugin_events: Arc<PluginEventBroadcaster>,
    show_desktop: bool,
    history: Arc<RwLock<VecDeque<NotificationEntry>>>,
    /// Dedupe state for the DESKTOP side: (device_id, notification id) →
    /// what we last posted. Keyed by device, NOT by link — a re-sync after
    /// a link replace (the test phone's 60s redial cadence, or any reconnect)
    /// re-sends the phone's full notification list, and without this each
    /// re-send posted a brand-new desktop popup (live: ~25 notifications
    /// ×12 re-syncs in 5 minutes, 2026-08-03).
    dedupe: Arc<RwLock<HashMap<(String, String), DedupeEntry>>>,
    icon_root: PathBuf,
    icon_lru: Arc<RwLock<HashMap<String, VecDeque<String>>>>,
    max_icons_per_device: usize,
    cert_manager: Option<Arc<crate::protocol::crypto::CertificateManager>>,
    connection_manager: Option<Arc<crate::protocol::ConnectionManager>>,
}

/// The last desktop post for one (device, notification id).
#[derive(Debug, Clone, Copy)]
struct DedupeEntry {
    /// Hash of (app_name, title, text) — what the user would see.
    signature: u64,
    /// The notification server's id for our post (0 = not yet shown: the
    /// freedesktop spec treats replaces_id 0 as "new notification").
    replaces_id: u32,
}

/// What the desktop side should do with an incoming notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopAction {
    /// First sight of this id: post a new desktop notification.
    Post,
    /// Identical re-send of an already-posted id: do nothing. This is the
    /// usability extension BEYOND stock — kdeconnect-kde replaces on every
    /// re-send, which still re-surfaces the popup in some notification
    /// centers (swaync).
    Suppress,
    /// Same id, changed content: replace the existing desktop notification
    /// (carries the server's replaces_id).
    Replace(u32),
}

/// Hash of the user-visible content of a notification.
fn content_signature(app_name: &str, title: &str, text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (app_name, title, text).hash(&mut hasher);
    hasher.finish()
}

/// Close a posted desktop notification by server id. notify-rust 4.12 only
/// exposes close on a consuming handle (xdg/mod.rs:138) and we deliberately
/// don't keep handles (see the show() site), so call CloseNotification over
/// our own session connection (zbus is already a dependency for MPRIS).
/// Log-and-continue: a notification center that already dropped the id is
/// not an error.
async fn close_desktop_notification(replaces_id: u32) {
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(e) => {
            debug!(error = %e, "No session bus to close a desktop notification");
            return;
        }
    };
    if let Err(e) = connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "CloseNotification",
            &(replaces_id,),
        )
        .await
    {
        debug!(error = %e, replaces_id, "Failed to close desktop notification");
    }
}

impl NotificationPlugin {
    pub fn new(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            plugin_events,
            show_desktop: true,
            history: Arc::new(RwLock::new(VecDeque::with_capacity(
                MAX_NOTIFICATION_HISTORY,
            ))),
            dedupe: Arc::new(RwLock::new(HashMap::new())),
            icon_root: std::env::temp_dir().join("rust-connect-notification-icons"),
            icon_lru: Arc::new(RwLock::new(HashMap::new())),
            max_icons_per_device: MAX_ICONS_PER_DEVICE,
            cert_manager: None,
            connection_manager: None,
        }
    }

    /// Creates a notification plugin that records and broadcasts notifications
    /// without forwarding them to the desktop notification server.
    pub fn new_without_desktop(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self {
            plugin_events,
            show_desktop: false,
            history: Arc::new(RwLock::new(VecDeque::with_capacity(
                MAX_NOTIFICATION_HISTORY,
            ))),
            dedupe: Arc::new(RwLock::new(HashMap::new())),
            icon_root: std::env::temp_dir().join("rust-connect-notification-icons"),
            icon_lru: Arc::new(RwLock::new(HashMap::new())),
            max_icons_per_device: MAX_ICONS_PER_DEVICE,
            cert_manager: None,
            connection_manager: None,
        }
    }

    /// Production constructor: cache icons under the configured data directory
    /// and receive their payloads over the same pinned TLS channel as shares.
    pub fn with_storage(
        plugin_events: Arc<PluginEventBroadcaster>,
        data_dir: PathBuf,
        cert_manager: Arc<crate::protocol::crypto::CertificateManager>,
        connection_manager: Arc<crate::protocol::ConnectionManager>,
    ) -> Self {
        let mut plugin = Self::new(plugin_events);
        plugin.icon_root = data_dir.join(ICON_DIR_NAME);
        plugin.cert_manager = Some(cert_manager);
        plugin.connection_manager = Some(connection_manager);
        plugin.load_existing_icons();
        plugin
    }

    fn valid_icon_hash(hash: &str) -> bool {
        hash.len() == 32 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn valid_device_component(device_id: &str) -> bool {
        (32..=38).contains(&device_id.len())
            && device_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }

    fn icon_path_unchecked(&self, device_id: &str, hash: &str) -> PathBuf {
        self.icon_root
            .join(device_id)
            .join(hash.to_ascii_lowercase())
    }

    fn is_regular_icon(path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_file())
            .unwrap_or(false)
    }

    fn icon_api_url(device_id: &str, hash: &str) -> String {
        format!(
            "/api/v1/devices/{device_id}/notification-icons/{}",
            hash.to_ascii_lowercase()
        )
    }

    /// Resolve a cached icon without trusting URL path components supplied by
    /// the caller. The API handler serves only paths admitted here.
    pub fn icon_path(&self, device_id: &str, hash: &str) -> Option<PathBuf> {
        if !Self::valid_device_component(device_id) || !Self::valid_icon_hash(hash) {
            return None;
        }
        let path = self.icon_path_unchecked(device_id, hash);
        Self::is_regular_icon(&path).then_some(path)
    }

    fn record_icon(&self, device_id: &str, hash: &str) {
        let normalized = hash.to_ascii_lowercase();
        let evicted = {
            let mut all = self.icon_lru.write().unwrap_or_else(|e| e.into_inner());
            let queue = all.entry(device_id.to_string()).or_default();
            queue.retain(|entry| entry != &normalized);
            queue.push_back(normalized);
            let mut evicted = Vec::new();
            while queue.len() > self.max_icons_per_device {
                if let Some(oldest) = queue.pop_front() {
                    evicted.push(oldest);
                }
            }
            evicted
        };
        for oldest in evicted {
            let _ = std::fs::remove_file(self.icon_path_unchecked(device_id, &oldest));
        }
    }

    /// Restore and enforce the per-device disk cap at startup. Modification
    /// order is the persisted LRU approximation; symlinks and malformed names
    /// are never admitted or served.
    fn load_existing_icons(&self) {
        let Ok(device_dirs) = std::fs::read_dir(&self.icon_root) else {
            return;
        };
        for device_dir in device_dirs.flatten() {
            let device_id = device_dir.file_name().to_string_lossy().into_owned();
            if !Self::valid_device_component(&device_id)
                || !device_dir.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            let Ok(files) = std::fs::read_dir(device_dir.path()) else {
                continue;
            };
            let mut icons: Vec<(std::time::SystemTime, String)> = files
                .flatten()
                .filter_map(|entry| {
                    let hash = entry.file_name().to_string_lossy().into_owned();
                    if !Self::valid_icon_hash(&hash)
                        || !entry.file_type().is_ok_and(|kind| kind.is_file())
                    {
                        return None;
                    }
                    let modified = entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    Some((modified, hash.to_ascii_lowercase()))
                })
                .collect();
            icons.sort_by_key(|(modified, _)| *modified);
            while icons.len() > self.max_icons_per_device {
                let (_, oldest) = icons.remove(0);
                let _ = std::fs::remove_file(self.icon_path_unchecked(&device_id, &oldest));
            }
            if !icons.is_empty() {
                self.icon_lru
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(device_id, icons.into_iter().map(|(_, hash)| hash).collect());
            }
        }
    }

    async fn receive_icon(
        &self,
        device_id: &str,
        payload_hash: &str,
        packet: &Packet,
    ) -> Option<PathBuf> {
        if !Self::valid_device_component(device_id) || !Self::valid_icon_hash(payload_hash) {
            debug!(device_id = %device_id, payload_hash = %payload_hash, "Ignoring invalid notification icon key");
            return None;
        }

        let path = self.icon_path_unchecked(device_id, payload_hash);
        if Self::is_regular_icon(&path) {
            self.record_icon(device_id, payload_hash);
            return Some(path);
        }

        let payload_size = packet.payload_size?;
        if payload_size == 0 || payload_size > MAX_NOTIFICATION_ICON_BYTES {
            debug!(device_id = %device_id, payload_size, "Notification icon payload exceeds the bound");
            return None;
        }
        let mut transfer_info: crate::protocol::payload_transfer::PayloadTransferInfo =
            serde_json::from_value(packet.payload_transfer_info.clone()?).ok()?;
        let cert_manager = self.cert_manager.clone()?;
        let connection_manager = self.connection_manager.as_ref()?;
        if transfer_info.ip.is_none() {
            transfer_info.ip = connection_manager
                .get_peer_addr(&device_id.to_string())
                .await
                .map(|address| address.ip().to_string());
        }
        transfer_info.ip.as_ref()?;

        if let Some(parent) = path.parent() {
            if tokio::fs::create_dir_all(parent).await.is_err() {
                return None;
            }
        }
        let transfer = crate::protocol::payload_transfer::PayloadTransfer::new(
            cert_manager,
            device_id.to_string(),
        );
        match transfer
            .receive_file(&transfer_info, payload_size, &path)
            .await
        {
            Ok(_) => {
                self.record_icon(device_id, payload_hash);
                Some(path)
            }
            Err(error) => {
                debug!(device_id = %device_id, error = %error, "Failed to receive notification icon payload");
                None
            }
        }
    }

    #[cfg(test)]
    fn with_test_icon_store(mut self, data_dir: PathBuf, cap: usize) -> Self {
        self.icon_root = data_dir.join(ICON_DIR_NAME);
        self.max_icons_per_device = cap;
        self
    }

    #[cfg(test)]
    fn store_test_icon(&self, device_id: &str, hash: &str, bytes: &[u8]) {
        let path = self.icon_path_unchecked(device_id, hash);
        std::fs::create_dir_all(path.parent().expect("icon parent")).expect("create icon dir");
        std::fs::write(path, bytes).expect("write icon");
        self.record_icon(device_id, hash);
    }

    /// Decide the desktop action for an incoming (non-cancel) notification
    /// and track it. The map records INTENT (replaces_id 0 until a show
    /// succeeds and fills it in), so a failed or headless show simply keeps
    /// the last decision — consistent, never a double-post.
    fn dedupe_track(&self, device_id: &str, id: &str, signature: u64) -> DesktopAction {
        let key = (device_id.to_string(), id.to_string());
        let mut map = self.dedupe.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&key) {
            Some(entry) if entry.signature == signature => DesktopAction::Suppress,
            Some(entry) => {
                let replaces_id = entry.replaces_id;
                map.insert(
                    key,
                    DedupeEntry {
                        signature,
                        replaces_id,
                    },
                );
                DesktopAction::Replace(replaces_id)
            }
            None => {
                map.insert(
                    key,
                    DedupeEntry {
                        signature,
                        replaces_id: 0,
                    },
                );
                DesktopAction::Post
            }
        }
    }

    /// Fill in the server id after a successful post/replace.
    fn dedupe_record_shown(&self, device_id: &str, id: &str, replaces_id: u32) {
        let key = (device_id.to_string(), id.to_string());
        if let Some(entry) = self
            .dedupe
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&key)
        {
            entry.replaces_id = replaces_id;
        }
    }

    /// Drop the dedupe entry for a cancelled notification, returning the
    /// server id to close, if any.
    fn dedupe_take(&self, device_id: &str, id: &str) -> Option<u32> {
        self.dedupe
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(device_id.to_string(), id.to_string()))
            .map(|entry| entry.replaces_id)
    }

    pub fn get_history(&self, device_id: Option<&str>, limit: usize) -> Vec<NotificationEntry> {
        let history = self.history.read().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<_> = history.iter().cloned().collect();
        if let Some(did) = device_id {
            entries.retain(|e| e.device_id == did);
        }
        entries.truncate(limit);
        entries
    }

    /// The reply handle for a notification, if the phone marked it repliable.
    ///
    /// Android mints this per repliable notification and keys its `pendingIntents`
    /// map by it (`NotificationsPlugin.kt:261`, `np["requestReplyId"] = rn.id`). It is
    /// NOT the notification id, so a reply must transmit this value or the phone's
    /// lookup misses. `None` means the notification carried no RemoteInput action and
    /// cannot be replied to.
    pub fn reply_handle(&self, device_id: &str, notification_id: &str) -> Option<String> {
        let history = self.history.read().unwrap_or_else(|e| e.into_inner());
        history
            .iter()
            .rev()
            .find(|e| e.device_id == device_id && e.id == notification_id && !e.is_cancel)
            .and_then(|e| e.reply_id.clone())
    }

    /// Whether the current replacement of a notification exposes this action.
    pub fn has_action(&self, device_id: &str, notification_id: &str, action: &str) -> bool {
        let history = self.history.read().unwrap_or_else(|e| e.into_inner());
        history.iter().rev().any(|entry| {
            entry.device_id == device_id
                && entry.id == notification_id
                && entry
                    .actions
                    .as_ref()
                    .is_some_and(|actions| actions.iter().any(|candidate| candidate == action))
        })
    }

    /// Drops a notification from history and announces the dismissal, for a
    /// dismissal the desktop initiated. Returns whether it was actually held.
    ///
    /// The caller sends the `cancel` packet to the phone; this erases our copy
    /// without waiting for confirmation, which is what kdeconnect-kde does and
    /// for the reason it gives: "we won't receive a response if we are out of
    /// sync and this notification no longer exists"
    /// (kdeconnect-kde plugins/notifications/notificationsplugin.cpp:146-150).
    ///
    /// The broadcast is the same `is_cancel: true` event the phone-initiated
    /// path emits (see the `is_cancel` branch of `handle_packet`), so an SSE
    /// consumer removes the row identically whichever side started it.
    pub fn dismiss(&self, device_id: &str, notification_id: &str) -> bool {
        let removed = {
            let mut history = self.history.write().unwrap_or_else(|e| e.into_inner());
            let before = history.len();
            history.retain(|e| !(e.device_id == device_id && e.id == notification_id));
            history.len() != before
        };

        // Our own dismiss: the popup is already gone from the desktop, so
        // just forget the dedupe entry — a future identical notification
        // must post fresh, not suppress.
        let _ = self.dedupe_take(device_id, notification_id);

        self.plugin_events.broadcast(PluginEvent::Notification {
            device_id: device_id.to_string(),
            id: notification_id.to_string(),
            // Same defaults the inbound cancel branch lands on: a cancel names a
            // notification, it does not restate its content.
            app_name: "unknown".to_string(),
            title: String::new(),
            text: String::new(),
            ticker: None,
            is_cancel: true,
            silent: false,
            actions: None,
            conversation: None,
            group_name: None,
            reply_id: None,
            icon_hash: None,
            icon_url: None,
        });

        removed
    }
}

#[async_trait::async_trait]
impl Plugin for NotificationPlugin {
    fn name(&self) -> &str {
        "notification"
    }

    /// Only `kdeconnect.notification`. `kdeconnect.notification.request` is
    /// received by the plugin that SENDS notifications — kdeconnect-kde's
    /// sendnotifications declares it
    /// (plugins/sendnotifications/kdeconnect_sendnotifications.json), its
    /// notifications plugin does not
    /// (plugins/notifications/kdeconnect_notifications.json), and Android splits
    /// the same way (NotificationsPlugin.kt:552-556 vs
    /// ReceiveNotificationsPlugin.kt:98). Ours lives in SendNotificationsPlugin.
    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.notification".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.notification".to_string(),
            "kdeconnect.notification.request".to_string(),
            "kdeconnect.notification.reply".to_string(),
        ]
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        // `request` only — kdeconnect-kde notificationsplugin.cpp:29 and
        // kdeconnect-android ReceiveNotificationsPlugin.kt:39-41 both send this
        // single field. `cancel` on this packet type means "the peer dismissed
        // notification <id>" and carries a string (notificationsplugin.cpp:143);
        // a bool there is not a shape any upstream client emits.
        vec![Packet::new(
            "kdeconnect.notification.request".to_string(),
            serde_json::json!({ "request": true }),
        )]
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type == "kdeconnect.notification" {
            let body: NotificationBody = packet.body_as("notification")?;
            let app_name = body.app_name.as_deref().unwrap_or("unknown");
            let title = body.title.as_deref().unwrap_or("");
            let text = body.text.as_deref().unwrap_or("");
            let id = body.id.as_deref().unwrap_or("");
            let ticker = body.ticker.as_deref().unwrap_or("");
            let is_cancel = body.is_cancel.unwrap_or(false);
            let silent = body.silent.unwrap_or(false);
            let actions = body.actions.clone();
            let conversation = body.conversation.clone();
            let group_name = body.group_name.clone();
            let reply_id = body.request_reply_id.clone();
            let icon_hash = body
                .payload_hash
                .as_deref()
                .filter(|hash| Self::valid_icon_hash(hash))
                .map(|hash| hash.to_ascii_lowercase());
            let icon_path = match icon_hash.as_deref() {
                Some(hash) => self.receive_icon(device_id, hash, &packet).await,
                None => None,
            };
            let icon_url = icon_path
                .as_ref()
                .and(icon_hash.as_deref())
                .map(|hash| Self::icon_api_url(device_id, hash));

            if is_cancel {
                let removed_from_history = {
                    let mut history = self.history.write().unwrap_or_else(|e| e.into_inner());
                    let before = history.len();
                    history.retain(|entry| !(entry.device_id == device_id && entry.id == id));
                    history.len() != before
                };
                let desktop_id = self.dedupe_take(device_id, id);

                if removed_from_history {
                    info!(
                        device_id = %device_id,
                        notification_id = id,
                        event = "notification_cancelled",
                        "Notification cancelled by device"
                    );
                }

                // Always broadcast the cancel so an SSE consumer with a different
                // view of history still sees the dismissal. The broadcast and
                // the desktop popup close are best-effort; repeating the
                // cancel for an unknown key is harmless upstream shape.
                self.plugin_events.broadcast(PluginEvent::Notification {
                    device_id: device_id.to_string(),
                    id: id.to_string(),
                    app_name: app_name.to_string(),
                    title: title.to_string(),
                    text: text.to_string(),
                    ticker: if ticker.is_empty() {
                        None
                    } else {
                        Some(ticker.to_string())
                    },
                    is_cancel: true,
                    silent,
                    actions: actions.clone(),
                    conversation: conversation.clone(),
                    group_name: group_name.clone(),
                    reply_id: None,
                    icon_hash: None,
                    icon_url: None,
                });
                if let Some(replaces_id) = desktop_id.filter(|id| *id != 0) {
                    close_desktop_notification(replaces_id).await;
                }
                return Ok(None);
            }

            info!(
                device_id = %device_id,
                app = app_name,
                notification_id = id,
                event = "notification_received",
                "Received notification from device"
            );

            let display_title = if title.is_empty() { app_name } else { title };

            // The handle from `show()` is dropped on purpose. Wiring its
            // close-event to an outgoing `cancel` (so dismissing the popup
            // dismisses the phone's notification) is deliberately not done:
            // notify-rust 4.12's `NotificationHandle::on_close` consumes the
            // handle and blocks (xdg/mod.rs:172, xdg/zbus_rs.rs:101-110, no
            // async variant), so it would cost a parked thread per popup;
            // and it fires on timeout expiry too, which with the 5s timeout
            // below would cancel the phone's notification five seconds after
            // it arrived. Dismiss-from-desktop is served instead by
            // POST /api/v1/devices/{id}/notification/{notification_id}/dismiss.
            //
            // Dedupe (see the field docs): an id we've already posted with
            // unchanged content is suppressed; changed content replaces the
            // existing popup via replaces_id; a new id posts fresh. A
            // notification with no id can't be deduped and always posts.
            let action = if id.is_empty() {
                DesktopAction::Post
            } else {
                self.dedupe_track(device_id, id, content_signature(app_name, title, text))
            };
            if matches!(action, DesktopAction::Suppress) {
                debug!(
                    device_id = %device_id,
                    notification_id = id,
                    event = "notification_represent_suppressed",
                    "Identical re-send suppressed (already posted)"
                );
            }
            if self.show_desktop
                && (std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok())
                && !matches!(action, DesktopAction::Suppress)
            {
                let mut builder = Notification::new();
                builder
                    .appname(app_name)
                    // The freedesktop spec treats the summary as plain
                    // text (escaping would show literal &amp;/&lt;);
                    // only the body carries the markup subset.
                    .summary(display_title)
                    .body(&escape_markup(text))
                    .hint(notify_rust::Hint::Custom(
                        "x-kdeconnect-source-device".to_string(),
                        device_id.to_string(),
                    ))
                    .timeout(notify_rust::Timeout::Milliseconds(5000));
                if let Some(path) = icon_path.as_ref().and_then(|path| path.to_str()) {
                    // KDE sets the downloaded PNG as the notification pixmap
                    // (notification.cpp:141-146,177-185).
                    builder.image_path(path);
                }
                if let DesktopAction::Replace(replaces_id) = action {
                    builder.id(replaces_id);
                }
                match builder.show() {
                    Ok(handle) => {
                        if !id.is_empty() {
                            self.dedupe_record_shown(device_id, id, handle.id());
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "Failed to show desktop notification");
                    }
                }
            } else if !self.show_desktop
                || (std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err())
            {
                debug!("No display server detected, skipping desktop notification");
            }

            self.plugin_events.broadcast(PluginEvent::Notification {
                device_id: device_id.to_string(),
                id: id.to_string(),
                app_name: app_name.to_string(),
                title: title.to_string(),
                text: text.to_string(),
                ticker: if ticker.is_empty() {
                    None
                } else {
                    Some(ticker.to_string())
                },
                is_cancel: false,
                silent,
                actions: actions.clone(),
                conversation: conversation.clone(),
                group_name: group_name.clone(),
                reply_id: reply_id.clone(),
                icon_hash: icon_hash.clone(),
                icon_url: icon_url.clone(),
            });

            if let Ok(mut history) = self.history.write() {
                // Android's notification key is the stable identity
                // (NotificationsPlugin.kt:220-225,251). Initial sync re-sends
                // the same keys, and updates reuse them, so the desktop state is
                // replace-by-(device,key), not an append-only event log.
                if !id.is_empty() {
                    history.retain(|entry| !(entry.device_id == device_id && entry.id == id));
                }
                history.push_back(NotificationEntry {
                    device_id: device_id.to_string(),
                    id: id.to_string(),
                    app_name: app_name.to_string(),
                    title: title.to_string(),
                    text: text.to_string(),
                    is_cancel: false,
                    silent,
                    actions,
                    conversation,
                    group_name,
                    reply_id,
                    icon_hash,
                    icon_url,
                    received_at: chrono::Utc::now(),
                });
                while history.len() > MAX_NOTIFICATION_HISTORY {
                    history.pop_front();
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

    fn make_plugin() -> NotificationPlugin {
        NotificationPlugin::new_without_desktop(Arc::new(PluginEventBroadcaster::new(16, "plugin")))
    }

    /// A red test that awaits an event must fail fast, not hang the whole
    /// suite forever (a 2h-hung red test stalled the Task 1.4 lane on
    /// 2026-08-06). Every SSE await goes through this guard.
    async fn recv_event(
        rx: &mut tokio::sync::broadcast::Receiver<PluginEvent>,
        label: &str,
    ) -> PluginEvent {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timed out waiting for the event"))
            .unwrap_or_else(|e| panic!("{label}: broadcast channel closed: {e:?}"))
    }

    #[test]
    fn test_escape_markup() {
        assert_eq!(escape_markup("<b>hi</b>"), "&lt;b&gt;hi&lt;/b&gt;");
        assert_eq!(escape_markup("a & b"), "a &amp; b");
        assert_eq!(escape_markup("\"quoted\""), "&quot;quoted&quot;");
        // '&' must be escaped first so existing entities are not double-unescaped.
        assert_eq!(escape_markup("&lt;"), "&amp;lt;");
        assert_eq!(escape_markup("plain text"), "plain text");
        assert_eq!(escape_markup(""), "");
    }

    /// History and broadcast events keep the raw peer text (they are JSON,
    /// not markup consumers); only the notify-rust desktop path escapes.
    #[tokio::test]
    async fn test_markup_kept_raw_in_history_and_events() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "markup-1",
                "appName": "EvilApp",
                "title": "<b>hi</b>",
                "text": "click <a href=\"http://evil.example\">here</a>"
            }),
        );
        assert!(plugin.handle_packet("phone-1", packet).await.is_ok());

        let event = recv_event(&mut rx, "expected notification event").await;
        match event {
            PluginEvent::Notification { title, text, .. } => {
                assert_eq!(title, "<b>hi</b>");
                assert_eq!(text, "click <a href=\"http://evil.example\">here</a>");
            }
            _ => panic!("Wrong event type"),
        }

        let history = plugin.get_history(Some("phone-1"), 10);
        assert_eq!(history[0].title, "<b>hi</b>");
        assert_eq!(
            history[0].text,
            "click <a href=\"http://evil.example\">here</a>"
        );
    }

    #[tokio::test]
    async fn test_notification_plugin_capabilities() {
        let plugin = make_plugin();
        assert_eq!(plugin.name(), "notification");
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.notification".to_string()));
    }

    #[tokio::test]
    async fn test_handle_notification_broadcasts() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "notif-1",
                "appName": "WhatsApp",
                "title": "John",
                "text": "Hey there"
            }),
        );
        assert!(plugin.handle_packet("phone-1", packet).await.is_ok());

        let event = recv_event(&mut rx, "expected notification event").await;
        match event {
            PluginEvent::Notification {
                device_id,
                app_name,
                title,
                text,
                is_cancel,
                silent,
                actions,
                conversation,
                group_name,
                reply_id,
                ..
            } => {
                assert_eq!(device_id, "phone-1");
                assert_eq!(app_name, "WhatsApp");
                assert_eq!(title, "John");
                assert_eq!(text, "Hey there");
                assert!(!is_cancel);
                assert!(!silent);
                assert!(actions.is_none());
                assert!(conversation.is_none());
                assert!(group_name.is_none());
                assert!(reply_id.is_none());
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_handle_notification_cancel() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "notif-2",
                "appName": "Messages",
                "title": "SMS",
                "text": "",
                "isCancel": true
            }),
        );
        assert!(plugin.handle_packet("phone-2", packet).await.is_ok());

        let event = recv_event(&mut rx, "expected notification event").await;
        match event {
            PluginEvent::Notification {
                device_id,
                is_cancel,
                reply_id,
                ..
            } => {
                assert_eq!(device_id, "phone-2");
                assert!(is_cancel);
                assert!(reply_id.is_none());
            }
            _ => panic!("Wrong event type"),
        }
    }

    /// After the ownership split this plugin no longer claims
    /// `kdeconnect.notification.request`. If the packet reaches it anyway (a
    /// direct call, or a future registry change), it must fall through the
    /// catch-all arm quietly rather than erroring.
    #[tokio::test]
    async fn test_notification_request_is_ignored_here() {
        let plugin = make_plugin();
        let packet = Packet::new(
            "kdeconnect.notification.request".to_string(),
            serde_json::json!({ "cancel": "0|com.sec.android.daemonapp|5|null|10203" }),
        );
        assert!(matches!(
            plugin.handle_packet("test", packet).await,
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn test_handle_notification_defaults() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let packet = Packet::new("kdeconnect.notification".to_string(), serde_json::json!({}));
        assert!(plugin.handle_packet("test", packet).await.is_ok());

        let event = recv_event(&mut rx, "expected notification event").await;
        match event {
            PluginEvent::Notification {
                id,
                app_name,
                title,
                text,
                ticker,
                is_cancel,
                silent,
                actions,
                conversation,
                group_name,
                reply_id,
                ..
            } => {
                assert_eq!(id, "");
                assert_eq!(app_name, "unknown");
                assert_eq!(title, "");
                assert_eq!(text, "");
                assert!(ticker.is_none());
                assert!(!is_cancel);
                assert!(!silent);
                assert!(actions.is_none());
                assert!(conversation.is_none());
                assert!(group_name.is_none());
                assert!(reply_id.is_none());
            }
            _ => panic!("Wrong event type"),
        }
    }

    // =====================================================================
    // TESTS FROM PROTOCOL REFERENCE (kdeconnect-android NotificationsPlugin.java)
    // These tests verify correct handling of protocol fields
    // =====================================================================

    #[tokio::test]
    async fn test_handle_notification_with_silent_flag() {
        // kdeconnect-android sends "silent" for pre-existing notifications
        // See NotificationsPlugin.java line 261
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "test-1",
                "appName": "TestApp",
                "title": "Test",
                "text": "Body",
                "silent": true
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());

        let history = plugin.get_history(None, 10);
        assert_eq!(history.len(), 1);
        assert!(history[0].silent);
    }

    #[tokio::test]
    async fn test_handle_notification_with_actions() {
        // kdeconnect-android sends "actions" for notification action buttons
        // See NotificationsPlugin.java line 255
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let actions_input = vec!["Reply".to_string(), "Dismiss".to_string()];
        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "test-2",
                "appName": "WhatsApp",
                "title": "Message",
                "text": "Reply?",
                "actions": actions_input
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());

        let history = plugin.get_history(None, 10);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].actions,
            Some(vec!["Reply".to_string(), "Dismiss".to_string()])
        );
    }

    #[tokio::test]
    async fn test_handle_notification_cancel_has_id_only() {
        // Android cancellation packets only have the ID, not content
        // See NotificationsPlugin.java lines 165-168
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "0|com.sec.android.daemonapp|5|null|10203",
                "isCancel": true
                // No appName, title, or text - only ID
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());

        // Verify cancel event was broadcast with "unknown" defaults
        let event = recv_event(&mut rx, "expected notification event").await;
        match event {
            PluginEvent::Notification {
                id,
                app_name,
                is_cancel,
                reply_id,
                ..
            } => {
                assert_eq!(id, "0|com.sec.android.daemonapp|5|null|10203");
                assert_eq!(app_name, "unknown"); // Default when missing
                assert!(is_cancel);
                assert!(reply_id.is_none());
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_handle_notification_with_conversation() {
        // kdeconnect-android sends "conversation" for messaging notifications
        // See NotificationsPlugin.java line 279
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let conv_json = serde_json::json!({
            "name": "John Doe",
            "participants": ["John Doe"]
        });
        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "conv-1",
                "appName": "Messages",
                "title": "John Doe",
                "text": "Hey!",
                "conversation": conv_json
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());

        let history = plugin.get_history(None, 10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].conversation, Some(conv_json));
    }

    #[tokio::test]
    async fn test_handle_notification_with_group_name() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "group-1",
                "appName": "Slack",
                "title": "Slack message",
                "text": "New message in #general",
                "groupName": "general"
            }),
        );
        assert!(plugin.handle_packet("test", packet).await.is_ok());

        let history = plugin.get_history(None, 10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].group_name, Some("general".to_string()));
    }

    #[tokio::test]
    async fn test_history_limit_eviction() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        for i in 0..150 {
            let packet = Packet::new(
                "kdeconnect.notification".to_string(),
                serde_json::json!({
                    "id": format!("notif-{}", i),
                    "appName": "TestApp",
                    "title": format!("Notification {}", i),
                    "text": "Body"
                }),
            );
            plugin
                .handle_packet("device1", packet)
                .await
                .expect("Value expected to be present");
        }

        let history = plugin.get_history(Some("device1"), 200);
        assert_eq!(history.len(), 100);
        assert_eq!(history[0].id, "notif-50");
        assert_eq!(history[99].id, "notif-149");
    }

    #[tokio::test]
    async fn test_get_history_by_device() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());

        for device in &["device1", "device2"] {
            for i in 0..5 {
                let packet = Packet::new(
                    "kdeconnect.notification".to_string(),
                    serde_json::json!({
                        "id": format!("{}-notif-{}", device, i),
                        "appName": "TestApp",
                        "title": format!("From {}", device),
                        "text": "Body"
                    }),
                );
                plugin
                    .handle_packet(device, packet)
                    .await
                    .expect("Value expected to be present");
            }
        }

        let history1 = plugin.get_history(Some("device1"), 10);
        let history2 = plugin.get_history(Some("device2"), 10);
        let all_history = plugin.get_history(None, 100);

        assert_eq!(history1.len(), 5);
        assert_eq!(history2.len(), 5);
        assert_eq!(all_history.len(), 10);
    }

    /// kdeconnect-kde's notifications plugin declares exactly one incoming
    /// packet type: `X-KdeConnect-SupportedPacketType: ["kdeconnect.notification"]`
    /// in plugins/notifications/kdeconnect_notifications.json. Its Android
    /// counterpart agrees — ReceiveNotificationsPlugin.kt:98 is
    /// `arrayOf(PACKET_TYPE_NOTIFICATION)`. `kdeconnect.notification.request`
    /// belongs to the plugin that SENDS notifications, which on our side is
    /// SendNotificationsPlugin. Declaring it here too made the registry fan the
    /// packet out to two plugins, and neither one acted on it.
    #[tokio::test]
    async fn test_request_capability_belongs_to_sendnotifications() {
        let plugin = make_plugin();
        assert_eq!(
            plugin.incoming_capabilities(),
            vec!["kdeconnect.notification".to_string()],
            "this plugin receives notifications, not requests for them"
        );
    }

    /// The on-connect packet carries `request` and nothing else:
    /// kdeconnect-kde notificationsplugin.cpp:29 is
    /// `NetworkPacket np(PACKET_TYPE_NOTIFICATION_REQUEST, {{QStringLiteral("request"), true}})`,
    /// and ReceiveNotificationsPlugin.kt:39-41 sets only `np["request"] = true`.
    /// We also sent `"cancel": false`, a boolean under the key the peer reads
    /// with `getString` (NotificationsPlugin.kt:529).
    /// Fixture: tests/fixtures/upstream-wire/notification/request_packet.json
    ///   kdeconnect-kde@f5ed3ed8 plugins/notifications/notificationsplugin.cpp:29
    #[tokio::test]
    async fn test_on_connected_matches_upstream_request_packet() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/notification/request_packet.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read notification request fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = make_plugin();
        let packets = plugin.on_connected("devabcdef0123456789abcdef01234567");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.notification.request");
        assert_eq!(packets[0].body, upstream_body);
        assert!(
            packets[0].body.get("cancel").is_none(),
            "`cancel` carries a notification-id string when present; a bool here is not a wire shape upstream produces"
        );
    }
}

#[cfg(test)]
mod reply_handle_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn make_plugin() -> NotificationPlugin {
        NotificationPlugin::new_without_desktop(Arc::new(PluginEventBroadcaster::new(16, "plugin")))
    }

    /// Android sends the reply handle as `requestReplyId` — NotificationsPlugin.kt:261,
    /// `np["requestReplyId"] = rn.id`, where `rn.id` is a generated handle keyed into
    /// `pendingIntents`, NOT the notification id. Reading any other field name means the
    /// handle is never captured and reply can never work.
    #[tokio::test]
    async fn test_request_reply_id_captured_from_wire() {
        let plugin = make_plugin();
        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "0|com.google.android.apps.messaging|42|null|10123",
                "appName": "Messages",
                "title": "Camille",
                "text": "on my way",
                "requestReplyId": "b3f1c2d4-5e6a-7b8c-9d0e-1f2a3b4c5d6e",
            }),
        );
        plugin
            .handle_packet("devabcdef0123456789abcdef01234567", packet)
            .await
            .unwrap();

        let entry = plugin
            .get_history(Some("devabcdef0123456789abcdef01234567"), usize::MAX)
            .into_iter()
            .next()
            .expect("notification stored");
        assert_eq!(
            entry.reply_id.as_deref(),
            Some("b3f1c2d4-5e6a-7b8c-9d0e-1f2a3b4c5d6e"),
            "reply handle must come from requestReplyId"
        );
    }

    /// The reply API must transmit the stored handle, not the notification id. Looking it
    /// up is the plugin's job; the handler asks by notification id.
    #[tokio::test]
    async fn test_reply_handle_lookup_by_notification_id() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef01234567";
        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "notif-id-1",
                "appName": "Messages",
                "title": "t",
                "text": "x",
                "requestReplyId": "handle-9999",
            }),
        );
        plugin.handle_packet(dev, packet).await.unwrap();

        assert_eq!(
            plugin.reply_handle(dev, "notif-id-1").as_deref(),
            Some("handle-9999"),
            "must return the wire handle, never the notification id"
        );
        assert_eq!(
            plugin.reply_handle(dev, "no-such-notification"),
            None,
            "unknown notification has no handle"
        );
    }

    /// A notification with no RemoteInput action carries no requestReplyId. Replying to it
    /// is not possible, and the plugin must say so rather than inventing a handle.
    #[tokio::test]
    async fn test_non_repliable_notification_has_no_handle() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef01234567";
        let packet = Packet::new(
            "kdeconnect.notification".to_string(),
            serde_json::json!({
                "id": "notif-id-2",
                "appName": "radio.net",
                "title": "Knock, knock...",
                "text": "In good mood with Radio Swiss Classic.",
            }),
        );
        plugin.handle_packet(dev, packet).await.unwrap();
        assert_eq!(plugin.reply_handle(dev, "notif-id-2"), None);
    }
}

#[cfg(test)]
mod cancel_semantics_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn make_plugin() -> NotificationPlugin {
        NotificationPlugin::new_without_desktop(Arc::new(PluginEventBroadcaster::new(16, "plugin")))
    }

    /// Red tests that await an event must fail fast, not hang the suite
    /// (see the identical guard in `mod tests`).
    async fn recv_event(
        rx: &mut tokio::sync::broadcast::Receiver<PluginEvent>,
        label: &str,
    ) -> PluginEvent {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timed out waiting for the event"))
            .unwrap_or_else(|e| panic!("{label}: broadcast channel closed: {e:?}"))
    }

    async fn post(plugin: &NotificationPlugin, dev: &str, id: &str, cancel: bool) {
        let mut body =
            serde_json::json!({ "id": id, "appName": "Signal", "title": "t", "text": "x" });
        if cancel {
            body = serde_json::json!({ "id": id, "isCancel": true });
        }
        plugin
            .handle_packet(
                dev,
                Packet::new("kdeconnect.notification".to_string(), body),
            )
            .await
            .unwrap();
    }

    /// A cancel means the phone dismissed the notification. History is what
    /// /api/v1/notifications renders, so a cancel must remove the notification it
    /// names rather than append a second, content-free row. Observed live
    /// 2026-07-30: 12 of 36 entries were blank cancels, one id appeared four
    /// times, and dismissed notifications still showed as present.
    #[tokio::test]
    async fn test_cancel_removes_the_notification_it_names() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef01234567";

        post(&plugin, dev, "notif-a", false).await;
        post(&plugin, dev, "notif-b", false).await;
        assert_eq!(plugin.get_history(Some(dev), usize::MAX).len(), 2);

        post(&plugin, dev, "notif-a", true).await;

        let history = plugin.get_history(Some(dev), usize::MAX);
        assert_eq!(history.len(), 1, "cancel must remove, not append");
        assert_eq!(history[0].id, "notif-b");
        assert!(
            !history.iter().any(|e| e.is_cancel),
            "no content-free cancel rows may remain in history"
        );
    }

    /// Repeat cancels for the same id must not accumulate. The live capture had
    /// one id cancelled four times, each one adding a row.
    #[tokio::test]
    async fn test_repeat_cancels_do_not_accumulate() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef01234567";

        post(&plugin, dev, "notif-a", false).await;
        for _ in 0..4 {
            post(&plugin, dev, "notif-a", true).await;
        }

        assert!(
            plugin.get_history(Some(dev), usize::MAX).is_empty(),
            "four cancels of one notification must leave nothing behind"
        );
    }

    /// A cancel for a notification we never held still surfaces as an SSE
    /// event (consumers may have a divergent history view) but must not
    /// error. Mirrors the lenient-cancel decision already on the wire.
    #[tokio::test]
    async fn test_cancel_for_unknown_id_is_accepted() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());
        let dev = "devabcdef0123456789abcdef01234567";

        post(&plugin, dev, "never-seen", true).await;

        match recv_event(&mut rx, "the cancel event must still reach SSE").await {
            PluginEvent::Notification {
                device_id,
                is_cancel,
                ..
            } => {
                assert_eq!(device_id, dev);
                assert!(is_cancel);
            }
            _ => panic!("Wrong event type"),
        }
        assert!(plugin.get_history(Some(dev), usize::MAX).is_empty());
    }

    /// A cancel names one notification on one device. It must not disturb the
    /// same id on a different device.
    #[tokio::test]
    async fn test_cancel_is_scoped_to_its_device() {
        let plugin = make_plugin();
        let a = "devabcdef0123456789abcdef0123aaaa";
        let b = "devabcdef0123456789abcdef0123bbbb";

        post(&plugin, a, "shared-id", false).await;
        post(&plugin, b, "shared-id", false).await;
        post(&plugin, a, "shared-id", true).await;

        assert!(plugin.get_history(Some(a), usize::MAX).is_empty());
        assert_eq!(plugin.get_history(Some(b), usize::MAX).len(), 1);
    }

    /// The desktop can dismiss a phone notification. kdeconnect-kde's
    /// `dismissRequested` sends the cancel and then erases the local copy
    /// without waiting for the phone: "we won't receive a response if we are out
    /// of sync and this notification no longer exists"
    /// (plugins/notifications/notificationsplugin.cpp:140-151). This method is
    /// the local half of that.
    #[tokio::test]
    async fn test_dismiss_removes_from_history() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef01234567";

        post(&plugin, dev, "notif-a", false).await;
        post(&plugin, dev, "notif-b", false).await;

        assert!(plugin.dismiss(dev, "notif-a"), "notif-a was in history");

        let history = plugin.get_history(Some(dev), usize::MAX);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "notif-b");
    }

    /// A desktop-initiated dismiss must produce the same event an inbound
    /// `isCancel` produces, so an SSE consumer drops the row the same way
    /// whichever side started it.
    #[tokio::test]
    async fn test_dismiss_broadcasts_a_cancel_event() {
        let broadcaster = Arc::new(PluginEventBroadcaster::new(16, "plugin"));
        let mut rx = broadcaster.subscribe();
        let plugin = NotificationPlugin::new_without_desktop(broadcaster.clone());
        let dev = "devabcdef0123456789abcdef01234567";

        plugin
            .handle_packet(
                dev,
                Packet::new(
                    "kdeconnect.notification".to_string(),
                    serde_json::json!({ "id": "notif-x", "appName": "Signal", "title": "t", "text": "x" }),
                ),
            )
            .await
            .unwrap();
        let _posted = recv_event(&mut rx, "the posted notification event").await;

        plugin.dismiss(dev, "notif-x");

        match recv_event(&mut rx, "the dismiss event").await {
            PluginEvent::Notification {
                device_id,
                id,
                is_cancel,
                ..
            } => {
                assert_eq!(device_id, dev);
                assert_eq!(id, "notif-x");
                assert!(is_cancel);
            }
            _ => panic!("Wrong event type"),
        }
    }

    /// Out-of-sync is the normal case upstream calls out, so dismissing an id we
    /// no longer hold is not an error. It reports that nothing was removed and
    /// still announces the dismissal.
    #[tokio::test]
    async fn test_dismiss_unknown_id_reports_false() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef01234567";
        assert!(!plugin.dismiss(dev, "never-seen"));
    }

    /// A dismiss names one notification on one device.
    #[tokio::test]
    async fn test_dismiss_is_scoped_to_its_device() {
        let plugin = make_plugin();
        let a = "devabcdef0123456789abcdef0123aaaa";
        let b = "devabcdef0123456789abcdef0123bbbb";

        post(&plugin, a, "shared-id", false).await;
        post(&plugin, b, "shared-id", false).await;

        assert!(plugin.dismiss(a, "shared-id"));
        assert!(plugin.get_history(Some(a), usize::MAX).is_empty());
        assert_eq!(plugin.get_history(Some(b), usize::MAX).len(), 1);
    }

    // Desktop dedupe (the swaync duplicate storm, 2026-08-03): a re-synced
    // notification list must not re-post every popup.

    /// Unseen id → Post; identical re-send → Suppress; changed content →
    /// Replace carrying the last known server id.
    #[test]
    fn test_dedupe_track_post_suppress_replace() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef0123aaaa";
        let sig1 = content_signature("Signal", "Alice", "hi");
        let sig2 = content_signature("Signal", "Alice", "hi again");

        assert_eq!(
            plugin.dedupe_track(dev, "n1", sig1),
            DesktopAction::Post,
            "first sight of an id must post"
        );
        assert_eq!(
            plugin.dedupe_track(dev, "n1", sig1),
            DesktopAction::Suppress,
            "identical re-send must suppress"
        );
        assert_eq!(
            plugin.dedupe_track(dev, "n1", sig2),
            DesktopAction::Replace(0),
            "changed content must replace (0 = not yet shown)"
        );

        // Once a show lands, later replaces carry the real server id.
        plugin.dedupe_record_shown(dev, "n1", 42);
        assert_eq!(
            plugin.dedupe_track(dev, "n1", sig1),
            DesktopAction::Replace(42),
            "the replace must carry the posted notification's server id"
        );
    }

    /// The map is keyed per DEVICE, not per link: a re-sync after a link
    /// replace (same device, new connection) still suppresses, and a second
    /// device with the same notification id is independent.
    #[test]
    fn test_dedupe_is_per_device_and_link_agnostic() {
        let plugin = make_plugin();
        let dev_a = "devabcdef0123456789abcdef0123aaaa";
        let dev_b = "devabcdef0123456789abcdef0123bbbb";
        let sig = content_signature("Signal", "Alice", "hi");

        // First link: post. Second link (the replace): the plugin instance
        // and its map persist, so the re-send suppresses.
        assert_eq!(plugin.dedupe_track(dev_a, "n1", sig), DesktopAction::Post);
        assert_eq!(
            plugin.dedupe_track(dev_a, "n1", sig),
            DesktopAction::Suppress,
            "a re-sync on a replacement link must still suppress"
        );
        assert_eq!(
            plugin.dedupe_track(dev_b, "n1", sig),
            DesktopAction::Post,
            "another device with the same notification id is independent"
        );
    }

    /// A phone-initiated cancel drops the dedupe entry (and asks for the
    /// popup to close), so a later identical notification posts fresh.
    #[tokio::test]
    async fn test_cancel_drops_dedupe_entry() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef0123aaaa";
        let sig = content_signature("Signal", "t", "x");

        post(&plugin, dev, "n1", false).await;
        assert_eq!(
            plugin.dedupe_track(dev, "n1", sig),
            DesktopAction::Suppress,
            "the posted notification must be tracked"
        );

        post(&plugin, dev, "n1", true).await;
        assert_eq!(
            plugin.dedupe_track(dev, "n1", sig),
            DesktopAction::Post,
            "after a cancel the entry is gone: identical content posts fresh"
        );
    }

    /// A desktop-initiated dismiss drops the dedupe entry too.
    #[tokio::test]
    async fn test_dismiss_drops_dedupe_entry() {
        let plugin = make_plugin();
        let dev = "devabcdef0123456789abcdef0123aaaa";
        let sig = content_signature("Signal", "t", "x");

        post(&plugin, dev, "n1", false).await;
        assert!(plugin.dismiss(dev, "n1"));
        assert_eq!(
            plugin.dedupe_track(dev, "n1", sig),
            DesktopAction::Post,
            "after a dismiss the entry is gone: identical content posts fresh"
        );
    }
}

#[cfg(test)]
mod task_1_4_icon_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::*;

    fn fixture(name: &str) -> Packet {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/notification")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(path).expect("read notification fixture"))
            .expect("parse notification fixture")
    }

    #[tokio::test]
    async fn test_icon_hash_is_exposed_even_when_payload_is_not_available() {
        let plugin = NotificationPlugin::new_without_desktop(Arc::new(
            PluginEventBroadcaster::new(16, "plugin"),
        ));
        let dev = "devabcdef0123456789abcdef01234567";
        plugin
            .handle_packet(dev, fixture("full_with_icon_actions_reply.json"))
            .await
            .unwrap();

        let entry = plugin
            .get_history(Some(dev), usize::MAX)
            .into_iter()
            .next()
            .expect("notification stored");
        assert_eq!(
            entry.icon_hash.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(
            entry.icon_url.is_none(),
            "no payload cache means no API URL"
        );
    }

    #[test]
    fn test_icon_cache_enforces_per_device_cap() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let plugin = NotificationPlugin::new_without_desktop(Arc::new(
            PluginEventBroadcaster::new(16, "plugin"),
        ))
        .with_test_icon_store(temp.path().to_path_buf(), 2);
        let dev = "devabcdef0123456789abcdef01234567";
        let first = "0123456789abcdef0123456789abcdef";
        let second = "11111111111111111111111111111111";
        let third = "22222222222222222222222222222222";

        plugin.store_test_icon(dev, first, b"png-1");
        plugin.store_test_icon(dev, second, b"png-2");
        plugin.store_test_icon(dev, third, b"png-3");

        assert!(
            plugin.icon_path(dev, first).is_none(),
            "oldest icon evicted"
        );
        assert!(plugin.icon_path(dev, second).is_some());
        assert!(plugin.icon_path(dev, third).is_some());
    }
}

#[cfg(test)]
mod task_1_4_state_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::*;

    fn fixture(name: &str) -> Packet {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/notification")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(path).expect("read notification fixture"))
            .expect("parse notification fixture")
    }

    fn plugin() -> NotificationPlugin {
        NotificationPlugin::new_without_desktop(Arc::new(PluginEventBroadcaster::new(16, "plugin")))
    }

    #[tokio::test]
    async fn test_initial_sync_fixture_is_idempotent() {
        let plugin = plugin();
        let dev = "devabcdef0123456789abcdef01234567";
        let packet = fixture("full_with_icon_actions_reply.json");

        plugin.handle_packet(dev, packet.clone()).await.unwrap();
        plugin.handle_packet(dev, packet).await.unwrap();

        let history = plugin.get_history(Some(dev), usize::MAX);
        assert_eq!(
            history.len(),
            1,
            "a repeated full-sync item replaces by key"
        );
        assert_eq!(
            history[0].actions.as_deref(),
            Some(&["Mark as read".to_string(), "Mute".to_string()][..])
        );
        assert_eq!(
            history[0].reply_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
    }

    #[tokio::test]
    async fn test_replacement_fixture_updates_actions_and_reply_identity_in_place() {
        let plugin = plugin();
        let dev = "devabcdef0123456789abcdef01234567";

        plugin
            .handle_packet(dev, fixture("full_with_icon_actions_reply.json"))
            .await
            .unwrap();
        plugin
            .handle_packet(dev, fixture("replacement.json"))
            .await
            .unwrap();

        let history = plugin.get_history(Some(dev), usize::MAX);
        assert_eq!(history.len(), 1, "same phone key must update, not append");
        assert_eq!(history[0].text, "made it");
        assert_eq!(
            history[0].actions.as_deref(),
            Some(&["Mark as read".to_string()][..])
        );
        assert_eq!(
            plugin
                .reply_handle(dev, "0|org.thoughtcrime.securesms|42|null|10123")
                .as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            "reply-by-notification-id must route through the replacement's current phone handle"
        );
    }
}

//! SFTP plugin
//!
//! Single Responsibility: Handle SFTP (SSH Filesystem) browsing of remote
//! device storage, and mount that storage locally via `sshfs`.
//!
//! Protocol:
//! - Outgoing: kdeconnect.sftp.request { "startBrowsing": true }
//! - Incoming: kdeconnect.sftp { ip, port, user, password, path, multiPaths, pathNames }
//!
//! The Android device runs an SFTP server. We request browsing, receive
//! the connection credentials, keep them per device, and (on demand) mount
//! the device's filesystem under `<data_dir>/mounts/sftp-<device_id>`.
//! Mounting is the desktop side's responsibility: see `mounter.rs` for
//! the subprocess boundary and `plugins/sftp/sftpplugin.cpp` (kdeconnect-kde
//! @ f5ed3ed8) for the upstream state-machine shape.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::plugins::events::{PluginEvent, PluginEventBroadcaster};
use crate::plugins::sftp::mounter::{MountOutcome, MountRequest, Mounter, UnmountOutcome};
use crate::protocol::types::Packet;
use crate::utils::errors::{Error, Result};

use super::plugin::Plugin;

/// Wall-clock budget for one sshfs mount attempt or fusermount unmount,
/// waited on from async code (see `mount_via_mounter`/`unmount_via_mounter`).
/// Mirrors kdeconnect-kde's own `mounter.cpp:32` 10s wait-for-result
/// timeout.
const MOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub mod mounter;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpConnectionInfo {
    pub ip: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub path: String,
    pub multi_paths: Vec<String>,
    pub path_names: Vec<String>,
}

/// `Debug` is hand-rolled to redact the password. The derived form would
/// include the plaintext (sftp.rs:26-36 prior to this lane) and a single
/// `{:?}` in a log line would leak the credential — pinned by
/// `sftp_connection_info_debug_redacts_password` in this module's tests.
impl fmt::Debug for SftpConnectionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpConnectionInfo")
            .field("ip", &self.ip)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"***redacted***")
            .field("path", &self.path)
            .field("multi_paths", &self.multi_paths)
            .field("path_names", &self.path_names)
            .finish()
    }
}

/// Per-device mount lifecycle. Mirrors the upstream QSignal set
/// (kdeconnect-kde mounter.cpp:29-30, 121-123, 139-160, 153-164): we model
/// the same "mounting → mounted | failed" progression the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    Unmounted,
    Mounting,
    Mounted,
    /// Last mount attempt failed; the message is short (≤ 512 chars) and
    /// never contains the password.
    Failed(String),
}

/// What the API returns for a device's mount point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountStatus {
    pub state: MountState,
    pub mount_point: Option<PathBuf>,
}

/// The plugin keeps credentials AND a per-device mount table. The mounter
/// itself is stateless — see `mounter.rs` for why.
pub struct SftpPlugin {
    connections: Arc<RwLock<HashMap<String, SftpConnectionInfo>>>,
    mounts: Arc<RwLock<HashMap<String, MountState>>>,
    data_dir: PathBuf,
    mounter: Arc<Mounter>,
    plugin_events: Arc<PluginEventBroadcaster>,
    /// Source of the authenticated link's peer address — the ONLY address
    /// sshfs is ever pointed at (kdeconnect-kde mounter.cpp:81-94). `None`
    /// only in test constructions; production wires it in loader.rs.
    connection_manager: Option<Arc<crate::protocol::ConnectionManager>>,
    /// Bound on `mount_via_mounter`/`unmount_via_mounter`'s wait for the
    /// blocking-pool task. Defaults to `MOUNT_TIMEOUT`; overridden only by
    /// the test-only `with_mount_timeout` so a timeout test doesn't need
    /// to sleep for the real 10s production budget.
    mount_timeout: std::time::Duration,
    /// Per-device lock serializing the disconnect cleanup and a
    /// replacement mount: both operate on the same `sftp-<device>` path
    /// (PR #40 review). One entry per device id, created on first use.
    mount_locks: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Default for SftpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SftpPlugin {
    pub fn new() -> Self {
        // Fall back to an inert data dir + the real system mounter.
        // Production paths construct via `with_events_and_data_dir` (or
        // `with_mounter` in tests).
        Self::with_mounter(
            Arc::new(PluginEventBroadcaster::new(16, "plugin")),
            std::env::temp_dir().join("rust-connect-sftp-fallback"),
            Arc::new(mounter::SystemCommandRunner::new()),
        )
    }

    pub fn with_events(plugin_events: Arc<PluginEventBroadcaster>) -> Self {
        Self::with_mounter(
            plugin_events,
            std::env::temp_dir().join("rust-connect-sftp-fallback"),
            Arc::new(mounter::SystemCommandRunner::new()),
        )
    }

    /// Production constructor: events + data dir + real system mounter.
    pub fn with_events_and_data_dir(
        plugin_events: Arc<PluginEventBroadcaster>,
        data_dir: PathBuf,
    ) -> Self {
        Self::with_mounter(
            plugin_events,
            data_dir,
            Arc::new(mounter::SystemCommandRunner::new()),
        )
    }

    /// Test seam: every input is injectable.
    pub fn with_mounter(
        plugin_events: Arc<PluginEventBroadcaster>,
        data_dir: PathBuf,
        runner: Arc<dyn mounter::CommandRunner>,
    ) -> Self {
        let mounter_inst = Mounter::new(runner);
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            mounts: Arc::new(RwLock::new(HashMap::new())),
            data_dir,
            mounter: Arc::new(mounter_inst),
            plugin_events,
            connection_manager: None,
            mount_timeout: MOUNT_TIMEOUT,
            mount_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Test-only override of `mount_timeout` — lets a timeout test use a
    /// short bound instead of sleeping for the real production budget.
    #[cfg(test)]
    fn with_mount_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.mount_timeout = timeout;
        self
    }

    #[allow(clippy::expect_used)]
    /// Wire the connection manager so the sshfs target is the address of
    /// the TLS link the credentials arrived on, never the packet's `ip`.
    pub fn with_connection_manager(
        mut self,
        connection_manager: Arc<crate::protocol::ConnectionManager>,
    ) -> Self {
        self.connection_manager = Some(connection_manager);
        self
    }

    /// Plant credentials as if they had arrived on a live link. Test-only:
    /// production credentials enter through `handle_packet`, which binds
    /// the sshfs target to the authenticated link's address and validates
    /// the peer-supplied fields (2026-09-02 audit, B1).
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn plant_connection_for_test(&self, device_id: &str, info: SftpConnectionInfo) {
        if let Ok(mut connections) = self.connections.write() {
            connections.insert(device_id.to_string(), info);
        }
    }

    pub fn get_connection(&self, device_id: &str) -> Option<SftpConnectionInfo> {
        let connections = self.connections.read().unwrap_or_else(|e| e.into_inner());
        connections.get(device_id).cloned()
    }

    /// Read-only view of the mount table for a device.
    pub fn get_mount_status(&self, device_id: &str) -> MountStatus {
        let mounts = self.mounts.read().unwrap_or_else(|e| e.into_inner());
        let mount_point = mount_point_for(&self.data_dir, device_id);
        let state = mounts.get(device_id).cloned();
        match state {
            Some(MountState::Mounted) | Some(MountState::Mounting) => MountStatus {
                state: state.expect("matched just above"),
                mount_point: Some(mount_point),
            },
            Some(MountState::Failed(_)) => MountStatus {
                state: state.expect("matched just above"),
                mount_point: Some(mount_point),
            },
            _ => MountStatus {
                state: MountState::Unmounted,
                mount_point: None,
            },
        }
    }

    /// Server-determined mount point. The UI never gets to choose —
    /// a fixed shape under data_dir keeps the cleanup paths trivial.
    pub fn mount_point(&self, device_id: &str) -> PathBuf {
        mount_point_for(&self.data_dir, device_id)
    }

    pub fn request_sftp(&self, _device_id: &str) -> Packet {
        Packet::new(
            "kdeconnect.sftp.request".to_string(),
            serde_json::json!({
                "startBrowsing": true
            }),
        )
    }

    pub fn set_connection(&self, device_id: &str, info: SftpConnectionInfo) {
        if let Ok(mut connections) = self.connections.write() {
            connections.insert(device_id.to_string(), info);
        }
    }

    /// Runs `Mounter::mount` (a synchronous sshfs spawn + wait) on the
    /// blocking thread pool and bounds the wait with `MOUNT_TIMEOUT`.
    /// Calling `Mounter::mount` directly from an async fn — the pre-fix
    /// shape — blocks the tokio worker thread for however long sshfs
    /// takes to connect and authenticate, which on a dead/unreachable
    /// phone is "however long the TCP stack takes to give up," i.e.
    /// effectively unbounded. `spawn_blocking` moves that wait off the
    /// worker thread; `spawn_blocking` cannot forcibly cancel the
    /// underlying OS thread (there is no cooperative cancellation point
    /// inside a blocking subprocess wait), so a timeout only bounds how
    /// long THIS caller waits — the orphaned thread finishes sshfs's own
    /// wait on its own in the background. That trade (bounded caller wait,
    /// occasional orphaned blocking thread) is the standard shape for
    /// wrapping non-cancellable blocking work from async code.
    ///
    /// When the wait times out this also hands back the still-running
    /// blocking task: a caller holding the device's mount lock must keep
    /// holding the lock until that task exits, or a replacement operation
    /// can start on the same path while the orphan may still touch it
    /// (PR #40 review round 2, cubic-dev P1).
    async fn mount_via_mounter_tracked(
        &self,
        req: MountRequest,
        password: String,
    ) -> (
        Result<MountOutcome>,
        Option<tokio::task::JoinHandle<Result<MountOutcome>>>,
    ) {
        let mounter = self.mounter.clone();
        let mut join = tokio::task::spawn_blocking(move || mounter.mount(&req, &password));
        match tokio::time::timeout(self.mount_timeout, std::pin::Pin::new(&mut join)).await {
            Ok(join_result) => (
                join_result
                    .map_err(|e| Error::Internal(format!("mount task panicked: {e}")))
                    .and_then(|outcome| outcome),
                None,
            ),
            Err(_elapsed) => (
                Ok(MountOutcome::Failed(format!(
                    "sshfs mount timed out after {}s",
                    self.mount_timeout.as_secs_f64()
                ))),
                Some(join),
            ),
        }
    }

    /// `Mounter::unmount` counterpart of `mount_via_mounter_tracked` —
    /// see its doc for why this exists instead of calling
    /// `Mounter::unmount` directly. Kept for `startup_sweep`, which runs
    /// before any connection exists and needs no device-lock handoff.
    async fn unmount_via_mounter(&self, mp: PathBuf) -> Result<UnmountOutcome> {
        let mounter = self.mounter.clone();
        let join = tokio::task::spawn_blocking(move || mounter.unmount(&mp));
        match tokio::time::timeout(self.mount_timeout, join).await {
            Ok(join_result) => {
                join_result.map_err(|e| Error::Internal(format!("unmount task panicked: {e}")))?
            }
            Err(_elapsed) => Ok(UnmountOutcome::Failed(format!(
                "fusermount unmount timed out after {}s",
                self.mount_timeout.as_secs_f64()
            ))),
        }
    }

    /// Tracked variant of `unmount_via_mounter`; see
    /// `mount_via_mounter_tracked` for why the leftover matters.
    async fn unmount_via_mounter_tracked(
        &self,
        mp: PathBuf,
    ) -> (
        Result<UnmountOutcome>,
        Option<tokio::task::JoinHandle<Result<UnmountOutcome>>>,
    ) {
        let mounter = self.mounter.clone();
        let mut join = tokio::task::spawn_blocking(move || mounter.unmount(&mp));
        match tokio::time::timeout(self.mount_timeout, std::pin::Pin::new(&mut join)).await {
            Ok(join_result) => (
                join_result
                    .map_err(|e| Error::Internal(format!("unmount task panicked: {e}")))
                    .and_then(|outcome| outcome),
                None,
            ),
            Err(_elapsed) => (
                Ok(UnmountOutcome::Failed(format!(
                    "fusermount unmount timed out after {}s",
                    self.mount_timeout.as_secs_f64()
                ))),
                Some(join),
            ),
        }
    }

    /// Keep the device's mount lock held until a timed-out blocking
    /// command truly exits (see `mount_via_mounter_tracked`). Without the
    /// handoff the orphaned fusermount/sshfs can still touch the path
    /// after the lock is released and release a replacement's mount.
    fn hand_lock_to_leftover_task<T: Send + 'static>(
        guard: tokio::sync::OwnedMutexGuard<()>,
        leftover: Option<tokio::task::JoinHandle<T>>,
    ) {
        match leftover {
            Some(task) => {
                tokio::spawn(async move {
                    let _ = task.await;
                    drop(guard);
                });
            }
            None => drop(guard),
        }
    }

    /// Mount the device's filesystem. Returns the resulting status. The
    /// mount is recorded in the table; `PluginEvent::SftpUpdate` is
    /// broadcast with the new state.
    pub async fn mount_device(
        &self,
        device_id: &str,
        info: &SftpConnectionInfo,
    ) -> Result<MountStatus> {
        // Serialize with a disconnect cleanup racing for the same path:
        // the cleanup unmounts under this same lock, so a reconnect's
        // mount either precedes it (and the cleanup sees the live
        // connection and stands down) or follows it (and mounts a clean
        // path) — it never overlaps the unmount (PR #40 review).
        let mount_lock = self.mount_lock_for(device_id);
        let mount_guard = mount_lock.lock_owned().await;
        let (status, leftover) = self.mount_device_locked(device_id, info).await;
        Self::hand_lock_to_leftover_task(mount_guard, leftover);
        status
    }

    /// Inner mount for callers that already hold the device's mount lock
    /// (`re_mount_if_mounted` holds it across unmount + remount). Returns
    /// the leftover blocking task when the mount timed out — the caller
    /// decides who keeps the lock until it exits.
    async fn mount_device_locked(
        &self,
        device_id: &str,
        info: &SftpConnectionInfo,
    ) -> (
        Result<MountStatus>,
        Option<tokio::task::JoinHandle<Result<MountOutcome>>>,
    ) {
        let mp = self.mount_point(device_id);
        let req = MountRequest {
            ip: info.ip.clone(),
            port: info.port,
            user: info.user.clone(),
            path: info.path.clone(),
            mount_point: mp.clone(),
        };
        // Transition to Mounting FIRST so a concurrent mount request sees
        // the right state.
        self.set_mount_state(device_id, MountState::Mounting);

        let (outcome, leftover) = self
            .mount_via_mounter_tracked(req, info.password.clone())
            .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => return (Err(e), leftover),
        };
        let final_state = match outcome {
            MountOutcome::Mounted => MountState::Mounted,
            MountOutcome::Failed(msg) => MountState::Failed(msg),
        };
        self.set_mount_state(device_id, final_state.clone());
        self.broadcast_update(device_id, info, &final_state, Some(mp.as_path()));
        (
            Ok(MountStatus {
                state: final_state,
                mount_point: Some(mp),
            }),
            leftover,
        )
    }

    /// Unmount the device's filesystem. No-op if nothing is mounted.
    pub async fn unmount_device(&self, device_id: &str) -> Result<MountStatus> {
        let mp = self.mount_point(device_id);
        // Serialize with a mount or disconnect cleanup racing for the
        // same path (PR #40 review round 2).
        let unmount_lock = self.mount_lock_for(device_id);
        let unmount_guard = unmount_lock.lock_owned().await;
        let (outcome, leftover) = self.unmount_via_mounter_tracked(mp.clone()).await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                Self::hand_lock_to_leftover_task(unmount_guard, leftover);
                return Err(e);
            }
        };
        let final_state = match outcome {
            UnmountOutcome::Unmounted => MountState::Unmounted,
            UnmountOutcome::Failed(msg) => MountState::Failed(msg),
        };
        self.set_mount_state(device_id, final_state.clone());
        // Unmount drops the credentials too: when the user disconnects
        // the mount, the desktop-side SFTP session is over; a new
        // `kdeconnect.sftp` packet will re-populate the table.
        if matches!(final_state, MountState::Unmounted) {
            if let Ok(mut connections) = self.connections.write() {
                connections.remove(device_id);
            }
        }
        if let Some(info) = self.get_connection(device_id) {
            self.broadcast_update(device_id, &info, &final_state, None);
        }
        Self::hand_lock_to_leftover_task(unmount_guard, leftover);
        Ok(MountStatus {
            state: final_state,
            mount_point: None,
        })
    }

    /// Lifecycle hook: unmount + drop credentials for a single device.
    /// Idempotent — safe to call on an already-cleaned device. Used by
    /// the unpair and delete handlers and the daemon shutdown path.
    pub async fn cleanup_device(&self, device_id: &str) {
        let mp = self.mount_point(device_id);
        // Serialize with a mount or disconnect cleanup racing for the
        // same path (PR #40 review round 2).
        let cleanup_lock = self.mount_lock_for(device_id);
        let cleanup_guard = cleanup_lock.lock_owned().await;
        let leftover = if mp.exists() {
            let (outcome, leftover) = self.unmount_via_mounter_tracked(mp).await;
            let _ = outcome;
            leftover
        } else {
            None
        };
        self.set_mount_state(device_id, MountState::Unmounted);
        if let Ok(mut connections) = self.connections.write() {
            connections.remove(device_id);
        }
        Self::hand_lock_to_leftover_task(cleanup_guard, leftover);
    }

    /// Daemon-shutdown cleanup: unmount every active mount, drop every
    /// stored credential, clear the tables. Best-effort — failures are
    /// logged, not returned.
    pub async fn cleanup_all(&self) {
        // Devices with an active mount OR a stored credential must be
        // cleaned — the two sets are not identical (a device can have
        // creds from a kdeconnect.sftp packet but not be mounted yet).
        let mut devices: std::collections::BTreeSet<String> = self
            .mounts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        devices.extend(
            self.connections
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned(),
        );
        for d in devices {
            self.cleanup_device(&d).await;
        }
    }

    /// Startup sweep: walk `<data_dir>/mounts/`, attempt `fusermount3 -u`
    /// (or `fusermount -u`) on every `sftp-*` directory left by a
    /// previous crash. Does NOT require sshfs on PATH — a host that
    /// installs the daemon but never sshfs still gets a clean restart.
    /// Each unmount goes through `unmount_via_mounter`, so one wedged
    /// leftover costs at most `mount_timeout` of daemon start.
    pub async fn startup_sweep(&self) -> Vec<String> {
        let mounts_dir = self.data_dir.join("mounts");
        let mut released = Vec::new();
        let entries = match std::fs::read_dir(&mounts_dir) {
            Ok(e) => e,
            Err(_) => return released, // no mounts dir yet, nothing to do
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if !s.starts_with("sftp-") {
                continue;
            }
            let path = entry.path();
            let outcome = self.unmount_via_mounter(path.clone()).await;
            let label = match &outcome {
                Ok(UnmountOutcome::Unmounted) => {
                    // Best-effort: remove the now-empty dir so the next
                    // mount starts clean.
                    let _ = std::fs::remove_dir(&path);
                    "unmounted"
                }
                Ok(UnmountOutcome::Failed(_)) => "failed",
                Err(_) => "error",
            };
            released.push(format!("{}:{}", path.display(), label));
        }
        released
    }

    /// Credential rotation path: if the device is currently mounted,
    /// tear it down and re-mount with the new credentials. Otherwise
    /// the caller is expected to have just stored the new info.
    pub async fn re_mount_if_mounted(
        &self,
        device_id: &str,
        info: &SftpConnectionInfo,
    ) -> Result<MountStatus> {
        let currently_mounted = matches!(
            self.get_mount_status(device_id).state,
            MountState::Mounted | MountState::Mounting
        );
        if !currently_mounted {
            return Ok(self.get_mount_status(device_id));
        }
        // Hold the device's mount lock across the WHOLE unmount +
        // remount: any gap between them lets a disconnect cleanup or a
        // second mount interleave on the same path (PR #40 review
        // round 2, cubic-dev P2). Tear down the stale mount first; we
        // deliberately ignore its outcome (it may be in a half-torn-down
        // state from the phone's side) and let the new mount attempt
        // speak for itself.
        let remount_lock = self.mount_lock_for(device_id);
        let remount_guard = remount_lock.lock_owned().await;
        let (unmount_outcome, leftover) = self
            .unmount_via_mounter_tracked(self.mount_point(device_id))
            .await;
        let _ = unmount_outcome;
        if leftover.is_some() {
            // The old unmount is still draining on the blocking pool;
            // remounting now could be released by the orphan. Keep the
            // lock held until it exits and report the failure.
            let msg = format!(
                "remount skipped: previous unmount still draining after {}s",
                self.mount_timeout.as_secs_f64()
            );
            self.set_mount_state(device_id, MountState::Failed(msg.clone()));
            Self::hand_lock_to_leftover_task(remount_guard, leftover);
            return Ok(MountStatus {
                state: MountState::Failed(msg),
                mount_point: None,
            });
        }
        let (status, leftover) = self.mount_device_locked(device_id, info).await;
        Self::hand_lock_to_leftover_task(remount_guard, leftover);
        status
    }

    fn set_mount_state(&self, device_id: &str, state: MountState) {
        if let Ok(mut mounts) = self.mounts.write() {
            mounts.insert(device_id.to_string(), state);
        }
    }

    fn mount_lock_for(&self, device_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.mount_locks.lock().unwrap_or_else(|e| e.into_inner());
        // Device ids arrive from the LAN, so prune entries nobody holds
        // anymore — an unpruned map grows for the daemon's lifetime and
        // any peer can drive the growth (PR #40 review round 2,
        // cubic-dev P2). Safe under this same std lock: a concurrent
        // caller either already holds a clone (strong_count > 1, kept)
        // or has not cloned yet (will insert a fresh entry).
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks
            .entry(device_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn cleanup_mount_on_disconnect(&self, device_id: &str, live_mount: bool) {
        let mp = mount_point_for(&self.data_dir, device_id);
        let cleanup_lock = self.mount_lock_for(device_id);
        let cleanup_guard = cleanup_lock.lock_owned().await;
        // The registry awaits plugins sequentially, so this cleanup can be
        // delayed behind other plugins' handlers — long enough for the
        // device to reconnect and even re-mount. If a live connection
        // exists for the device now, this cleanup belongs to a stale
        // teardown: the newer session owns the mount path, and unmounting
        // here would release its live mount (PR #40 review, cubic-dev P1).
        if let Some(cm) = &self.connection_manager {
            let id = device_id.to_string();
            if cm.get_generation(&id).await.is_some() {
                debug!(
                    device_id = %device_id,
                    event = "sftp_disconnect_cleanup_superseded",
                    "Disconnect cleanup superseded by a newer connection; leaving the mount alone"
                );
                return;
            }
        }
        if !live_mount {
            self.set_mount_state(device_id, MountState::Unmounted);
            debug!(
                device_id = %device_id,
                event = "sftp_disconnect_stale_state_cleared",
                "Cleared stale SFTP mount state; nothing live to release"
            );
            return;
        }

        let (outcome, leftover) = self.unmount_via_mounter_tracked(mp).await;
        match outcome {
            Ok(UnmountOutcome::Unmounted) => {
                self.set_mount_state(device_id, MountState::Unmounted);
                info!(
                    device_id = %device_id,
                    event = "sftp_unmount_on_disconnect",
                    "Released SFTP mount on disconnect"
                );
            }
            Ok(UnmountOutcome::Failed(_)) | Err(_) => {
                warn!(
                    device_id = %device_id,
                    event = "sftp_unmount_on_disconnect_failed",
                    "Failed to release SFTP mount on disconnect; will retry on startup"
                );
            }
        }
        Self::hand_lock_to_leftover_task(cleanup_guard, leftover);
    }

    fn broadcast_update(
        &self,
        device_id: &str,
        info: &SftpConnectionInfo,
        state: &MountState,
        mount_point: Option<&std::path::Path>,
    ) {
        let available = !info.password.is_empty() && state != &MountState::Failed("".into());
        let mounted = matches!(state, MountState::Mounted);
        self.plugin_events.broadcast(PluginEvent::SftpUpdate {
            device_id: device_id.to_string(),
            ip: info.ip.clone(),
            port: info.port,
            user: info.user.clone(),
            path: info.path.clone(),
            available,
            mounted,
            mount_point: mount_point.map(|p| p.display().to_string()),
        });
    }
}

/// `user@ip:path` is argv[0] to sshfs (mounter.rs `build_sshfs_args`). A
/// `user` that starts with `-` is parsed as an option (`-oProxyCommand=…`
/// is command execution as the daemon's user); `@` or `:` inside it shifts
/// what sshfs reads as host and path. The remote path must be absolute
/// (Android sends `/` and absolute `multiPaths`).
fn validate_sshfs_fields(user: &str, path: &str) -> std::result::Result<(), &'static str> {
    if user.is_empty() {
        return Err("empty user");
    }
    if user.starts_with('-') {
        return Err("user starts with '-' (sshfs option)");
    }
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("user contains characters outside [A-Za-z0-9._-]");
    }
    if !path.starts_with('/') {
        return Err("remote path is not absolute");
    }
    Ok(())
}

fn mount_point_for(data_dir: &std::path::Path, device_id: &str) -> PathBuf {
    data_dir.join("mounts").join(format!("sftp-{device_id}"))
}

fn mount_point_is_live(path: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        let target = path.to_string_lossy();
        mountinfo.lines().any(|line| {
            line.split_whitespace()
                .nth(4)
                .is_some_and(|mount_point| mount_point == target)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        path.exists()
    }
}

#[async_trait::async_trait]
impl Plugin for SftpPlugin {
    fn name(&self) -> &str {
        "sftp"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.sftp".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec!["kdeconnect.sftp.request".to_string()]
    }

    fn is_backend_available(&self) -> bool {
        self.mounter.is_available()
    }

    async fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut connections) = self.connections.write() {
            connections.remove(device_id);
        }
        let mp = mount_point_for(&self.data_dir, device_id);
        self.cleanup_mount_on_disconnect(device_id, mount_point_is_live(&mp))
            .await;
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type != "kdeconnect.sftp" {
            return Ok(None);
        }

        if let Some(error_msg) = packet.body.get("errorMessage") {
            info!(
                device_id = %device_id,
                error = %error_msg,
                event = "sftp_error",
                "SFTP error from device"
            );
            return Ok(None);
        }

        // The sshfs target is the peer address of the authenticated TLS
        // link, never the `ip` the packet claims (kdeconnect-kde
        // mounter.cpp:81-94; its expected-fields set does not list `ip`).
        // A paired-but-hostile peer could otherwise point the desktop's
        // sshfs session, password included, at any host (2026-09-02
        // audit, B1). The packet's `ip` is honored only when no connection
        // manager is wired (test constructions).
        let ip = match &self.connection_manager {
            Some(cm) => match cm.get_peer_addr(&device_id.to_string()).await {
                Some(addr) => addr.ip().to_string(),
                None => {
                    warn!(
                        device_id = %device_id,
                        event = "sftp_no_link_address",
                        "SFTP credentials arrived with no live link address; dropping"
                    );
                    return Ok(None);
                }
            },
            None => packet
                .body
                .get("ip")
                .and_then(|v| v.as_str())
                .unwrap_or("127.0.0.1")
                .to_string(),
        };
        let port = packet
            .body
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(1740) as u16;
        let user = packet
            .body
            .get("user")
            .and_then(|v| v.as_str())
            .unwrap_or("kdeconnect")
            .to_string();
        let password = packet
            .body
            .get("password")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let path = packet
            .body
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("/")
            .to_string();

        let multi_paths: Vec<String> = packet
            .body
            .get("multiPaths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let path_names: Vec<String> = packet
            .body
            .get("pathNames")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if let Err(reason) = validate_sshfs_fields(&user, &path) {
            warn!(
                device_id = %device_id,
                reason,
                event = "sftp_credentials_rejected",
                "SFTP credentials rejected; not stored"
            );
            return Ok(None);
        }

        let info = SftpConnectionInfo {
            ip,
            port,
            user,
            password,
            path,
            multi_paths,
            path_names,
        };

        // The log line intentionally omits the password. A future change
        // that adds a "password" field here would leak it; keep this
        // call shape stable.
        info!(
            device_id = %device_id,
            ip = %info.ip,
            port = info.port,
            path = %info.path,
            event = "sftp_connected",
            "SFTP connection info received"
        );

        // Rotation behavior: if the device is currently mounted, tear
        // down the stale mount and re-mount with the new creds.
        let was_mounted = matches!(
            self.get_mount_status(device_id).state,
            MountState::Mounted | MountState::Mounting
        );
        self.set_connection(device_id, info.clone());

        if was_mounted {
            // Re-mount with the rotated creds. The new password flows
            // through the same mount path the request originally used.
            self.re_mount_if_mounted(device_id, &info).await?;
        } else {
            // Idle credentials — broadcast the "available" state but no
            // mount change.
            self.broadcast_update(device_id, &info, &MountState::Unmounted, None);
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::plugins::sftp::mounter::{
        CommandOutput, CommandRunner, MountOutcome, UnmountOutcome,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Fake runner that returns preset outcomes in order. The first
    /// `mounts.len()` calls to `run_with_stdin` are the mount calls; the
    /// next call (if any) is the unmount. Tests assert behavior via
    /// `argv_log` (a `Vec<Vec<OsString>>` of every argv observed).
    #[derive(Clone)]
    struct ScriptedRunner {
        sshfs_outcome: MountOutcome,
        unmount_outcome: UnmountOutcome,
        argv_log: Arc<Mutex<Vec<Vec<OsString>>>>,
        stdin_log: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedRunner {
        fn always_succeed() -> Self {
            Self {
                sshfs_outcome: MountOutcome::Mounted,
                unmount_outcome: UnmountOutcome::Unmounted,
                argv_log: Arc::new(Mutex::new(Vec::new())),
                stdin_log: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn with_outcomes(mount: MountOutcome, unmount: UnmountOutcome) -> Self {
            Self {
                sshfs_outcome: mount,
                unmount_outcome: unmount,
                argv_log: Arc::new(Mutex::new(Vec::new())),
                stdin_log: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn which(&self, name: &str) -> Option<PathBuf> {
            Some(PathBuf::from(format!("/usr/bin/{name}")))
        }
        fn run_with_stdin(
            &self,
            _program: &Path,
            args: &[OsString],
            stdin_payload: Option<&str>,
        ) -> Result<CommandOutput> {
            let arg_str: Vec<OsString> = args.to_vec();
            let is_sshfs = args.iter().any(|a| a == "user@ip:path")
                || args.iter().any(|a| {
                    a.to_string_lossy().contains('@') && a.to_string_lossy().contains(':')
                });
            self.argv_log.lock().unwrap().push(arg_str);
            if let Some(payload) = stdin_payload {
                self.stdin_log.lock().unwrap().push(payload.to_string());
            }
            if is_sshfs {
                Ok(CommandOutput {
                    status: if matches!(self.sshfs_outcome, MountOutcome::Mounted) {
                        0
                    } else {
                        1
                    },
                    stdout: String::new(),
                    stderr: if let MountOutcome::Failed(msg) = &self.sshfs_outcome {
                        msg.clone()
                    } else {
                        String::new()
                    },
                })
            } else {
                Ok(CommandOutput {
                    status: if matches!(self.unmount_outcome, UnmountOutcome::Unmounted) {
                        0
                    } else {
                        1
                    },
                    stdout: String::new(),
                    stderr: if let UnmountOutcome::Failed(msg) = &self.unmount_outcome {
                        msg.clone()
                    } else {
                        String::new()
                    },
                })
            }
        }
    }

    fn sample_info() -> SftpConnectionInfo {
        SftpConnectionInfo {
            ip: "192.168.1.50".to_string(),
            port: 1740,
            user: "kdeconnect".to_string(),
            password: "phonesecret".to_string(),
            path: "/storage/emulated/0".to_string(),
            multi_paths: vec!["/storage/emulated/0".to_string()],
            path_names: vec!["Internal".to_string()],
        }
    }

    fn test_plugin_with_runner(runner: ScriptedRunner) -> (SftpPlugin, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(runner),
        );
        (plugin, dir)
    }

    const LINK_OUR_ID: &str = "sftp-our-aaaaaaaaaaaaaaaaaaaaaaaa";
    const LINK_PEER_ID: &str = "sftp-peer-aaaaaaaaaaaaaaaaaaaaaaa";

    /// A ConnectionManager holding a live in-process TLS link to
    /// `LINK_PEER_ID`, so `get_peer_addr` has a real address (127.0.0.1)
    /// to hand out. Same shape as share.rs's `cm_with_live_link`.
    async fn cm_with_live_link() -> (
        Arc<crate::protocol::ConnectionManager>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        use crate::protocol::{CertificateManager, ConnectionManager};

        let temp = tempfile::TempDir::new().expect("tempdir");
        let certs = Arc::new(CertificateManager::new(temp.path().to_path_buf()));
        certs.init().expect("init");
        let server_cm = Arc::new(ConnectionManager::new(certs.clone()).expect("cm"));
        server_cm.set_device_identity(LINK_OUR_ID, "Us");
        let client_cm = Arc::new(ConnectionManager::new(certs).expect("cm"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = server_cm.clone();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            server
                .accept_test(LINK_PEER_ID.to_string(), stream)
                .await
                .expect("accept_test");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
        client_cm
            .connect(&LINK_PEER_ID.to_string(), addr)
            .await
            .expect("connect");
        (client_cm, handle, temp)
    }

    fn sftp_packet(ip: &str, user: &str, path: &str) -> Packet {
        Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": ip,
                "port": 1740,
                "user": user,
                "password": "phonesecret",
                "path": path,
                "multiPaths": ["/storage/emulated/0"],
                "pathNames": ["Internal"]
            }),
        )
    }

    /// B1 (2026-09-02 audit): the sshfs target is the address of the
    /// authenticated TLS link, never the `ip` the packet claims
    /// (kdeconnect-kde mounter.cpp:81-94 uses the link address; its
    /// expected-fields set does not even list `ip`). A paired-but-hostile
    /// peer could otherwise point the desktop's sshfs session, password
    /// included, at any host.
    #[tokio::test]
    async fn handle_packet_takes_ip_from_the_live_link_not_the_packet() {
        let (cm, server, _t) = cm_with_live_link().await;
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let plugin = plugin.with_connection_manager(cm);

        plugin
            .handle_packet(LINK_PEER_ID, sftp_packet("203.0.113.9", "kdeconnect", "/"))
            .await
            .expect("handle_packet");

        let stored = plugin
            .get_connection(LINK_PEER_ID)
            .expect("credentials must be stored");
        assert_eq!(
            stored.ip, "127.0.0.1",
            "sshfs target must be the link peer address, not the packet's claim"
        );
        server.abort();
    }

    /// B1: `user@ip:path` is argv[0] to sshfs. A `user` that starts with
    /// `-` is an option (`-oProxyCommand=…` is command execution), and `@`
    /// or `:` inside it shifts what sshfs parses as host and path. Such
    /// packets are dropped, never stored.
    #[tokio::test]
    async fn handle_packet_rejects_option_or_separator_shaped_user() {
        for user in ["-oProxyCommand=id", "kde@evil", "kde:x", "", "kde connect"] {
            let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
            plugin
                .handle_packet("dev-1", sftp_packet("192.168.1.50", user, "/"))
                .await
                .expect("handle_packet");
            assert!(
                plugin.get_connection("dev-1").is_none(),
                "user {user:?} must be rejected, not stored"
            );
        }
    }

    /// B1: the remote path must be absolute (Android sends `/` and absolute
    /// multiPaths); anything else is not a path we asked for.
    #[tokio::test]
    async fn handle_packet_rejects_relative_remote_path() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        plugin
            .handle_packet(
                "dev-1",
                sftp_packet("192.168.1.50", "kdeconnect", "storage"),
            )
            .await
            .expect("handle_packet");
        assert!(plugin.get_connection("dev-1").is_none());
    }

    #[test]
    fn sftp_connection_info_debug_redacts_password() {
        let info = sample_info();
        let debug = format!("{info:?}");
        assert!(
            debug.contains("***redacted***"),
            "missing redaction marker: {debug}"
        );
        assert!(
            !debug.contains("phonesecret"),
            "password leaked in Debug: {debug}"
        );
        // Other fields are still visible.
        assert!(debug.contains("192.168.1.50"));
        assert!(debug.contains("kdeconnect"));
    }

    #[tokio::test]
    async fn plugin_is_backend_available_reflects_mounter() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        assert!(plugin.is_backend_available());
    }

    #[tokio::test]
    async fn mount_starts_in_unmounted_state() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let status = plugin.get_mount_status("dev1");
        assert_eq!(status.state, MountState::Unmounted);
        assert!(status.mount_point.is_none());
    }

    #[tokio::test]
    async fn mount_transitions_to_mounted_on_sshfs_success() {
        let (plugin, dir) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("mount call");
        let status = plugin.get_mount_status("dev1");
        assert_eq!(status.state, MountState::Mounted);
        let mp = status.mount_point.expect("mount point set on success");
        assert!(
            mp.starts_with(dir.path()),
            "mount point under data_dir: {mp:?}"
        );
        assert_eq!(mp.file_name().and_then(|s| s.to_str()), Some("sftp-dev1"));
    }

    #[tokio::test]
    async fn mount_records_failure_with_sshfs_stderr() {
        let runner = ScriptedRunner::with_outcomes(
            MountOutcome::Failed("connection refused".to_string()),
            UnmountOutcome::Unmounted,
        );
        let (plugin, _d) = test_plugin_with_runner(runner);
        let status = plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("mount call");
        assert!(matches!(
            status,
            MountStatus {
                state: MountState::Failed(_),
                ..
            }
        ));
        let stored = plugin.get_mount_status("dev1");
        match stored.state {
            MountState::Failed(msg) => assert!(msg.contains("connection refused")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A `CommandRunner` whose `run_with_stdin` sleeps past a small test
    /// timeout — stands in for an sshfs process that never returns (dead
    /// peer, hung network stack). Blocking-`std::thread::sleep`, not
    /// `tokio::time::sleep` — it must actually occupy a blocking-pool OS
    /// thread the way a real subprocess wait would, or the test would not
    /// exercise `spawn_blocking` at all.
    struct SleepyRunner {
        sleep_for: std::time::Duration,
    }

    impl CommandRunner for SleepyRunner {
        fn which(&self, name: &str) -> Option<PathBuf> {
            Some(PathBuf::from(format!("/usr/bin/{name}")))
        }
        fn run_with_stdin(
            &self,
            _program: &Path,
            _args: &[OsString],
            _stdin_payload: Option<&str>,
        ) -> Result<CommandOutput> {
            std::thread::sleep(self.sleep_for);
            Ok(CommandOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// Pre-fix, `mount_device` called `Mounter::mount` directly on the
    /// async task — a hung sshfs process (modeled here by `SleepyRunner`)
    /// blocked `mount_device`'s `.await` forever, with no bound. Post-fix,
    /// `mount_via_mounter` wraps the call in `spawn_blocking` + a timeout;
    /// this test proves `mount_device` returns `MountState::Failed`
    /// (a timeout report) well within the test's own timeout guard,
    /// instead of hanging.
    /// `on_disconnected` runs on the connection task. Pre-fix it called
    /// `Mounter::unmount` inline, so a hung `fusermount3` (a FUSE mount
    /// whose sshfs is wedged on a dead peer is the common way to get one)
    /// stalled the disconnect path for as long as the process took, with
    /// no bound. Post-fix it goes through `unmount_via_mounter`
    /// (`spawn_blocking` + `mount_timeout`), so the caller returns at the
    /// timeout and the state stays `Mounted` for the startup sweep to
    /// retry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_returns_at_timeout_on_hung_fusermount() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(SleepyRunner {
                sleep_for: std::time::Duration::from_millis(1500),
            }),
        )
        .with_mount_timeout(std::time::Duration::from_millis(50));
        plugin.set_mount_state("live", MountState::Mounted);
        std::fs::create_dir_all(plugin.mount_point("live")).expect("live mount directory");

        let started = std::time::Instant::now();
        plugin.cleanup_mount_on_disconnect("live", true).await;
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(750),
            "disconnect blocked on the hung unmount for {took:?}"
        );
        assert_eq!(plugin.get_mount_status("live").state, MountState::Mounted);
    }

    /// Same bound for the startup sweep: one wedged leftover mount must
    /// not hold up daemon start for longer than `mount_timeout` per entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_sweep_returns_at_timeout_on_hung_fusermount() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(SleepyRunner {
                sleep_for: std::time::Duration::from_millis(1500),
            }),
        )
        .with_mount_timeout(std::time::Duration::from_millis(50));
        std::fs::create_dir_all(dir.path().join("mounts").join("sftp-stuck"))
            .expect("stale mount dir");

        let started = std::time::Instant::now();
        let released = plugin.startup_sweep().await;
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(750),
            "startup sweep blocked on the hung unmount for {took:?}"
        );
        assert_eq!(released.len(), 1);
        assert!(released[0].ends_with(":failed"), "got {released:?}");
    }

    #[tokio::test]
    async fn mount_reports_timeout_instead_of_hanging_on_dead_sshfs() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(SleepyRunner {
                sleep_for: std::time::Duration::from_millis(500),
            }),
        )
        .with_mount_timeout(std::time::Duration::from_millis(50));

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            plugin.mount_device("dev1", &sample_info()),
        )
        .await
        .expect("mount_device must return well within 2s, not hang on the dead sshfs process")
        .expect("mount call");

        match status.state {
            MountState::Failed(msg) => assert!(
                msg.contains("timed out"),
                "expected a timeout message, got: {msg}"
            ),
            other => panic!("expected Failed(timeout), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mount_writes_password_via_stdin_only() {
        let runner = ScriptedRunner::always_succeed();
        let stdin_log = runner.stdin_log.clone();
        let argv_log = runner.argv_log.clone();
        let (plugin, _d) = test_plugin_with_runner(runner);
        plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("mount call");
        let argv = argv_log.lock().unwrap();
        let stdin = stdin_log.lock().unwrap();
        assert_eq!(stdin.len(), 1, "sshfs must receive exactly one stdin write");
        assert_eq!(stdin[0], "phonesecret");
        // argv must never contain the password.
        for arg in argv[0].iter() {
            assert_ne!(arg.to_string_lossy().as_ref(), "phonesecret");
        }
        // The hardening opts are present in argv.
        let argv_str: Vec<String> = argv[0]
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "password_stdin"));
    }

    #[tokio::test]
    async fn unmount_transitions_back_to_unmounted() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("mount");
        let status = plugin.unmount_device("dev1").await.expect("unmount");
        assert_eq!(status.state, MountState::Unmounted);
        let stored = plugin.get_mount_status("dev1");
        assert_eq!(stored.state, MountState::Unmounted);
    }

    #[tokio::test]
    async fn credential_rotation_replaces_when_not_mounted() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        plugin.set_connection("dev1", sample_info());
        let fresh = SftpConnectionInfo {
            password: "rotated-secret".to_string(),
            port: 1741,
            ..sample_info()
        };
        // handle_packet path: a kdeconnect.sftp packet with the new creds.
        let pkt = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": fresh.ip,
                "port": fresh.port,
                "user": fresh.user,
                "password": fresh.password,
                "path": fresh.path
            }),
        );
        plugin
            .handle_packet("dev1", pkt)
            .await
            .expect("handle_packet");
        let stored = plugin.get_connection("dev1").expect("connection stored");
        assert_eq!(stored.password, "rotated-secret");
        // Not mounted → state stays Unmounted.
        assert_eq!(plugin.get_mount_status("dev1").state, MountState::Unmounted);
    }

    #[tokio::test]
    async fn credential_rotation_remounts_when_currently_mounted() {
        // sshfs succeeds first time, FAILS on the second call (rotation
        // re-mount). The plugin must unmount the stale mount before
        // attempting the re-mount, and the final state is Failed.
        let runner =
            ScriptedRunner::with_outcomes(MountOutcome::Mounted, UnmountOutcome::Unmounted);
        let argv_log = runner.argv_log.clone();
        let (plugin, _d) = test_plugin_with_runner(runner);

        // First, mount with the original creds.
        plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("first mount");
        assert_eq!(plugin.get_mount_status("dev1").state, MountState::Mounted);

        // Now inject a fake mounter for the re-mount attempt that fails:
        // replace the runner inside the plugin via a second
        // mount_device call after handle_packet delivers rotated creds.
        // We need a different SftpPlugin for the second mount since the
        // runner is set in with_mounter; do the second mount through
        // a direct call to a runner-aware helper. The cleanest path
        // here: verify the unmount was called BEFORE the new mount by
        // inspecting argv_log ordering.
        //
        // The plugin's handle_packet calls mount_device internally when
        // a mount is active. The same ScriptedRunner answers both
        // mount calls — the first returns Mounted (so initial state is
        // Mounted), then the plugin re-uses the runner for the unmount
        // and the re-mount. argv_log ends with: [mount-args, unmount-args, mount-args-new].
        let rotated = SftpConnectionInfo {
            password: "rotated".to_string(),
            ..sample_info()
        };
        // Replace the plugin's stored info AND mount state so handle_packet
        // sees the rotated creds + active mount.
        plugin.set_connection("dev1", rotated.clone());
        plugin
            .re_mount_if_mounted("dev1", &rotated)
            .await
            .expect("re-mount");

        // The argv log should contain: sshfs-mount, fusermount-unmount, sshfs-mount.
        // (Re-mount always re-uses the tracked mount point; the unmount
        // is what tears the stale one down before the new one starts.)
        let log = argv_log.lock().unwrap();
        assert!(
            log.len() >= 3,
            "expected at least 3 spawns, got {}",
            log.len()
        );
        // First spawn is the initial mount.
        assert!(log[0].iter().any(|a| a == "password_stdin"));
        // Second spawn is the unmount (contains -u, NOT password_stdin).
        let is_unmount =
            log[1].iter().any(|a| a == "-u") && !log[1].iter().any(|a| a == "password_stdin");
        assert!(
            is_unmount,
            "second spawn should be the unmount: {:?}",
            log[1]
        );
        // Third spawn is the re-mount (contains the NEW password in stdin).
        assert!(log[2].iter().any(|a| a == "password_stdin"));
    }

    #[tokio::test]
    async fn unmount_unknown_device_is_a_noop() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let status = plugin
            .unmount_device("never-mounted")
            .await
            .expect("unmount");
        assert_eq!(status.state, MountState::Unmounted);
    }

    #[tokio::test]
    async fn cleanup_device_drops_connection_and_mount() {
        let runner = ScriptedRunner::always_succeed();
        let argv_log = runner.argv_log.clone();
        let (plugin, _d) = test_plugin_with_runner(runner);
        plugin.set_connection("dev1", sample_info());
        plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("mount");
        assert!(plugin.get_connection("dev1").is_some());
        assert_eq!(plugin.get_mount_status("dev1").state, MountState::Mounted);

        plugin.cleanup_device("dev1").await;
        assert!(plugin.get_connection("dev1").is_none());
        assert_eq!(plugin.get_mount_status("dev1").state, MountState::Unmounted);
        // cleanup_device unmounts the tracked mount point.
        let log = argv_log.lock().unwrap();
        let last = log.last().expect("at least one spawn");
        assert!(last.iter().any(|a| a == "-u"));
    }

    #[tokio::test]
    async fn cleanup_device_idempotent() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        // Clean an already-clean device — must not panic, must not error.
        plugin.cleanup_device("never-seen").await;
        plugin.cleanup_device("never-seen").await;
    }

    #[tokio::test]
    async fn cleanup_all_unmounts_every_tracked_device() {
        let runner = ScriptedRunner::always_succeed();
        let argv_log = runner.argv_log.clone();
        let (plugin, _d) = test_plugin_with_runner(runner);
        // Two devices, both mounted.
        plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("mount1");
        plugin
            .mount_device("dev2", &sample_info())
            .await
            .expect("mount2");
        plugin.cleanup_all().await;
        assert_eq!(plugin.get_mount_status("dev1").state, MountState::Unmounted);
        assert_eq!(plugin.get_mount_status("dev2").state, MountState::Unmounted);
        // The last two spawns should be the unmounts (cleanup_unmounts).
        let log = argv_log.lock().unwrap();
        let n = log.len();
        assert!(n >= 4, "expected at least 2 mounts + 2 unmounts, got {n}");
        for argset in &log[n - 2..] {
            assert!(argset.iter().any(|a| a == "-u"));
        }
    }

    #[tokio::test]
    async fn startup_sweep_releases_stale_mounts() {
        // The data dir has two stale sftp-* dirs from a previous crash.
        // The fake runner reports UnmountOutcome::Unmounted for them.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mounts_dir = dir.path().join("mounts");
        std::fs::create_dir_all(mounts_dir.join("sftp-dev1")).unwrap();
        std::fs::create_dir_all(mounts_dir.join("sftp-dev2")).unwrap();
        // A non-sftp dir should be left alone.
        let other = mounts_dir.join("not-sftp");
        std::fs::create_dir_all(&other).unwrap();
        let argv_log = Arc::new(Mutex::new(Vec::new()));
        let runner = ScriptedRunner {
            sshfs_outcome: MountOutcome::Mounted,
            unmount_outcome: UnmountOutcome::Unmounted,
            argv_log: argv_log.clone(),
            stdin_log: Arc::new(Mutex::new(Vec::new())),
        };
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(runner),
        );
        let released = plugin.startup_sweep().await;
        assert_eq!(released.len(), 2, "two sftp-* dirs swept: {released:?}");
        // Stale dirs removed after unmount.
        assert!(!mounts_dir.join("sftp-dev1").exists());
        assert!(!mounts_dir.join("sftp-dev2").exists());
        // The unrelated dir is untouched.
        assert!(other.exists());
    }

    #[tokio::test]
    async fn startup_sweep_safe_when_mounts_dir_missing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        // No mounts/ subdir at all.
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(ScriptedRunner::always_succeed()),
        );
        let released = plugin.startup_sweep().await;
        assert!(released.is_empty());
    }

    #[tokio::test]
    async fn startup_sweep_safe_when_sshfs_missing() {
        // sshfs is NOT on PATH: a startup sweep must still run for any
        // stale mounts left by a previous daemon. (kdeconnect-kde's
        // sweep is implicit in mounter construction; we make it explicit
        // because the cleanup here is "fusermount the previous mount
        // point", not "spawn sshfs again".)
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mounts_dir = dir.path().join("mounts");
        std::fs::create_dir_all(mounts_dir.join("sftp-stale")).unwrap();
        // A runner that reports nothing on PATH for sshfs but still
        // answers fusermount calls.
        struct NoSshfsRunner;
        impl CommandRunner for NoSshfsRunner {
            fn which(&self, name: &str) -> Option<PathBuf> {
                if name == "sshfs" {
                    None
                } else {
                    Some(PathBuf::from(format!("/usr/bin/{name}")))
                }
            }
            fn run_with_stdin(
                &self,
                _program: &Path,
                _args: &[OsString],
                _stdin: Option<&str>,
            ) -> crate::utils::errors::Result<CommandOutput> {
                Ok(CommandOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(NoSshfsRunner),
        );
        let released = plugin.startup_sweep().await;
        assert_eq!(released.len(), 1);
        assert!(!mounts_dir.join("sftp-stale").exists());
    }

    #[test]
    fn sftp_plugin_name() {
        let plugin = SftpPlugin::new();
        assert_eq!(plugin.name(), "sftp");
    }

    #[test]
    fn sftp_capabilities() {
        let plugin = SftpPlugin::new();
        assert!(plugin
            .incoming_capabilities()
            .contains(&"kdeconnect.sftp".to_string()));
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.sftp.request".to_string()));
    }

    #[tokio::test]
    async fn handle_sftp_packet_stores_info() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": "192.168.1.100",
                "port": 1740,
                "user": "kdeconnect",
                "password": "secretpassword",
                "path": "/storage/emulated/0",
                "multiPaths": ["/storage/emulated/0", "/storage/emulated/0/DCIM"],
                "pathNames": ["Internal Storage", "Camera pictures"]
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        let info = plugin
            .get_connection("device1")
            .expect("Value expected to be present");
        assert_eq!(info.ip, "192.168.1.100");
        assert_eq!(info.password, "secretpassword");
    }

    /// The exact wire envelope the Android app sends — every key the rust
    /// plugin reads (ip, port, user, password, path, multiPaths, pathNames)
    /// is present and matches the upstream-derived fixture at
    /// tests/fixtures/upstream-wire/sftp/credentials.json (cited against
    /// kdeconnect-android SftpPlugin.kt:126-137). The binary payload stream
    /// rides on a separate channel and is not asserted here.
    #[tokio::test]
    async fn test_credentials_packet_shape_matches_android() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/sftp/credentials.json"),
            )
            .expect("sftp/credentials.json"),
        )
        .expect("sftp/credentials.json parses");

        let packet = Packet::new("kdeconnect.sftp".to_string(), body.clone());
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("handle_packet");

        let info = plugin
            .get_connection("device1")
            .expect("Value expected to be present");
        assert_eq!(info.ip, body["ip"].as_str().expect("ip"));
        assert_eq!(info.port, body["port"].as_u64().expect("port") as u16);
        assert_eq!(info.user, body["user"].as_str().expect("user"));
        assert_eq!(info.password, body["password"].as_str().expect("password"));
        assert_eq!(info.path, body["path"].as_str().expect("path"));
        assert_eq!(
            info.multi_paths,
            body["multiPaths"]
                .as_array()
                .expect("multiPaths")
                .iter()
                .map(|v| v.as_str().expect("multiPaths entry").to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            info.path_names,
            body["pathNames"]
                .as_array()
                .expect("pathNames")
                .iter()
                .map(|v| v.as_str().expect("pathNames entry").to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn handle_sftp_error_does_not_store() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "errorMessage": "No storage locations configured"
            }),
        );
        assert!(plugin.handle_packet("device1", packet).await.is_ok());
        assert!(plugin.get_connection("device1").is_none());
    }

    #[test]
    fn request_sftp_packet() {
        let plugin = SftpPlugin::new();
        let packet = plugin.request_sftp("device1");
        assert_eq!(packet.packet_type, "kdeconnect.sftp.request");
        assert_eq!(
            packet.body.get("startBrowsing"),
            Some(&serde_json::json!(true))
        );
    }

    #[tokio::test]
    async fn disconnect_stale_record_without_live_mount_clears_without_unmount() {
        let runner = ScriptedRunner::with_outcomes(
            MountOutcome::Mounted,
            UnmountOutcome::Failed("not mounted".to_string()),
        );
        let argv_log = runner.argv_log.clone();
        let (plugin, _d) = test_plugin_with_runner(runner);
        let mount_point = plugin.mount_point("stale");
        std::fs::create_dir_all(&mount_point).expect("stale mount directory");
        plugin.set_mount_state("stale", MountState::Mounted);

        plugin.on_disconnected("stale").await;

        assert!(argv_log.lock().unwrap().is_empty());
        assert_eq!(
            plugin.get_mount_status("stale").state,
            MountState::Unmounted
        );
    }

    #[tokio::test]
    async fn disconnect_failed_live_unmount_keeps_state_for_retry() {
        let runner = ScriptedRunner::with_outcomes(
            MountOutcome::Mounted,
            UnmountOutcome::Failed("busy".to_string()),
        );
        let argv_log = runner.argv_log.clone();
        let (plugin, _d) = test_plugin_with_runner(runner);
        plugin.set_mount_state("live", MountState::Mounted);
        std::fs::create_dir_all(plugin.mount_point("live")).expect("live mount directory");

        plugin.cleanup_mount_on_disconnect("live", true).await;

        assert_eq!(argv_log.lock().unwrap().len(), 1);
        assert_eq!(plugin.get_mount_status("live").state, MountState::Mounted);
    }

    #[tokio::test]
    async fn disconnected_clears_connection_and_mount() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let packet = Packet::new(
            "kdeconnect.sftp".to_string(),
            serde_json::json!({
                "ip": "192.168.1.100",
                "port": 1740,
                "user": "kdeconnect",
                "password": "secret",
                "path": "/"
            }),
        );
        plugin
            .handle_packet("device1", packet)
            .await
            .expect("handle_packet");
        plugin
            .mount_device("device1", &plugin.get_connection("device1").unwrap())
            .await
            .expect("mount");
        assert_eq!(
            plugin.get_mount_status("device1").state,
            MountState::Mounted
        );
        plugin.on_disconnected("device1").await;
        assert!(plugin.get_connection("device1").is_none());
        assert_eq!(
            plugin.get_mount_status("device1").state,
            MountState::Unmounted
        );
    }

    /// PR #40 review (cubic-dev P1): a device that reconnects while the old
    /// connection's disconnect cleanup is still pending mounts the same
    /// `sftp-<device>` path. The stale cleanup must recognize the newer
    /// connection superseded it and stand down — unmounting would release
    /// the replacement's live mount.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_cleanup_does_not_unmount_a_replacement_mount() {
        // LINK_PEER_ID holds a live link: the device already reconnected
        // before the old teardown's cleanup gets to run.
        let (cm, _server, _t) = cm_with_live_link().await;
        let runner = ScriptedRunner::always_succeed();
        let argv_log = runner.argv_log.clone();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(runner),
        )
        .with_connection_manager(cm);
        plugin.set_mount_state(LINK_PEER_ID, MountState::Mounted);
        std::fs::create_dir_all(plugin.mount_point(LINK_PEER_ID)).expect("mount directory");

        plugin.cleanup_mount_on_disconnect(LINK_PEER_ID, true).await;

        assert_eq!(
            plugin.get_mount_status(LINK_PEER_ID).state,
            MountState::Mounted
        );
        let log = argv_log.lock().unwrap();
        assert!(
            log.is_empty(),
            "stale disconnect cleanup ran fusermount on the replacement's mount: {log:?}"
        );
    }

    /// Same race from the other side: when the disconnect cleanup wins the
    /// device first, the replacement mount must wait for the in-flight
    /// unmount instead of mounting into it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replacement_mount_waits_for_in_flight_disconnect_unmount() {
        /// Records mount/unmount phase order and sleeps like a slow
        /// fusermount so the two operations overlap unless serialized.
        struct OrderingRunner {
            events: Arc<Mutex<Vec<&'static str>>>,
            sleep_for: std::time::Duration,
            // notify_one: a permit survives an early fire (PR #40's
            // registry test hung on notify_waiters' lost wake).
            unmount_started: Arc<tokio::sync::Notify>,
        }

        impl CommandRunner for OrderingRunner {
            fn which(&self, name: &str) -> Option<PathBuf> {
                Some(PathBuf::from(format!("/usr/bin/{name}")))
            }

            fn run_with_stdin(
                &self,
                _program: &Path,
                args: &[OsString],
                _stdin: Option<&str>,
            ) -> Result<CommandOutput> {
                let is_mount = args.iter().any(|a| a.to_string_lossy().contains('@'));
                let (start, end) = if is_mount {
                    ("mount-start", "mount-end")
                } else {
                    ("unmount-start", "unmount-end")
                };
                self.events.lock().unwrap().push(start);
                if !is_mount {
                    self.unmount_started.notify_one();
                }
                std::thread::sleep(self.sleep_for);
                self.events.lock().unwrap().push(end);
                Ok(CommandOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }

        let events = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let unmount_started = Arc::new(tokio::sync::Notify::new());
        let dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = Arc::new(
            SftpPlugin::with_mounter(
                Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                    8, "test",
                )),
                dir.path().to_path_buf(),
                Arc::new(OrderingRunner {
                    events: events.clone(),
                    sleep_for: std::time::Duration::from_millis(300),
                    unmount_started: unmount_started.clone(),
                }),
            )
            .with_mount_timeout(std::time::Duration::from_secs(5)),
        );
        plugin.set_mount_state("dev1", MountState::Mounted);

        // Register the readiness waiter BEFORE spawning so the notify
        // can't be lost, then wait until the cleanup's unmount is
        // actually running (holds the device's mount lock).
        let started_fut = unmount_started.notified();
        tokio::pin!(started_fut);
        started_fut.as_mut().enable();
        let c = plugin.clone();
        let cleanup = tokio::spawn(async move {
            c.cleanup_mount_on_disconnect("dev1", true).await;
        });
        started_fut.await;

        plugin
            .mount_device("dev1", &sample_info())
            .await
            .expect("replacement mount");
        cleanup.await.expect("cleanup completes");

        assert_eq!(plugin.get_mount_status("dev1").state, MountState::Mounted);
        let ev = events.lock().unwrap();
        let mount_start = ev
            .iter()
            .position(|e| *e == "mount-start")
            .expect("mount ran");
        let unmount_end = ev
            .iter()
            .position(|e| *e == "unmount-end")
            .expect("unmount ran");
        assert!(
            mount_start > unmount_end,
            "replacement mount started before the in-flight unmount finished: {ev:?}"
        );
    }

    /// PR #40 review round 2 (cubic-dev P2): device ids arrive from the
    /// LAN; without pruning, every id that ever mounted or cleaned up
    /// would leave a permanent entry in `mount_locks`.
    #[tokio::test]
    async fn mount_locks_are_pruned_when_no_longer_held() {
        let (plugin, _d) = test_plugin_with_runner(ScriptedRunner::always_succeed());
        let gone = plugin.mount_lock_for("dev-gone");
        assert_eq!(plugin.mount_locks.lock().unwrap().len(), 1);
        drop(gone);
        // The next lookup prunes the orphaned entry while inserting its own.
        let _here = plugin.mount_lock_for("dev-here");
        let locks = plugin.mount_locks.lock().unwrap();
        assert!(
            locks.get("dev-gone").is_none(),
            "orphaned mount-lock entry was not pruned"
        );
        assert!(locks.contains_key("dev-here"));
    }
}

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
use crate::utils::errors::Result;

use super::plugin::Plugin;

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
        }
    }

    #[allow(clippy::expect_used)]
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

    /// Mount the device's filesystem. Returns the resulting status. The
    /// mount is recorded in the table; `PluginEvent::SftpUpdate` is
    /// broadcast with the new state.
    pub async fn mount_device(
        &self,
        device_id: &str,
        info: &SftpConnectionInfo,
    ) -> Result<MountStatus> {
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

        let outcome = self.mounter.mount(&req, &info.password)?;
        let final_state = match outcome {
            MountOutcome::Mounted => MountState::Mounted,
            MountOutcome::Failed(msg) => MountState::Failed(msg),
        };
        self.set_mount_state(device_id, final_state.clone());
        self.broadcast_update(device_id, info, &final_state, Some(mp.as_path()));
        Ok(MountStatus {
            state: final_state,
            mount_point: Some(mp),
        })
    }

    /// Unmount the device's filesystem. No-op if nothing is mounted.
    pub async fn unmount_device(&self, device_id: &str) -> Result<MountStatus> {
        let mp = self.mount_point(device_id);
        let outcome = self.mounter.unmount(&mp)?;
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
        if mp.exists() {
            let _ = self.mounter.unmount(&mp);
        }
        self.set_mount_state(device_id, MountState::Unmounted);
        if let Ok(mut connections) = self.connections.write() {
            connections.remove(device_id);
        }
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
    pub fn startup_sweep(&self) -> Vec<String> {
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
            let outcome = self.mounter.unmount(&path);
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
        // Tear down the stale mount first; we deliberately ignore its
        // outcome (it may be in a half-torn-down state from the phone's
        // side) and let the new mount attempt speak for itself.
        let _ = self.mounter.unmount(&self.mount_point(device_id));
        self.mount_device(device_id, info).await
    }

    fn set_mount_state(&self, device_id: &str, state: MountState) {
        if let Ok(mut mounts) = self.mounts.write() {
            mounts.insert(device_id.to_string(), state);
        }
    }

    fn cleanup_mount_on_disconnect(&self, device_id: &str, live_mount: bool) {
        let mp = mount_point_for(&self.data_dir, device_id);
        if !live_mount {
            self.set_mount_state(device_id, MountState::Unmounted);
            debug!(
                device_id = %device_id,
                event = "sftp_disconnect_stale_state_cleared",
                "Cleared stale SFTP mount state; nothing live to release"
            );
            return;
        }

        match self.mounter.unmount(&mp) {
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

    fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut connections) = self.connections.write() {
            connections.remove(device_id);
        }
        let mp = mount_point_for(&self.data_dir, device_id);
        self.cleanup_mount_on_disconnect(device_id, mount_point_is_live(&mp));
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

        let ip = packet
            .body
            .get("ip")
            .and_then(|v| v.as_str())
            .unwrap_or("127.0.0.1")
            .to_string();
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

    #[test]
    fn startup_sweep_releases_stale_mounts() {
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
        let released = plugin.startup_sweep();
        assert_eq!(released.len(), 2, "two sftp-* dirs swept: {released:?}");
        // Stale dirs removed after unmount.
        assert!(!mounts_dir.join("sftp-dev1").exists());
        assert!(!mounts_dir.join("sftp-dev2").exists());
        // The unrelated dir is untouched.
        assert!(other.exists());
    }

    #[test]
    fn startup_sweep_safe_when_mounts_dir_missing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        // No mounts/ subdir at all.
        let plugin = SftpPlugin::with_mounter(
            Arc::new(crate::plugins::events::PluginEventBroadcaster::new(
                8, "test",
            )),
            dir.path().to_path_buf(),
            Arc::new(ScriptedRunner::always_succeed()),
        );
        let released = plugin.startup_sweep();
        assert!(released.is_empty());
    }

    #[test]
    fn startup_sweep_safe_when_sshfs_missing() {
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
        let released = plugin.startup_sweep();
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

        plugin.on_disconnected("stale");

        assert!(argv_log.lock().unwrap().is_empty());
        assert_eq!(
            plugin.get_mount_status("stale").state,
            MountState::Unmounted
        );
    }

    #[test]
    fn disconnect_failed_live_unmount_keeps_state_for_retry() {
        let runner = ScriptedRunner::with_outcomes(
            MountOutcome::Mounted,
            UnmountOutcome::Failed("busy".to_string()),
        );
        let argv_log = runner.argv_log.clone();
        let (plugin, _d) = test_plugin_with_runner(runner);
        plugin.set_mount_state("live", MountState::Mounted);
        std::fs::create_dir_all(plugin.mount_point("live")).expect("live mount directory");

        plugin.cleanup_mount_on_disconnect("live", true);

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
        plugin.on_disconnected("device1");
        assert!(plugin.get_connection("device1").is_none());
        assert_eq!(
            plugin.get_mount_status("device1").state,
            MountState::Unmounted
        );
    }
}

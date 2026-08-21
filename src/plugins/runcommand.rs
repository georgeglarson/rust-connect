//! Runcommand plugin
//!
//! Single Responsibility: Advertise this machine's command list to paired
//! devices and execute allowlisted commands they request.
//!
//! Wire shapes (upstream-verified):
//! - Outgoing `kdeconnect.runcommand` — the command-list advertisement. Body:
//!   `commandList`, a JSON-ENCODED STRING of `{key: {"name": ..., "command": ...}}`
//!   (kdeconnect-kde plugins/runcommand/runcommandplugin.cpp:161-168 sends the
//!   config string verbatim; kdeconnect-android
//!   src/main/java/org/kde/kdeconnect/plugins/runcommand/RunCommandPlugin.java:155
//!   parses it with `new JSONObject(np.getString("commandList"))`), plus
//!   `canAddCommand` boolean (runcommandplugin.cpp:165; RunCommandPlugin.java:197).
//!   kdeconnect-kde sends it on connect (runcommandplugin.cpp:156-159).
//! - Incoming `kdeconnect.runcommand.request` — body is EITHER
//!   `{"requestCommandList": true}` (RunCommandPlugin.java:250-254; answered
//!   with the advertisement, runcommandplugin.cpp:52-55) OR `{"key": "<key>"}`
//!   (RunCommandPlugin.java:242-248; executed if configured,
//!   runcommandplugin.cpp:57-58, 70-103). Upstream also defines `setup`/`stop`
//!   (RunCommandPlugin.java:260-270) which we parse-and-ignore: we have no
//!   config UI to open and no long-running process table to stop.
//! - Execution runs the configured command via `/bin/sh -c <command>` exactly
//!   like upstream (runcommandplugin.cpp:34-37, 102).
//!
//! NOT implemented (and therefore NOT advertised): `kdeconnect.runcommand.output`
//! streaming (runcommandplugin.cpp:149-154). Our plugin API returns reply
//! packets synchronously while execution is async, so we cannot stream output
//! honestly; per project law we don't advertise what we can't honor.
//!
//! SECURITY / production posture (per project decision, 2026-08-20): the
//! allowlist is populated from the desktop config file (`AppSettings` ->
//! `RuncommandConfig`, deserialized from `[[runcommand.commands]]` entries
//! in `~/.config/rust-connect/config.toml`) and registered into the plugin
//! at boot via `register_from_config`. There is intentionally NO runtime
//! write path (no REST, no DBus, no signal handler) — the allowlist can
//! only change by editing the config and restarting the daemon. This is
//! the kdeconnect-kde model: commands are defined on the desktop, the
//! paired phone only triggers them. Without a `[runcommand]` section the
//! allowlist stays empty and every request is blocked, preserving the
//! prior safe-by-default posture.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;

use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::settings::RuncommandConfig;
use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

const MAX_OUTPUT_SIZE: usize = 64 * 1024;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on the in-memory execution history: only the most recent
/// `MAX_EXECUTED_RECORDS` outcomes are kept (oldest dropped first).
const MAX_EXECUTED_RECORDS: usize = 100;

/// One allowlisted command. `key` is what the phone sends back in
/// `{"key": ...}`; `name`/`command` are the advertised fields
/// (kdeconnect-kde runcommandplugin.cpp:78-87 reads them from config).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommandEntry {
    pub name: String,
    pub command: String,
}

#[derive(Clone)]
pub struct RuncommandPlugin {
    /// device_id -> key -> entry. The per-device code API
    /// (`allow_command`) targets this map; in production it stays empty
    /// because nothing in production wiring calls it.
    allowed_commands: Arc<StdRwLock<HashMap<String, HashMap<String, CommandEntry>>>>,
    /// key -> entry. Desktop-global allowlist populated from
    /// `AppSettings.runcommand.commands` at boot via `register_from_config`.
    /// Both `lookup` and `command_list_json` consult it alongside the
    /// per-device map.
    global_commands: Arc<StdRwLock<HashMap<String, CommandEntry>>>,
    executed: Arc<StdRwLock<Vec<ExecutedCommand>>>,
    execution_timeout: Duration,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecutedCommand {
    pub device_id: String,
    pub key: String,
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Default for RuncommandPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl RuncommandPlugin {
    pub fn new() -> Self {
        Self {
            allowed_commands: Arc::new(StdRwLock::new(HashMap::new())),
            global_commands: Arc::new(StdRwLock::new(HashMap::new())),
            executed: Arc::new(StdRwLock::new(Vec::new())),
            execution_timeout: EXECUTION_TIMEOUT,
        }
    }

    pub fn with_execution_timeout(mut self, timeout: Duration) -> Self {
        self.execution_timeout = timeout;
        self
    }

    /// Code-level API to allowlist a command for a device. NOT called from
    /// production wiring — the per-device map stays empty outside tests.
    /// The production allowlist lives in `global_commands` and is set
    /// from the config file via `register_from_config`.
    pub fn allow_command(&self, device_id: &str, key: &str, name: &str, command: &str) {
        if let Ok(mut map) = self.allowed_commands.write() {
            map.entry(device_id.to_string()).or_default().insert(
                key.to_string(),
                CommandEntry {
                    name: name.to_string(),
                    command: command.to_string(),
                },
            );
        }
    }

    /// Populate the desktop-global allowlist from `RuncommandConfig`
    /// (deserialized from `[[runcommand.commands]]` entries in the config
    /// file). Called once at boot, after `RuncommandPlugin::new()`. No
    /// runtime write path exists — re-registering is the only way to
    /// change the allowlist without restarting the daemon, and it
    /// REPLACES the global set atomically: keys absent from the new
    /// config stop being executable.
    ///
    /// Validation: entries with empty `key`, `name`, or `command` are
    /// skipped and `warn!`-ed (daemon boot must not fail on a bad row);
    /// the first entry for any given key wins, later duplicates of the
    /// same key are skipped and `warn!`-ed so the advertised JSON object
    /// never carries duplicate-key entries that phones parse.
    /// Per-device entries set via `allow_command` are not touched and
    /// take precedence over global entries with the same key for
    /// lookup.
    pub fn register_from_config(&self, cfg: &RuncommandConfig) {
        if let Ok(mut global) = self.global_commands.write() {
            global.clear();
            for entry in &cfg.commands {
                if entry.key.is_empty() || entry.name.is_empty() || entry.command.is_empty() {
                    warn!(
                        key = %entry.key,
                        name = %entry.name,
                        event = "runcommand_config_invalid_entry",
                        "Skipping runcommand config entry with empty key/name/command"
                    );
                    continue;
                }
                if global.contains_key(&entry.key) {
                    warn!(
                        key = %entry.key,
                        event = "runcommand_config_duplicate_key",
                        "Skipping duplicate runcommand config entry; first wins"
                    );
                    continue;
                }
                global.insert(
                    entry.key.clone(),
                    CommandEntry {
                        name: entry.name.clone(),
                        command: entry.command.clone(),
                    },
                );
            }
        }
    }

    pub fn executed_commands(&self) -> Vec<ExecutedCommand> {
        self.executed
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    #[cfg(test)]
    pub async fn wait_for_commands(&self, expected_count: usize, timeout_ms: u64) {
        let start = std::time::Instant::now();
        while self.executed_commands().len() < expected_count {
            if start.elapsed() > Duration::from_millis(timeout_ms) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn lookup(&self, device_id: &str, key: &str) -> Option<CommandEntry> {
        // Per-device entries (set via allow_command, used by tests) take
        // precedence over desktop-global entries (set via register_from_config).
        let per_device = self
            .allowed_commands
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = per_device
            .get(device_id)
            .and_then(|cmds| cmds.get(key))
            .cloned()
        {
            return Some(entry);
        }
        let global = self
            .global_commands
            .read()
            .unwrap_or_else(|e| e.into_inner());
        global.get(key).cloned()
    }

    /// The `commandList` field value: a JSON-ENCODED STRING of
    /// `{key: {"name": ..., "command": ...}}` (kdeconnect-kde
    /// runcommandplugin.cpp:163-164 sends the config string; Android parses
    /// it as a string, RunCommandPlugin.java:155).
    ///
    /// The advertised list is the union of per-device entries for this
    /// device and the desktop-global entries — honest capability: every
    /// advertised key is one this plugin will execute when a paired phone
    /// requests it.
    fn command_list_json(&self, device_id: &str) -> String {
        let mut entries: HashMap<String, CommandEntry> = HashMap::new();
        let per_device = self
            .allowed_commands
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(device_entries) = per_device.get(device_id) {
            for (k, v) in device_entries {
                entries.insert(k.clone(), v.clone());
            }
        }
        let global = self
            .global_commands
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for (k, v) in global.iter() {
            entries.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if entries.is_empty() {
            "{}".to_string()
        } else {
            serde_json::to_string(&entries).unwrap_or_else(|_| "{}".to_string())
        }
    }

    /// Build the command-list advertisement packet for a device.
    /// `canAddCommand` is false: unlike kdeconnect-kde (which has a config
    /// dialog, runcommandplugin.cpp:165), we expose no way for the phone to
    /// add commands.
    pub fn command_list_packet(&self, device_id: &str) -> Packet {
        Packet::new(
            "kdeconnect.runcommand".to_string(),
            serde_json::json!({
                "commandList": self.command_list_json(device_id),
                "canAddCommand": false,
            }),
        )
    }

    /// Execute an allowlisted command via `/bin/sh -c`, exactly like upstream
    /// (kdeconnect-kde runcommandplugin.cpp:34-37, 102), with a timeout and a
    /// 64KB cap on captured output.
    ///
    /// Two hardening properties beyond the naive `.output()` version:
    ///
    /// - **Process-group kill on timeout.** The shell is spawned in its own
    ///   process group (`process_group(0)`, pgid == child pid) and a timeout
    ///   SIGKILLs the whole group. `kill_on_drop` alone only kills the direct
    ///   child, so a command that backgrounds work (`( … ) &`, pipelines,
    ///   subshells) would orphan survivors that keep running after the
    ///   "timed out" verdict (process-group kill finding). `kill_on_drop`
    ///   stays on as a backstop for the direct child.
    /// - **Streamed, capped output capture.** Both pipes are drained to EOF
    ///   (so the child never blocks on a full pipe) but only the first
    ///   MAX_OUTPUT_SIZE bytes are kept. `.output()` buffers everything the
    ///   command writes for its entire lifetime — `yes` would grow memory
    ///   unbounded until the timeout (output cap finding).
    async fn execute_command(&self, device_id: &str, key: &str, command: &str) -> ExecutedCommand {
        let device_id = device_id.to_string();
        let key = key.to_string();
        let command = command.to_string();

        let spawn = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();

        let mut child = match spawn {
            Ok(child) => child,
            Err(e) => {
                warn!(
                    device_id = %device_id,
                    key = %key,
                    error = %e,
                    event = "runcommand_failed",
                    "Command spawn failed"
                );
                return ExecutedCommand {
                    device_id,
                    key,
                    command,
                    success: false,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: e.to_string(),
                };
            }
        };

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let collect = async {
            let stdout_fut = read_capped(stdout_pipe);
            let stderr_fut = read_capped(stderr_pipe);
            let (stdout, stderr, status) = tokio::join!(stdout_fut, stderr_fut, child.wait());
            (stdout, stderr, status)
        };

        match timeout(self.execution_timeout, collect).await {
            Ok((stdout, stderr, Ok(status))) => {
                let success = status.success();
                let exit_code = status.code();

                info!(
                    device_id = %device_id,
                    key = %key,
                    exit_code = ?exit_code,
                    success = success,
                    event = "runcommand_completed",
                    "Command execution completed"
                );

                ExecutedCommand {
                    device_id,
                    key,
                    command,
                    success,
                    exit_code,
                    stdout: String::from_utf8_lossy(&stdout).to_string(),
                    stderr: String::from_utf8_lossy(&stderr).to_string(),
                }
            }
            Ok((_, _, Err(e))) => {
                warn!(
                    device_id = %device_id,
                    key = %key,
                    error = %e,
                    event = "runcommand_failed",
                    "Command execution failed"
                );

                ExecutedCommand {
                    device_id,
                    key,
                    command,
                    success: false,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: e.to_string(),
                }
            }
            Err(_) => {
                warn!(
                    device_id = %device_id,
                    key = %key,
                    timeout_secs = self.execution_timeout.as_secs(),
                    event = "runcommand_timeout",
                    "Command execution timed out"
                );

                // Kill the whole process group, not just the shell. The
                // child is the group leader (process_group(0) above), so
                // pgid == pid. Processes that called setsid() themselves
                // escape this — same as upstream, which does no group kill
                // at all.
                if let Some(pid) = child.id() {
                    // SAFETY: killpg on a process group we own (spawned by
                    // us as group leader); no memory safety involved. The
                    // return value is intentionally ignored — the group may
                    // already be gone if the command exited right at the
                    // deadline, and the kill_on_drop backstop covers the
                    // direct child regardless.
                    unsafe {
                        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
                // Backstop for the direct child + reap so no zombie is left.
                let _ = child.kill().await;
                let _ = child.wait().await;

                ExecutedCommand {
                    device_id,
                    key,
                    command,
                    success: false,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!(
                        "Command timed out after {} seconds",
                        self.execution_timeout.as_secs()
                    ),
                }
            }
        }
    }
}

/// Drain a child-output pipe to EOF, keeping only the first
/// MAX_OUTPUT_SIZE bytes. Draining (rather than `take(MAX)` and stop) keeps
/// the child from blocking on a full pipe once the cap is hit, so a chatty
/// command still finishes inside its timeout instead of wedging.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::with_capacity(MAX_OUTPUT_SIZE);
    let Some(mut reader) = pipe else {
        return buf;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Err(e) => {
                debug!(
                    error = %e,
                    event = "runcommand_output_read_error",
                    "Child output pipe read failed; keeping what was captured"
                );
                break;
            }
            Ok(n) => {
                let room = MAX_OUTPUT_SIZE.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    buf
}

#[async_trait::async_trait]
impl Plugin for RuncommandPlugin {
    fn name(&self) -> &str {
        "runcommand"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        // kdeconnect-kde kdeconnect_runcommand.json X-KdeConnect-SupportedPacketType
        vec!["kdeconnect.runcommand.request".to_string()]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // kdeconnect-kde also declares kdeconnect.runcommand.output, but we do
        // not stream output (see module docs), so we advertise only what we
        // honor: the command-list advertisement.
        vec!["kdeconnect.runcommand".to_string()]
    }

    fn on_connected(&self, device_id: &str) -> Vec<Packet> {
        // kdeconnect-kde sends the command list on connect
        // (runcommandplugin.cpp:156-159 connected() -> sendConfig()).
        vec![self.command_list_packet(device_id)]
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let body = &packet.body;

        // Flow 1: phone asks for our command list (RunCommandPlugin.java:250-254;
        // kdeconnect-kde answers with sendConfig, runcommandplugin.cpp:52-55).
        if body
            .get("requestCommandList")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            debug!(
                device_id = %device_id,
                event = "runcommand_list_requested",
                "Device requested the command list"
            );
            return Ok(Some(vec![self.command_list_packet(device_id)]));
        }

        // Flow 2: phone asks to run a command by key (RunCommandPlugin.java:242-248).
        if let Some(key) = body.get("key").and_then(|v| v.as_str()) {
            let entry = match self.lookup(device_id, key) {
                Some(entry) => entry,
                None => {
                    // Default posture: nothing is allowlisted, so every
                    // request lands here (runcommandplugin.cpp:82-84 logs the
                    // same "not a configured command" case upstream).
                    warn!(
                        device_id = %device_id,
                        key = %key,
                        event = "runcommand_blocked",
                        "Command key is not in the allowlist"
                    );
                    return Ok(None);
                }
            };

            let plugin = self.clone();
            let device_id = device_id.to_string();
            let key = key.to_string();

            tokio::spawn(async move {
                let result = plugin
                    .execute_command(&device_id, &key, &entry.command)
                    .await;
                if let Ok(mut executed) = plugin.executed.write() {
                    if executed.len() >= MAX_EXECUTED_RECORDS {
                        executed.remove(0);
                    }
                    executed.push(result);
                }
            });

            return Ok(None);
        }

        // `setup` / `stop` (RunCommandPlugin.java:260-270): parsed and ignored
        // — no config UI to open, no process table to stop.
        debug!(
            device_id = %device_id,
            event = "runcommand_request_ignored",
            "runcommand.request without key/requestCommandList ignored"
        );
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::config::settings::{RuncommandCommand, RuncommandConfig};

    fn request_packet(body: serde_json::Value) -> Packet {
        Packet::new("kdeconnect.runcommand.request".to_string(), body)
    }

    #[tokio::test]
    async fn test_runcommand_plugin_name() {
        let plugin = RuncommandPlugin::new();
        assert_eq!(plugin.name(), "runcommand");
    }

    #[tokio::test]
    async fn test_runcommand_capabilities() {
        // Matches kdeconnect-kde kdeconnect_runcommand.json minus
        // kdeconnect.runcommand.output, which we deliberately do NOT advertise
        // because we don't stream output.
        let plugin = RuncommandPlugin::new();
        assert_eq!(
            plugin.incoming_capabilities(),
            vec!["kdeconnect.runcommand.request".to_string()]
        );
        assert_eq!(
            plugin.outgoing_capabilities(),
            vec!["kdeconnect.runcommand".to_string()]
        );
    }

    /// Fixture: tests/fixtures/upstream-wire/runcommand/command_list_empty.json
    ///   kdeconnect-kde@f5ed3ed8 plugins/runcommand/runcommandplugin.cpp:188-195
    #[tokio::test]
    async fn test_advertisement_wire_shape_empty_allowlist() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/runcommand/command_list_empty.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read runcommand empty fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = RuncommandPlugin::new();
        let packet = plugin.command_list_packet("device1");
        // Production posture: empty allowlist -> commandList is the JSON
        // STRING "{}", canAddCommand false. Shape verified against
        // kdeconnect-kde runcommandplugin.cpp:188-195 and Android's parser
        // RunCommandPlugin.java:140 (getString -> new JSONObject).
        // We send canAddCommand=false even though upstream sends true
        // (runcommandplugin.cpp:192); the rust allowlist is one-way (we
        // push commands to the phone, the phone never pushes them to us).
        // Recorded as INTENTIONAL-DIVERGENCE in the runcommand ledger row.
        assert_eq!(packet.packet_type, "kdeconnect.runcommand");
        assert_eq!(packet.body["commandList"], upstream_body["commandList"]);
        assert_eq!(
            packet.body["canAddCommand"],
            serde_json::json!(false),
            "we intentionally advertise canAddCommand=false"
        );
        assert_ne!(
            packet.body["canAddCommand"], upstream_body["canAddCommand"],
            "upstream advertises canAddCommand=true (runcommandplugin.cpp:192)"
        );
    }

    /// Fixture: tests/fixtures/upstream-wire/runcommand/command_list_populated.json
    ///   The commandList string is JSON; Android parses via
    ///   RunCommandPlugin.java:140 (`new JSONObject(np.getString("commandList"))`).
    #[tokio::test]
    async fn test_advertisement_wire_shape_populated() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/runcommand/command_list_populated.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read runcommand populated fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = RuncommandPlugin::new();
        plugin.allow_command("device1", "suspend", "Suspend", "systemctl suspend");
        let packet = plugin.command_list_packet("device1");
        // The wire-level outer shape matches the upstream-derived fixture.
        assert_eq!(packet.packet_type, "kdeconnect.runcommand");
        assert_eq!(packet.body["commandList"], upstream_body["commandList"]);
        // The commandList string is itself JSON; verify its parsed shape.
        let list_str = packet.body["commandList"].as_str().unwrap();
        let list: serde_json::Value = serde_json::from_str(list_str).unwrap();
        assert_eq!(
            list,
            serde_json::json!({
                "suspend": { "name": "Suspend", "command": "systemctl suspend" }
            })
        );
    }

    #[tokio::test]
    async fn test_on_connected_sends_advertisement() {
        // kdeconnect-kde: connected() -> sendConfig()
        // (runcommandplugin.cpp:183-186).
        let plugin = RuncommandPlugin::new();
        let packets = plugin.on_connected("device1");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, "kdeconnect.runcommand");
    }

    /// Fixture: tests/fixtures/upstream-wire/runcommand/request_command_list.json
    ///   kdeconnect-android@a88f6fa0 RunCommandPlugin.java:258-262
    ///   `np.set("requestCommandList", true)` is the EXACT body the phone
    ///   sends to ask for the command list.
    #[tokio::test]
    async fn test_request_command_list_exact_phone_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/runcommand/request_command_list.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read request-command-list fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = RuncommandPlugin::new();
        let reply = plugin
            .handle_packet("device1", request_packet(upstream_body))
            .await
            .unwrap()
            .expect("requestCommandList must be answered");
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].packet_type, "kdeconnect.runcommand");
        assert_eq!(reply[0].body["commandList"], serde_json::json!("{}"));
    }

    /// Fixture: tests/fixtures/upstream-wire/runcommand/request_key.json
    ///   EXACT body the phone sends when the user taps a command:
    ///   kdeconnect-android@a88f6fa0 RunCommandPlugin.java:251-256
    ///     np.set("key", cmdKey)
    /// The request is BLOCKED here because the allowlist is empty — the
    /// test's purpose is to certify the phone-shape parity while exercising
    /// the blocked-by-default path.
    #[tokio::test]
    async fn test_blocked_by_default_exact_phone_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/runcommand/request_key.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read request-key fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = RuncommandPlugin::new();
        let result = plugin
            .handle_packet("device1", request_packet(upstream_body))
            .await
            .unwrap();
        assert!(result.is_none());
        plugin.wait_for_commands(1, 200).await;
        assert!(plugin.executed_commands().is_empty());
    }

    #[tokio::test]
    async fn test_allowed_command_executed() {
        let plugin = RuncommandPlugin::new();
        plugin.allow_command("device1", "greet", "Greet", "echo hello");
        let result = plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "greet" })),
            )
            .await
            .unwrap();
        assert!(result.is_none());
        plugin.wait_for_commands(1, 1000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].key, "greet");
        assert_eq!(executed[0].command, "echo hello");
        assert!(executed[0].success);
        assert_eq!(executed[0].exit_code, Some(0));
        assert_eq!(executed[0].stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn test_different_devices_separate_lists() {
        let plugin = RuncommandPlugin::new();
        plugin.allow_command("device1", "a", "A", "echo device1");
        plugin.allow_command("device2", "b", "B", "echo device2");

        // device2 asking for device1's key is refused.
        plugin
            .handle_packet("device2", request_packet(serde_json::json!({ "key": "a" })))
            .await
            .unwrap();
        plugin
            .handle_packet("device1", request_packet(serde_json::json!({ "key": "a" })))
            .await
            .unwrap();
        plugin
            .handle_packet("device2", request_packet(serde_json::json!({ "key": "b" })))
            .await
            .unwrap();

        plugin.wait_for_commands(2, 1000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 2);

        let cmd1 = executed.iter().find(|c| c.key == "a").unwrap();
        assert_eq!(cmd1.device_id, "device1");
        assert_eq!(cmd1.stdout.trim(), "device1");

        let cmd2 = executed.iter().find(|c| c.key == "b").unwrap();
        assert_eq!(cmd2.device_id, "device2");
        assert_eq!(cmd2.stdout.trim(), "device2");
    }

    #[tokio::test]
    async fn test_command_not_found() {
        // Via /bin/sh -c (upstream's executor), a missing command exits 127.
        let plugin = RuncommandPlugin::new();
        plugin.allow_command("device1", "bad", "Bad", "nonexistent_command_xyz");
        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "bad" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 1000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert!(!executed[0].success);
        assert_eq!(executed[0].exit_code, Some(127));
    }

    #[tokio::test]
    async fn test_command_timeout() {
        let plugin = RuncommandPlugin::new().with_execution_timeout(Duration::from_millis(100));
        plugin.allow_command("device1", "nap", "Nap", "sleep 60");
        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "nap" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 2000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert!(!executed[0].success);
        assert!(executed[0].stderr.contains("timed out"));
    }

    #[tokio::test]
    async fn test_timed_out_command_is_killed_not_orphaned() {
        // The command writes a marker file 300ms in; the timeout is 100ms.
        // If the timeout merely drops the future (spawn_blocking + output()),
        // the orphaned /bin/sh finishes and the marker APPEARS. A correct
        // implementation kills the child, so the marker must never exist.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("orphan-marker");
        let cmd = format!("sleep 0.3 && touch {}", marker.display());
        let plugin = RuncommandPlugin::new().with_execution_timeout(Duration::from_millis(100));
        plugin.allow_command("device1", "slow", "Slow", &cmd);
        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "slow" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 2000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert!(executed[0].stderr.contains("timed out"));
        // Give any orphaned child ample time to finish and write the marker.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !marker.exists(),
            "timed-out command kept running past the timeout (orphaned child)"
        );
    }

    #[tokio::test]
    async fn test_executed_history_is_capped() {
        // More executions than MAX_EXECUTED_RECORDS must not grow the
        // history without bound; the oldest records are dropped.
        let plugin = RuncommandPlugin::new();
        let total = MAX_EXECUTED_RECORDS + 5;
        for i in 0..total {
            let key = format!("k{i}");
            plugin.allow_command("device1", &key, &key, "true");
            plugin
                .handle_packet("device1", request_packet(serde_json::json!({ "key": key })))
                .await
                .unwrap();
        }
        plugin.wait_for_commands(MAX_EXECUTED_RECORDS, 2000).await;
        // Let the last few spawned executions land.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), MAX_EXECUTED_RECORDS);
        // Distinct keys, all from the issued set.
        let keys: std::collections::HashSet<&str> =
            executed.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys.len(), MAX_EXECUTED_RECORDS);
        assert!(keys.iter().all(|k| k.starts_with('k')));
    }

    #[tokio::test]
    async fn test_timeout_kills_whole_process_group() {
        // A backgrounded subshell writes the marker 300ms in; the timeout is
        // 100ms. Killing only the direct child (the shell) orphans the
        // subshell and the marker APPEARS. A process-group kill takes the
        // subshell with it, so the marker must never exist.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("group-marker");
        let cmd = format!("(sleep 0.3; touch {}) & wait", marker.display());
        let plugin = RuncommandPlugin::new().with_execution_timeout(Duration::from_millis(100));
        plugin.allow_command("device1", "grp", "Grp", &cmd);
        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "grp" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 2000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert!(executed[0].stderr.contains("timed out"));
        // Give any orphaned subshell ample time to finish and write the marker.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !marker.exists(),
            "timeout killed only the shell; a background child survived to write the marker"
        );
    }

    #[tokio::test]
    async fn test_output_is_captured_streaming_with_cap() {
        // 1MB of stdout from a command that exits normally: the captured
        // output is exactly MAX_OUTPUT_SIZE. The capture must stream (drain
        // the pipe, keep the first 64KB) rather than buffer-everything-then-
        // truncate like `.output()` — the difference is unbounded memory on
        // a command that never stops writing.
        let plugin = RuncommandPlugin::new();
        plugin.allow_command(
            "device1",
            "flood",
            "Flood",
            "head -c 1000000 /dev/zero | tr '\\0' 'a'",
        );
        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "flood" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 5000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert!(executed[0].success);
        assert_eq!(executed[0].stdout.len(), MAX_OUTPUT_SIZE);
        assert!(executed[0].stdout.chars().all(|c| c == 'a'));
    }

    #[tokio::test]
    async fn test_infinite_output_is_capped_and_killed() {
        // `yes` writes forever. The execution must time out (not hang, not
        // grow memory without bound) and the recorded output must stay
        // within the cap.
        let plugin = RuncommandPlugin::new().with_execution_timeout(Duration::from_millis(200));
        plugin.allow_command("device1", "yes", "Yes", "yes");
        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "yes" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 5000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert!(executed[0].stderr.contains("timed out"));
        assert!(executed[0].stdout.len() <= MAX_OUTPUT_SIZE);
    }

    #[tokio::test]
    async fn test_register_from_config_advertises_and_executes() {
        // Config-driven commands are desktop-global: after
        // register_from_config, the advertisement is non-empty AND the
        // request for a configured key executes the configured shell
        // command. Drives the full config -> wire path.
        let plugin = RuncommandPlugin::new();
        let cfg = RuncommandConfig {
            commands: vec![RuncommandCommand {
                key: "greet".to_string(),
                name: "Greet".to_string(),
                command: "echo hello-config".to_string(),
            }],
        };
        plugin.register_from_config(&cfg);

        // Advertisement now reflects exactly what is executable.
        let packet = plugin.command_list_packet("device1");
        let list_str = packet.body["commandList"].as_str().unwrap();
        let list: serde_json::Value = serde_json::from_str(list_str).unwrap();
        assert_eq!(
            list,
            serde_json::json!({
                "greet": { "name": "Greet", "command": "echo hello-config" }
            })
        );

        // Request for the configured key executes through the same path
        // as the per-device test API.
        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "greet" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 1000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].key, "greet");
        assert_eq!(executed[0].command, "echo hello-config");
        assert!(executed[0].success);
        assert_eq!(executed[0].stdout.trim(), "hello-config");
    }

    #[tokio::test]
    async fn test_register_from_config_is_desktop_global() {
        // Config-driven commands apply to every device the daemon pairs
        // with: device1 and device2 both see the same list and can both
        // trigger execution by key.
        let plugin = RuncommandPlugin::new();
        let cfg = RuncommandConfig {
            commands: vec![RuncommandCommand {
                key: "global".to_string(),
                name: "Global".to_string(),
                command: "echo from-config".to_string(),
            }],
        };
        plugin.register_from_config(&cfg);

        for device in ["device1", "device2"] {
            let packet = plugin.command_list_packet(device);
            let list_str = packet.body["commandList"].as_str().unwrap();
            let list: serde_json::Value = serde_json::from_str(list_str).unwrap();
            assert!(
                list.get("global").is_some(),
                "device {device} should see the global command list"
            );
        }

        plugin
            .handle_packet(
                "device2",
                request_packet(serde_json::json!({ "key": "global" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 1000).await;
        let executed = plugin.executed_commands();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].device_id, "device2");
        assert_eq!(executed[0].stdout.trim(), "from-config");
    }

    #[tokio::test]
    async fn test_register_from_config_skips_malformed_entries() {
        // Entries with empty key / name / command must be skipped (warned)
        // and must NOT prevent valid siblings from loading. A bad entry in
        // the middle of a list still leaves the surrounding valid ones
        // registered.
        let plugin = RuncommandPlugin::new();
        let cfg = RuncommandConfig {
            commands: vec![
                RuncommandCommand {
                    key: "".to_string(),
                    name: "Empty key".to_string(),
                    command: "echo nothing".to_string(),
                },
                RuncommandCommand {
                    key: "good".to_string(),
                    name: "".to_string(),
                    command: "echo hi".to_string(),
                },
                RuncommandCommand {
                    key: "bad".to_string(),
                    name: "Bad command".to_string(),
                    command: "".to_string(),
                },
                RuncommandCommand {
                    key: "ok".to_string(),
                    name: "OK".to_string(),
                    command: "echo fine".to_string(),
                },
            ],
        };
        plugin.register_from_config(&cfg);

        // Only the valid entry survives; the three malformed ones are
        // skipped.
        let packet = plugin.command_list_packet("device1");
        let list_str = packet.body["commandList"].as_str().unwrap();
        let list: serde_json::Value = serde_json::from_str(list_str).unwrap();
        assert_eq!(list.as_object().unwrap().len(), 1);
        assert!(list.get("ok").is_some());
    }

    #[tokio::test]
    async fn test_register_from_config_skips_duplicate_keys() {
        // The first entry with a given key wins; later entries with the
        // same key are skipped (warned). Prevents the advertisement from
        // carrying duplicate keys, which would corrupt the JSON object
        // shape phones parse.
        let plugin = RuncommandPlugin::new();
        let cfg = RuncommandConfig {
            commands: vec![
                RuncommandCommand {
                    key: "shared".to_string(),
                    name: "First".to_string(),
                    command: "echo first".to_string(),
                },
                RuncommandCommand {
                    key: "shared".to_string(),
                    name: "Second".to_string(),
                    command: "echo second".to_string(),
                },
                RuncommandCommand {
                    key: "unique".to_string(),
                    name: "Unique".to_string(),
                    command: "echo unique".to_string(),
                },
            ],
        };
        plugin.register_from_config(&cfg);

        let packet = plugin.command_list_packet("device1");
        let list_str = packet.body["commandList"].as_str().unwrap();
        let list: serde_json::Value = serde_json::from_str(list_str).unwrap();
        assert_eq!(list.as_object().unwrap().len(), 2);
        assert_eq!(
            list["shared"],
            serde_json::json!({ "name": "First", "command": "echo first" })
        );
        assert!(list.get("unique").is_some());
    }

    #[tokio::test]
    async fn test_register_from_config_empty_config_preserves_blocked_by_default() {
        // An empty config (or absent section) keeps the production
        // posture: advertisement is the JSON string "{}", every key
        // request is refused.
        let plugin = RuncommandPlugin::new();
        plugin.register_from_config(&RuncommandConfig::default());

        let packet = plugin.command_list_packet("device1");
        assert_eq!(packet.body["commandList"], serde_json::json!("{}"));

        plugin
            .handle_packet(
                "device1",
                request_packet(serde_json::json!({ "key": "anything" })),
            )
            .await
            .unwrap();
        plugin.wait_for_commands(1, 200).await;
        assert!(plugin.executed_commands().is_empty());
    }

    #[tokio::test]
    async fn test_setup_and_stop_ignored() {
        // Upstream defines these (RunCommandPlugin.java:260-270); we have no
        // config UI and no process table, so they must be harmless no-ops.
        let plugin = RuncommandPlugin::new();
        for body in [
            serde_json::json!({ "setup": true }),
            serde_json::json!({ "stop": true }),
        ] {
            let result = plugin
                .handle_packet("device1", request_packet(body))
                .await
                .unwrap();
            assert!(result.is_none());
        }
        assert!(plugin.executed_commands().is_empty());
    }

    /// Panel NIT (review-20260820T235242Z, codex): re-registration must
    /// REPLACE the global set — keys absent from the new config stop
    /// being executable, and changed keys take the new definition.
    #[tokio::test]
    async fn test_register_from_config_re_registration_replaces_global_set() {
        let plugin = RuncommandPlugin::new();
        let cfg_a = RuncommandConfig {
            commands: vec![
                RuncommandCommand {
                    key: "a".to_string(),
                    name: "A".to_string(),
                    command: "echo a".to_string(),
                },
                RuncommandCommand {
                    key: "b".to_string(),
                    name: "B".to_string(),
                    command: "echo b".to_string(),
                },
            ],
        };
        plugin.register_from_config(&cfg_a);
        assert!(plugin.lookup("any-device", "a").is_some());
        assert!(plugin.lookup("any-device", "b").is_some());

        let cfg_b = RuncommandConfig {
            commands: vec![RuncommandCommand {
                key: "b".to_string(),
                name: "B2".to_string(),
                command: "echo b2".to_string(),
            }],
        };
        plugin.register_from_config(&cfg_b);

        assert!(
            plugin.lookup("any-device", "a").is_none(),
            "a key absent from the new config must stop being executable"
        );
        assert_eq!(
            plugin
                .lookup("any-device", "b")
                .expect("b should still be registered")
                .command,
            "echo b2"
        );
    }
}

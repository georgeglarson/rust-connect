//! CLI client mode
//!
//! Single Responsibility: the command-line surface for driving the running
//! daemon's REST API. `rust-connect` with no subcommand still runs the
//! daemon; a subcommand (devices, pair, ping, …) talks to the daemon over
//! localhost HTTP instead.

pub mod client;
pub mod output;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use serde_json::json;

use client::{ApiClient, ClientConfig};
use output::{format_uptime, DeviceRow};

/// Default REST API base URL (mirrors `AppSettings::default().api_port` on
/// the loopback bind).
pub const DEFAULT_API_URL: &str = "http://127.0.0.1:9090";

/// Hint appended when the daemon cannot be reached.
pub const UNREACHABLE_HINT: &str = "is the daemon running? systemctl --user status rust-connect";

/// Android closes its pairing window after ~30s; poll a little past it.
const PAIR_TIMEOUT: Duration = Duration::from_secs(35);
const PAIR_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The daemon's upload cap (MAX_UPLOAD_BYTES in api/handlers/share.rs);
/// files larger than this are rejected client-side before reading.
const MAX_SHARE_FILE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "rust-connect",
    version = crate::BUILD_VERSION,
    about = "A modern, API-first reimplementation of KDE Connect in Rust"
)]
pub struct Cli {
    /// Path to configuration file (TOML)
    #[arg(short, long, global = true)]
    pub config: Option<String>,

    /// TCP/UDP port
    #[arg(short, long)]
    pub port: Option<u16>,

    /// API server port
    #[arg(long, global = true)]
    pub api_port: Option<u16>,

    /// Log level
    #[arg(long, value_parser = ["trace", "debug", "info", "warn", "error"])]
    pub log_level: Option<String>,

    /// Device name (default: hostname)
    #[arg(long)]
    pub device_name: Option<String>,

    /// Disable the REST API server
    #[arg(long)]
    pub no_api: bool,

    /// Idle timeout in seconds (0=disabled)
    #[arg(long)]
    pub idle_timeout: Option<u64>,

    /// Daemon REST API base URL (client mode)
    #[arg(long, global = true, env = "RUST_CONNECT_API_URL")]
    pub api_url: Option<String>,

    /// API key (client mode); default: the data-dir api_key file.
    ///
    /// A key passed on the command line is visible to every user on the box
    /// via the process listing (`ps`, `/proc/<pid>/cmdline`). Prefer
    /// `RUST_CONNECT_API_KEY` or the data-dir key file.
    #[arg(long, global = true, env = "RUST_CONNECT_API_KEY")]
    pub api_key: Option<String>,

    /// Machine-readable JSON output (client mode)
    #[arg(long, global = true)]
    pub json: bool,

    /// Client subcommand; omit to run the daemon
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show daemon health and device count
    Status,
    /// List known devices
    Devices,
    /// Pair with a device (shows the verification code and waits)
    Pair {
        /// Device ID or unique prefix (see `rust-connect devices`)
        device_id: String,
        /// Accept an incoming pairing request without prompting
        #[arg(long)]
        yes: bool,
    },
    /// Unpair a device
    Unpair {
        /// Device ID or unique prefix (see `rust-connect devices`)
        device_id: String,
    },
    /// Send a ping to a device
    Ping {
        /// Device ID or unique prefix (see `rust-connect devices`)
        device_id: String,
    },
    /// Send a file to a device
    Share {
        /// Device ID or unique prefix (see `rust-connect devices`)
        device_id: String,
        /// Path to the file to send
        file: PathBuf,
    },
    /// Get or set the shared clipboard (default: get)
    Clipboard {
        #[command(subcommand)]
        action: Option<ClipboardAction>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ClipboardAction {
    /// Print the current clipboard content
    Get,
    /// Set the clipboard content on connected devices
    Set {
        /// Text to put on the clipboard
        text: String,
    },
}

/// Client-mode failure, classified by exit code.
#[derive(Debug)]
pub enum CliError {
    /// Transport-level failure (connection refused, timeout) — exit 2.
    Unreachable(String),
    /// The daemon answered with an error envelope or non-2xx status — exit 1.
    Api(String),
    /// Local usage error (unreadable file, missing API key) — exit 1.
    Usage(String),
    /// Writing output failed — exit 1.
    Io(std::io::Error),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Unreachable(_) => 2,
            Self::Api(_) | Self::Usage(_) | Self::Io(_) => 1,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) => write!(f, "error: {m}\n{UNREACHABLE_HINT}"),
            Self::Api(m) | Self::Usage(m) => write!(f, "error: {m}"),
            Self::Io(e) => write!(f, "error: {e}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Resolved client-mode connection settings.
pub struct ClientOpts {
    pub api_url: String,
    pub api_key: String,
}

impl Cli {
    /// Resolve the API base URL and key for client mode: `--api-url` /
    /// `RUST_CONNECT_API_URL` (clap `env` already folded into the field),
    /// then `--api-key` / `RUST_CONNECT_API_KEY`, then the daemon's own
    /// config when `--config` is given (api_bind/api_port for the URL,
    /// api_keys or the data-dir key file for auth), then the default
    /// data-dir `api_key` file the daemon persists on first run.
    pub fn resolve_client_opts(&self) -> Result<ClientOpts, CliError> {
        let file_settings = match &self.config {
            Some(path) => Some(
                crate::config::settings::AppSettings::load_from_file(&PathBuf::from(path))
                    .map_err(|e| CliError::Usage(format!("cannot read --config {path}: {e}")))?,
            ),
            None => None,
        };

        let api_url = self.api_url.clone().unwrap_or_else(|| {
            // Explicit flag beats config file; config beats the default.
            self.api_port
                .map(|p| format!("http://127.0.0.1:{p}"))
                .or_else(|| {
                    file_settings.as_ref().map(|s| {
                        // A daemon bound on a wildcard or IPv6 address
                        // doesn't make a portable client URL — map wildcard
                        // to loopback, bracket IPv6.
                        let host = match s.api_bind.as_str() {
                            "" | "0.0.0.0" | "::" | "::0" => "127.0.0.1".to_string(),
                            h if h.contains(':') => format!("[{h}]"),
                            h => h.to_string(),
                        };
                        format!("http://{host}:{}", s.api_port)
                    })
                })
                .unwrap_or_else(|| DEFAULT_API_URL.to_string())
        });
        let api_key = match &self.api_key {
            Some(key) => key.clone(),
            None => {
                let from_config = file_settings
                    .as_ref()
                    .and_then(|s| s.api_keys.first().cloned());
                let from_key_file = read_api_key_file(
                    &file_settings
                        .as_ref()
                        .map(|s| s.data_dir.clone())
                        .unwrap_or_else(default_data_dir),
                );
                from_config.or(from_key_file).ok_or_else(|| {
                    CliError::Usage(format!(
                        "no API key: pass --api-key, set RUST_CONNECT_API_KEY, \
                         or start the daemon once so it writes {}",
                        default_data_dir().join("api_key").display()
                    ))
                })?
            }
        };
        Ok(ClientOpts { api_url, api_key })
    }
}

/// The daemon's data directory: `RUST_CONNECT_DATA_DIR` when set (tests,
/// portable installs), otherwise the platform data dir — same rule as
/// `AppSettings::default`.
fn default_data_dir() -> PathBuf {
    std::env::var_os("RUST_CONNECT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from(".data"))
                .join("rust-connect")
        })
}

/// Read the persisted API key from `<data_dir>/api_key` (whitespace-trimmed).
fn read_api_key_file(data_dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(data_dir.join("api_key")).ok()?;
    let key = content.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

/// Entry point for client mode: resolve settings, build the HTTP client,
/// dispatch the subcommand. Output goes to `out` so tests can capture it.
pub fn run(cli: &Cli, command: &Command, out: &mut impl Write) -> Result<(), CliError> {
    let opts = cli.resolve_client_opts()?;
    let client = ApiClient::new(ClientConfig {
        base_url: opts.api_url,
        api_key: opts.api_key,
    });
    execute(&client, command, cli.json, out)
}

/// Dispatch a client subcommand against a prepared client.
pub fn execute(
    client: &ApiClient,
    command: &Command,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    match command {
        Command::Status => cmd_status(client, json, out),
        Command::Devices => cmd_devices(client, json, out),
        Command::Pair { device_id, yes } => cmd_pair(client, device_id, *yes, json, out),
        Command::Unpair { device_id } => cmd_unpair(client, device_id, json, out),
        Command::Ping { device_id } => cmd_ping(client, device_id, json, out),
        Command::Share { device_id, file } => cmd_share(client, device_id, file, json, out),
        Command::Clipboard { action } => cmd_clipboard(client, action.as_ref(), json, out),
    }
}

/// Extract `data` from the `{status, data, metadata}` envelope; responses
/// without an envelope (health) pass through whole.
fn envelope_data(body: &str) -> Result<serde_json::Value, CliError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| CliError::Api(format!("invalid JSON from daemon: {e}")))?;
    Ok(value.get("data").cloned().unwrap_or(value))
}

fn device_path(device_id: &str) -> String {
    // IDs resolved from the daemon are wire-format-safe, but percent-encode
    // anyway — a hand-typed ID must not reshape the request path.
    format!(
        "/api/v1/devices/{}",
        crate::cli::client::percent_encode(device_id)
    )
}

/// Fetch the full device list, following pagination — the API defaults to
/// 50 devices per page, so a bare first-page GET can silently drop devices.
/// Returns a data-shaped object: { "devices": [...], "total": N }.
fn fetch_all_devices(client: &ApiClient) -> Result<serde_json::Value, CliError> {
    const PAGE_SIZE: usize = 100;
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut page = 1;
    loop {
        let body = client.get(&format!("/api/v1/devices?page={page}&limit={PAGE_SIZE}"))?;
        let data = envelope_data(&body)?;
        let batch = data
            .get("devices")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let total = data.get("total").and_then(|t| t.as_u64()).unwrap_or(0) as usize;
        let batch_len = batch.len();
        all.extend(batch);
        if all.len() >= total || batch_len < PAGE_SIZE {
            return Ok(json!({ "devices": all, "total": total }));
        }
        page += 1;
    }
}

/// Resolve a CLI device argument to a full device ID. `devices` prints
/// truncated IDs, so commands accept a unique (case-sensitive) prefix of a
/// known device's full ID; full IDs keep working as exact matches.
fn resolve_device_id(client: &ApiClient, id_or_prefix: &str) -> Result<String, CliError> {
    let data = fetch_all_devices(client)?;
    let ids: Vec<&str> = data
        .get("devices")
        .and_then(|d| d.as_array())
        .map(|devices| {
            devices
                .iter()
                .filter_map(|d| d.get("id").and_then(|v| v.as_str()))
                .collect()
        })
        .unwrap_or_default();
    match_device_id(&ids, id_or_prefix)
}

/// Pure matching behind `resolve_device_id`: exact match wins, otherwise a
/// unique prefix; ambiguity and no-match are usage errors.
fn match_device_id(ids: &[&str], id_or_prefix: &str) -> Result<String, CliError> {
    if let Some(exact) = ids.iter().find(|id| **id == id_or_prefix) {
        return Ok(exact.to_string());
    }
    let matches: Vec<&str> = ids
        .iter()
        .filter(|id| id.starts_with(id_or_prefix))
        .copied()
        .collect();
    match matches.len() {
        1 => Ok(matches[0].to_string()),
        0 => Err(CliError::Usage(format!(
            "no device matches '{id_or_prefix}'"
        ))),
        _ => Err(CliError::Usage(format!(
            "'{id_or_prefix}' is ambiguous — matches: {}",
            matches.join(", ")
        ))),
    }
}

fn cmd_status(client: &ApiClient, json: bool, out: &mut impl Write) -> Result<(), CliError> {
    let health_body = client.get("/api/v1/health")?;

    let health: serde_json::Value = serde_json::from_str(&health_body)
        .map_err(|e| CliError::Api(format!("invalid JSON from daemon: {e}")))?;
    let devices = fetch_all_devices(client)?;

    if json {
        let combined = json!({ "health": health, "devices": devices });
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&combined)
                .map_err(|e| CliError::Api(format!("failed to render JSON: {e}")))?
        )?;
        return Ok(());
    }

    let uptime = health
        .get("uptime_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = devices.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    writeln!(out, "Daemon:  ok (up {})", format_uptime(uptime))?;
    writeln!(out, "Devices: {total} known")?;
    Ok(())
}

fn cmd_devices(client: &ApiClient, json: bool, out: &mut impl Write) -> Result<(), CliError> {
    let data = fetch_all_devices(client)?;
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&data)
                .map_err(|e| CliError::Api(format!("failed to render JSON: {e}")))?
        )?;
        return Ok(());
    }

    let devices = data
        .get("devices")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();

    let mut rows = Vec::with_capacity(devices.len());
    for device in &devices {
        let id = device
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // paired_at rides along in the list summary (the daemon overlays it
        // from the pairing store) — no per-device detail fetch needed.
        let paired_at = device
            .get("paired_at")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        rows.push(DeviceRow {
            id,
            name: device
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            device_type: device
                .get("device_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            state: device
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            paired_at,
        });
    }

    write!(out, "{}", output::render_devices_table(&rows))?;
    Ok(())
}

fn cmd_pair(
    client: &ApiClient,
    device_id: &str,
    yes: bool,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    use std::io::IsTerminal;

    let device_id = resolve_device_id(client, device_id)?;

    // Detail first: an incoming request must be shown and confirmed, never
    // accepted blind (POSTing /pair while a request is pending IS the accept).
    let detail_body = client.get(&device_path(&device_id))?;
    let detail = envelope_data(&detail_body)?;

    if detail.get("pair_state").and_then(|v| v.as_str()) == Some("requested_by_peer") {
        if !json {
            writeln!(out, "Incoming pairing request from {device_id}.")?;
            match detail.get("verification_key").and_then(|v| v.as_str()) {
                Some(sas) => {
                    writeln!(out, "\nVerification key (SAS): {sas}\n")?;
                    writeln!(out, "Compare this code with the one shown on your phone.")?;
                }
                None => {
                    writeln!(
                        out,
                        "Verification key unavailable — compare on the phone side."
                    )?;
                }
            }
        }

        if !yes {
            if json {
                return Err(CliError::Api(
                    "an incoming pairing request is pending; --json cannot prompt \
                     for confirmation — compare the verification key (it is on the \
                     device record) and re-run with --yes"
                        .to_string(),
                ));
            }
            if !std::io::stdin().is_terminal() {
                return Err(CliError::Api(
                    "an incoming pairing request is pending; re-run with --yes \
                     to accept it non-interactively"
                        .to_string(),
                ));
            }
            write!(out, "Accept pairing? [y/N] ")?;
            out.flush()?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|e| CliError::Api(format!("failed to read confirmation: {e}")))?;
            let answer = answer.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                writeln!(
                    out,
                    "Not accepted. The request expires on its own within ~25s."
                )?;
                return Ok(());
            }
        }

        // The confirmation is unbounded but the request is not: it expires
        // after ~25s. Re-read the device before accepting, because POSTing
        // /pair with no request pending does NOT fail — it starts a new
        // OUTGOING pairing, which would both report false success and put an
        // unexpected request on the phone. Re-reading also catches the phone
        // re-sending during the prompt: that is a different request with a
        // different key, and accepting it would pair on a key the user never
        // compared.
        let recheck_body = client.get(&device_path(&device_id))?;
        let recheck = envelope_data(&recheck_body)?;
        if recheck.get("pair_state").and_then(|v| v.as_str()) != Some("requested_by_peer") {
            return Err(CliError::Api(format!(
                "the pairing request from {device_id} is no longer pending — its \
                 ~25s window closed; ask the phone to send it again, then re-run"
            )));
        }
        let shown_key = detail.get("verification_key").and_then(|v| v.as_str());
        let current_key = recheck.get("verification_key").and_then(|v| v.as_str());
        if shown_key != current_key {
            return Err(CliError::Api(
                "the pending request changed while awaiting confirmation: its \
                 verification key no longer matches the one displayed. Re-run \
                 and compare the new key before accepting."
                    .to_string(),
            ));
        }

        let body = client.post_empty(&format!("{}/pair", device_path(&device_id)))?;
        let data = envelope_data(&body)?;
        let status = data
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if status != "paired" {
            return Err(CliError::Api(format!(
                "the accept did not complete the pairing — the daemon reported \
                 '{status}'. The request most likely expired and an outgoing \
                 pairing was started instead; check the phone, and run \
                 `rust-connect unpair {device_id}` if it shows an unexpected request."
            )));
        }
        if json {
            writeln!(out, "{body}")?;
        } else {
            writeln!(out, "Paired with {device_id}.")?;
        }
        return Ok(());
    }

    // No incoming request pending: daemon-initiated pairing (existing flow).
    let body = client.post_empty(&format!("{}/pair", device_path(&device_id)))?;
    let data = envelope_data(&body)?;

    // Accepting the phone's incoming request completes immediately.
    if data.get("status").and_then(|v| v.as_str()) == Some("paired") {
        if json {
            writeln!(out, "{body}")?;
        } else {
            writeln!(out, "Paired with {device_id}.")?;
        }
        return Ok(());
    }

    // Daemon-initiated: surface the SAS and wait for the phone to accept.
    let mut detail_body = client.get(&device_path(&device_id))?;
    let mut detail = envelope_data(&detail_body)?;

    if !json {
        writeln!(out, "Pairing request sent to {device_id}.")?;
        if let Some(sas) = detail.get("verification_key").and_then(|v| v.as_str()) {
            writeln!(out, "\nVerification key (SAS): {sas}\n")?;
            writeln!(out, "Compare this code with the one shown on your phone.")?;
        }
        writeln!(
            out,
            "On Android the pairing request arrives as a silent notification — \
             check the notification shade."
        )?;
        writeln!(
            out,
            "Waiting for pairing to complete (up to {}s)...",
            PAIR_TIMEOUT.as_secs()
        )?;
        out.flush()?;
    }

    let deadline = Instant::now() + PAIR_TIMEOUT;
    loop {
        if is_paired(&detail) {
            if json {
                writeln!(out, "{detail_body}")?;
            } else {
                writeln!(out, "Paired with {device_id}.")?;
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::Api(format!(
                "pairing timed out after {}s — the phone's pairing window \
                 closes after ~30s; run `rust-connect pair {device_id}` again to retry",
                PAIR_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(PAIR_POLL_INTERVAL);
        detail_body = client.get(&device_path(&device_id))?;
        detail = envelope_data(&detail_body)?;
    }
}

/// A device counts as paired only once the pairing store stamped
/// `paired_at`. The record's `state` is CONNECTION state — an unpaired
/// phone sits at "connected" for as long as the link is up, so it must
/// not count here (the pair watch would report success instantly).
fn is_paired(device: &serde_json::Value) -> bool {
    device.get("paired_at").is_some_and(|v| !v.is_null())
}

fn cmd_unpair(
    client: &ApiClient,
    device_id: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let device_id = resolve_device_id(client, device_id)?;
    let body = client.delete(&format!("{}/unpair", device_path(&device_id)))?;
    if json {
        writeln!(out, "{body}")?;
    } else {
        writeln!(out, "Unpaired {device_id}.")?;
    }
    Ok(())
}

fn cmd_ping(
    client: &ApiClient,
    device_id: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let device_id = resolve_device_id(client, device_id)?;
    let body = client.post_json("/api/v1/ping", &json!({ "device_id": device_id }))?;
    if json {
        writeln!(out, "{body}")?;
    } else {
        writeln!(out, "Ping sent to {device_id}.")?;
    }
    Ok(())
}

fn cmd_share(
    client: &ApiClient,
    device_id: &str,
    file: &Path,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    // Fail before any network I/O: the daemon rejects uploads over its
    // 100 MiB cap, and unreadable files are local usage errors.
    let metadata = std::fs::metadata(file)
        .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", file.display())))?;
    if !metadata.is_file() {
        return Err(CliError::Usage(format!(
            "{} is not a regular file",
            file.display()
        )));
    }
    if metadata.len() > MAX_SHARE_FILE_BYTES {
        return Err(CliError::Usage(format!(
            "{} is {} bytes — over the daemon's 100 MiB upload cap",
            file.display(),
            metadata.len()
        )));
    }
    if metadata.len() == 0 {
        return Err(CliError::Usage(format!(
            "refusing to send empty file {}",
            file.display()
        )));
    }
    let handle = std::fs::File::open(file)
        .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", file.display())))?;
    let device_id = resolve_device_id(client, device_id)?;
    let filename = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shared_file".to_string());

    let path = format!(
        "{}/share/send?filename={}",
        device_path(&device_id),
        client::percent_encode(&filename)
    );
    let body = client.post_file(&path, "application/octet-stream", handle)?;

    if json {
        writeln!(out, "{body}")?;
    } else {
        let size = envelope_data(&body)
            .ok()
            .and_then(|d| d.get("size").and_then(|v| v.as_u64()))
            .unwrap_or(metadata.len());
        writeln!(out, "Sent {filename} ({size} bytes) to {device_id}.")?;
    }
    Ok(())
}

fn cmd_clipboard(
    client: &ApiClient,
    action: Option<&ClipboardAction>,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    match action {
        None | Some(ClipboardAction::Get) => {
            let body = client.get("/api/v1/clipboard")?;
            if json {
                writeln!(out, "{body}")?;
            } else {
                let data = envelope_data(&body)?;
                let content = data
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                writeln!(out, "{content}")?;
            }
        }
        Some(ClipboardAction::Set { text }) => {
            let body = client.post_json("/api/v1/clipboard", &json!({ "content": text }))?;
            if json {
                writeln!(out, "{body}")?;
            } else {
                let devices = envelope_data(&body)
                    .ok()
                    .and_then(|d| d.get("devices").and_then(|v| v.as_u64()))
                    .unwrap_or(0);
                writeln!(out, "Clipboard sent to {devices} device(s).")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn test_no_subcommand_is_daemon_mode() {
        let cli = parse(&["rust-connect"]).expect("parse");
        assert!(cli.command.is_none());
        let cli = parse(&["rust-connect", "--port", "1716", "--no-api"]).expect("parse");
        assert!(cli.command.is_none());
        assert_eq!(cli.port, Some(1716));
        assert!(cli.no_api);
    }

    #[test]
    fn test_subcommands_parse() {
        let cli = parse(&["rust-connect", "status"]).expect("parse");
        assert!(matches!(cli.command, Some(Command::Status)));

        let cli = parse(&["rust-connect", "devices"]).expect("parse");
        assert!(matches!(cli.command, Some(Command::Devices)));

        let cli = parse(&["rust-connect", "pair", "abc123"]).expect("parse");
        match cli.command {
            Some(Command::Pair { device_id, yes: _ }) => assert_eq!(device_id, "abc123"),
            other => panic!("expected pair, got {other:?}"),
        }

        let cli = parse(&["rust-connect", "unpair", "abc123"]).expect("parse");
        assert!(matches!(cli.command, Some(Command::Unpair { .. })));

        let cli = parse(&["rust-connect", "ping", "abc123"]).expect("parse");
        assert!(matches!(cli.command, Some(Command::Ping { .. })));

        let cli = parse(&["rust-connect", "share", "abc123", "/tmp/f.pdf"]).expect("parse");
        match cli.command {
            Some(Command::Share { device_id, file }) => {
                assert_eq!(device_id, "abc123");
                assert_eq!(file, PathBuf::from("/tmp/f.pdf"));
            }
            other => panic!("expected share, got {other:?}"),
        }
    }

    #[test]
    fn test_clipboard_actions() {
        let cli = parse(&["rust-connect", "clipboard"]).expect("parse");
        match cli.command {
            Some(Command::Clipboard { action }) => assert!(action.is_none()),
            other => panic!("expected clipboard, got {other:?}"),
        }

        let cli = parse(&["rust-connect", "clipboard", "get"]).expect("parse");
        match cli.command {
            Some(Command::Clipboard {
                action: Some(ClipboardAction::Get),
            }) => {}
            other => panic!("expected clipboard get, got {other:?}"),
        }

        let cli = parse(&["rust-connect", "clipboard", "set", "hello world"]).expect("parse");
        match cli.command {
            Some(Command::Clipboard {
                action: Some(ClipboardAction::Set { text }),
            }) => assert_eq!(text, "hello world"),
            other => panic!("expected clipboard set, got {other:?}"),
        }
    }

    #[test]
    fn test_client_flags_work_before_and_after_subcommand() {
        let cli = parse(&["rust-connect", "--json", "devices"]).expect("parse");
        assert!(cli.json);
        let cli = parse(&["rust-connect", "devices", "--json"]).expect("parse");
        assert!(cli.json);

        let cli = parse(&[
            "rust-connect",
            "devices",
            "--api-url",
            "http://127.0.0.1:1234",
            "--api-key",
            "k",
        ])
        .expect("parse");
        assert_eq!(cli.api_url.as_deref(), Some("http://127.0.0.1:1234"));
        assert_eq!(cli.api_key.as_deref(), Some("k"));
    }

    #[test]
    fn test_unknown_subcommand_rejected() {
        assert!(parse(&["rust-connect", "frobnicate"]).is_err());
    }

    #[test]
    fn test_resolve_opts_flag_wins_and_defaults_apply() {
        let cli = parse(&["rust-connect", "devices", "--api-key", "flag-key"]).expect("parse");
        let opts = cli.resolve_client_opts().expect("resolve");
        assert_eq!(opts.api_key, "flag-key");
        assert_eq!(opts.api_url, DEFAULT_API_URL);
    }

    #[test]
    fn test_read_api_key_file() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        assert!(read_api_key_file(temp.path()).is_none());

        std::fs::write(temp.path().join("api_key"), "  some-key\n").expect("write");
        assert_eq!(read_api_key_file(temp.path()).as_deref(), Some("some-key"));

        std::fs::write(temp.path().join("api_key"), "  \n").expect("write");
        assert!(read_api_key_file(temp.path()).is_none());
    }

    #[test]
    fn test_envelope_data_unwraps_and_passes_through() {
        let enveloped = r#"{"status":"ok","data":{"total":2},"metadata":{}}"#;
        assert_eq!(
            envelope_data(enveloped).expect("parse"),
            json!({"total": 2})
        );

        let bare = r#"{"status":"ok","uptime_seconds":5}"#;
        assert_eq!(
            envelope_data(bare).expect("parse"),
            json!({"status": "ok", "uptime_seconds": 5})
        );

        assert!(envelope_data("not json").is_err());
    }

    #[test]
    fn test_match_device_id_exact_match_wins() {
        let ids = ["abcdef1234567890", "abcdef9999999999"];
        // An exact full ID resolves even when it prefixes another ID.
        assert_eq!(
            match_device_id(&ids, "abcdef1234567890").expect("exact match"),
            "abcdef1234567890"
        );
    }

    #[test]
    fn test_match_device_id_unique_prefix() {
        let ids = ["0123456789abcdef0123456789abcdef", "f00d00baa1"];
        assert_eq!(
            match_device_id(&ids, "01234567").expect("unique prefix"),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn test_match_device_id_ambiguous_prefix_lists_candidates() {
        let ids = ["abcdef1234567890", "abcdef9999999999", "f00d00baa1"];
        let err = match_device_id(&ids, "abcdef").expect_err("ambiguous prefix");
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(msg.contains("abcdef1234567890"), "{msg}");
        assert!(msg.contains("abcdef9999999999"), "{msg}");
        assert!(!msg.contains("f00d00baa1"), "{msg}");
    }

    #[test]
    fn test_match_device_id_no_match() {
        let ids = ["0123456789abcdef0123456789abcdef"];
        let err = match_device_id(&ids, "zzzz").expect_err("no match");
        assert!(err.to_string().contains("no device matches 'zzzz'"));
        assert!(match_device_id(&[], "anything").is_err());
    }

    #[test]
    fn test_is_paired() {
        assert!(is_paired(&json!({"paired_at": "2026-07-30T12:00:00Z"})));
        // `state` is connection state: an unpaired phone sits at
        // "connected" — it must NOT count as paired.
        assert!(!is_paired(&json!({"state": "connected"})));
        assert!(!is_paired(&json!({"state": "paired"})));
        assert!(!is_paired(&json!({"state": "pairing", "paired_at": null})));
        assert!(!is_paired(&json!({"state": "discovered"})));
    }

    #[test]
    fn test_exit_codes() {
        assert_eq!(CliError::Unreachable("x".into()).exit_code(), 2);
        assert_eq!(CliError::Api("x".into()).exit_code(), 1);
        assert_eq!(CliError::Usage("x".into()).exit_code(), 1);
        assert!(CliError::Unreachable("x".into())
            .to_string()
            .contains("systemctl --user status rust-connect"));
    }
}

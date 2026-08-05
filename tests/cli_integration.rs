//! Integration tests for CLI client mode against a mock daemon.
//!
//! The mock is a tiny std::net::TcpListener HTTP/1.1 server — enough to
//! assert the client's request shape (method, path, X-API-Key header, body)
//! and feed it canned envelopes.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rust_connect::cli::client::{ApiClient, ClientConfig};
use rust_connect::cli::{execute, CliError, ClipboardAction, Command};

struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// Read one full HTTP request: headers up to CRLFCRLF, then the
/// Content-Length body (requests without a body end at the headers).
fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (headers_end, content_length) = loop {
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "connection closed mid-request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
            let len = headers
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            break (pos + 4, len);
        }
    };
    while buf.len() < headers_end + content_length {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "connection closed mid-body");
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(buf).expect("request utf8")
}

impl MockServer {
    /// `respond` maps (request_line, body) -> (status, body). The server
    /// handles one request per connection until dropped.
    fn start(respond: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let thread_requests = requests.clone();
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || loop {
            if thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let request = read_request(&mut stream);
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    let body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, b)| b)
                        .unwrap_or_default()
                        .to_string();
                    thread_requests.lock().expect("requests lock").push(request);

                    let (status, response_body) = respond(&request_line, &body);
                    let reason = if (200..300).contains(&status) {
                        "OK"
                    } else {
                        "ERROR"
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                        response_body.len()
                    );
                    stream
                        .write_all(response.as_bytes())
                        .expect("write response");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        });

        Self {
            addr,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn client(&self) -> ApiClient {
        ApiClient::new(ClientConfig {
            base_url: self.url(),
            api_key: "test-key".to_string(),
        })
    }

    fn recorded(&self) -> Vec<String> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Wake the accept loop so it notices the stop flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn envelope(data: serde_json::Value) -> String {
    serde_json::json!({
        "status": "ok",
        "data": data,
        "metadata": { "timestamp": "2026-08-03T00:00:00Z", "request_id": "r1" },
    })
    .to_string()
}

#[test]
fn test_devices_table_request_shape_and_output() {
    let server = MockServer::start(|request_line, _| {
        if request_line.starts_with("GET /api/v1/devices?") {
            (
                200,
                envelope(serde_json::json!({
                    "devices": [{
                        "id": "0123456789abcdef0123456789abcdef",
                        "name": "Pixel 8",
                        "device_type": "phone",
                        "state": "connected",
                        "last_seen": "2026-08-03T00:00:00Z",
                        "paired_at": "2026-07-30T12:34:56Z"
                    }],
                    "total": 1
                })),
            )
        } else {
            (
                404,
                r#"{"status":"error","error":{"code":"NOT_FOUND","message":"nope"}}"#.into(),
            )
        }
    });

    let mut out = Vec::new();
    execute(&server.client(), &Command::Devices, false, &mut out).expect("devices succeeds");
    let output = String::from_utf8(out).expect("utf8");

    let requests = server.recorded();
    assert!(
        requests[0].starts_with("GET /api/v1/devices?page="),
        "list request: {}",
        requests[0]
    );
    // paired_at rides in the list summary — no per-device detail fetch.
    assert_eq!(requests.len(), 1, "no N+1 detail requests: {:?}", requests);
    assert!(
        requests
            .iter()
            .all(|r| r.to_ascii_lowercase().contains("x-api-key: test-key")),
        "every request carries the API key header"
    );

    assert!(output.contains("ID"), "table header: {output}");
    assert!(output.contains("01234567"), "short id: {output}");
    assert!(output.contains("Pixel 8"), "name: {output}");
    assert!(output.contains("phone"), "type: {output}");
    assert!(output.contains("connected"), "state: {output}");
    assert!(output.contains("2026-07-30 12:34"), "paired_at: {output}");
}

#[test]
fn test_devices_json_merged_output() {
    let server = MockServer::start(|_, _| {
        (
            200,
            envelope(serde_json::json!({ "devices": [], "total": 0 })),
        )
    });

    let mut out = Vec::new();
    execute(&server.client(), &Command::Devices, true, &mut out).expect("devices succeeds");
    let output = String::from_utf8(out).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON output");
    // --json prints the merged (all-pages) data object, not one page's
    // envelope.
    assert_eq!(parsed["total"], 0);
    assert_eq!(parsed["devices"], serde_json::json!([]));
}

#[test]
fn test_pair_shows_sas_and_polls_until_paired() {
    let server = MockServer::start(|request_line, _| {
        if request_line.starts_with("GET /api/v1/devices?") {
            (
                200,
                envelope(serde_json::json!({
                    "devices": [{ "id": "dev-a", "name": "Pixel 8" }],
                    "total": 1
                })),
            )
        } else if request_line == "POST /api/v1/devices/dev-a/pair HTTP/1.1" {
            (
                200,
                envelope(serde_json::json!({
                    "device_id": "dev-a",
                    "status": "pairing_initiated"
                })),
            )
        } else if request_line == "GET /api/v1/devices/dev-a HTTP/1.1" {
            (
                200,
                envelope(serde_json::json!({
                    "id": "dev-a",
                    "state": "paired",
                    "paired_at": "2026-08-03T00:00:00Z",
                    "verification_key": "00F8F3CE",
                    "pair_state": "not_paired"
                })),
            )
        } else {
            (
                404,
                r#"{"status":"error","error":{"code":"NOT_FOUND","message":"nope"}}"#.into(),
            )
        }
    });

    let mut out = Vec::new();
    execute(
        &server.client(),
        &Command::Pair {
            device_id: "dev-a".to_string(),
            yes: false,
        },
        false,
        &mut out,
    )
    .expect("pair succeeds");
    let output = String::from_utf8(out).expect("utf8");

    assert!(output.contains("00F8F3CE"), "SAS printed: {output}");
    assert!(
        output.contains("notification shade"),
        "Android hint: {output}"
    );
    assert!(
        output.contains("Paired with dev-a."),
        "completion: {output}"
    );

    let requests = server.recorded();
    assert_eq!(
        requests[1].lines().next().expect("request line"),
        "GET /api/v1/devices/dev-a HTTP/1.1",
        "detail is fetched first to detect an incoming request"
    );
    assert_eq!(
        requests[2].lines().next().expect("request line"),
        "POST /api/v1/devices/dev-a/pair HTTP/1.1"
    );
    assert!(
        requests[3..]
            .iter()
            .all(|r| r.starts_with("GET /api/v1/devices/dev-a ")),
        "subsequent requests poll the device: {requests:?}"
    );
}

#[test]
fn test_pair_incoming_request_prints_sas_and_accepts_with_yes() {
    let server = MockServer::start(|request_line, _| {
        if request_line.starts_with("GET /api/v1/devices?") {
            (
                200,
                envelope(serde_json::json!({
                    "devices": [{ "id": "dev-in", "name": "test phone" }],
                    "total": 1
                })),
            )
        } else if request_line == "GET /api/v1/devices/dev-in HTTP/1.1" {
            (
                200,
                envelope(serde_json::json!({
                    "id": "dev-in",
                    "state": "connected",
                    "pair_state": "requested_by_peer",
                    "verification_key": "00F8F3CE"
                })),
            )
        } else if request_line == "POST /api/v1/devices/dev-in/pair HTTP/1.1" {
            (
                200,
                envelope(serde_json::json!({
                    "device_id": "dev-in",
                    "status": "paired"
                })),
            )
        } else {
            (
                404,
                r#"{"status":"error","error":{"code":"NOT_FOUND","message":"nope"}}"#.into(),
            )
        }
    });

    let mut out = Vec::new();
    execute(
        &server.client(),
        &Command::Pair {
            device_id: "dev-in".to_string(),
            yes: true,
        },
        false,
        &mut out,
    )
    .expect("accept succeeds");
    let output = String::from_utf8(out).expect("utf8");

    assert!(output.contains("00F8F3CE"), "SAS printed: {output}");
    assert!(output.contains("Paired with dev-in."), "completion: {output}");

    let requests = server.recorded();
    assert!(
        requests.iter().any(|r| r.starts_with("POST /api/v1/devices/dev-in/pair ")),
        "accept POSTed: {requests:?}"
    );
}

#[test]
fn test_pair_incoming_request_without_yes_refuses_when_not_a_tty() {
    // Test stdin is not a terminal, so the no-flag path must refuse rather
    // than accept blind.
    let server = MockServer::start(|request_line, _| {
        if request_line.starts_with("GET /api/v1/devices?") {
            (
                200,
                envelope(serde_json::json!({
                    "devices": [{ "id": "dev-in", "name": "test phone" }],
                    "total": 1
                })),
            )
        } else if request_line == "GET /api/v1/devices/dev-in HTTP/1.1" {
            (
                200,
                envelope(serde_json::json!({
                    "id": "dev-in",
                    "state": "connected",
                    "pair_state": "requested_by_peer",
                    "verification_key": "00F8F3CE"
                })),
            )
        } else {
            (
                404,
                r#"{"status":"error","error":{"code":"NOT_FOUND","message":"nope"}}"#.into(),
            )
        }
    });

    let mut out = Vec::new();
    let err = execute(
        &server.client(),
        &Command::Pair {
            device_id: "dev-in".to_string(),
            yes: false,
        },
        false,
        &mut out,
    )
    .expect_err("must refuse to accept blind");
    let msg = format!("{err}");
    assert!(msg.contains("--yes"), "error names the flag: {msg}");

    let requests = server.recorded();
    assert!(
        !requests.iter().any(|r| r.contains("/pair ")),
        "no accept must be sent: {requests:?}"
    );
}

#[test]
fn test_share_resolves_prefix_and_streams_file_body() {
    let full_id = "0123456789abcdef0123456789abcdef";
    let temp = tempfile::TempDir::new().expect("tempdir");
    let file_path = temp.path().join("hello world.txt");
    std::fs::write(&file_path, b"file contents").expect("write file");

    let server = MockServer::start(move |request_line, _| {
        if request_line.starts_with("GET /api/v1/devices?") {
            (
                200,
                envelope(serde_json::json!({
                    "devices": [{ "id": full_id, "name": "Pixel 8" }],
                    "total": 1
                })),
            )
        } else if request_line
            .starts_with("POST /api/v1/devices/0123456789abcdef0123456789abcdef/share/send")
        {
            (
                200,
                envelope(serde_json::json!({ "size": 13, "sent": true })),
            )
        } else {
            (
                404,
                r#"{"status":"error","error":{"code":"NOT_FOUND","message":"nope"}}"#.into(),
            )
        }
    });

    let mut out = Vec::new();
    execute(
        &server.client(),
        &Command::Share {
            device_id: "01234567".to_string(),
            file: file_path,
        },
        false,
        &mut out,
    )
    .expect("share succeeds");
    let output = String::from_utf8(out).expect("utf8");

    let requests = server.recorded();
    assert_eq!(requests.len(), 2, "{requests:?}");
    let request = &requests[1];
    assert!(
        request.starts_with(
            "POST /api/v1/devices/0123456789abcdef0123456789abcdef/share/send?filename=hello%20world.txt HTTP/1.1"
        ),
        "prefix resolved to the full id, filename encoded: {request}"
    );
    let body = request.split_once("\r\n\r\n").expect("headers").1;
    assert_eq!(body, "file contents", "streamed file body: {request}");
    assert!(
        output.contains("Sent hello world.txt (13 bytes) to 0123456789abcdef0123456789abcdef."),
        "{output}"
    );
}

#[test]
fn test_share_rejects_unreadable_file_before_any_request() {
    let server = MockServer::start(|_, _| (200, envelope(serde_json::json!({}))));

    let mut out = Vec::new();
    let err = execute(
        &server.client(),
        &Command::Share {
            device_id: "any".to_string(),
            file: std::path::PathBuf::from("/nonexistent/definitely-missing.bin"),
        },
        false,
        &mut out,
    )
    .expect_err("missing file must fail");
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().contains("cannot read"), "{err}");
}

#[test]
fn test_clipboard_set_sends_json_body() {
    let server = MockServer::start(|_, _| {
        (
            200,
            envelope(serde_json::json!({ "sent": true, "devices": 2, "failed": 0 })),
        )
    });

    let mut out = Vec::new();
    execute(
        &server.client(),
        &Command::Clipboard {
            action: Some(ClipboardAction::Set {
                text: "hello clipboard".to_string(),
            }),
        },
        false,
        &mut out,
    )
    .expect("clipboard set succeeds");
    let output = String::from_utf8(out).expect("utf8");

    let requests = server.recorded();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request.starts_with("POST /api/v1/clipboard HTTP/1.1"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{request}"
    );
    assert!(
        request.contains(r#"{"content":"hello clipboard"}"#),
        "JSON body: {request}"
    );
    assert!(
        output.contains("Clipboard sent to 2 device(s)."),
        "{output}"
    );
}

#[test]
fn test_api_error_maps_to_exit_1() {
    let server = MockServer::start(|_, _| {
        (
            404,
            r#"{"status":"error","error":{"code":"DEVICE_NOT_FOUND","message":"Device 'ghost' not found"},"metadata":{}}"#
                .to_string(),
        )
    });

    let mut out = Vec::new();
    let err = execute(&server.client(), &Command::Devices, false, &mut out)
        .expect_err("404 envelope must fail");
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string()
            .contains("DEVICE_NOT_FOUND: Device 'ghost' not found"),
        "{err}"
    );
}

#[test]
fn test_unauthorized_maps_to_exit_1_with_message() {
    let server = MockServer::start(|_, _| {
        (
            401,
            r#"{"status":"error","error":{"code":"UNAUTHORIZED","message":"Invalid API key"},"metadata":{}}"#
                .to_string(),
        )
    });

    let mut out = Vec::new();
    let err = execute(&server.client(), &Command::Devices, false, &mut out)
        .expect_err("401 envelope must fail");
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string().contains("UNAUTHORIZED: Invalid API key"),
        "{err}"
    );
}

#[test]
fn test_unreachable_maps_to_exit_2_with_hint() {
    // Bind then drop: the port is closed, connections are refused.
    let addr = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr");
    let client = ApiClient::new(ClientConfig {
        base_url: format!("http://{addr}"),
        api_key: "test-key".to_string(),
    });

    let mut out = Vec::new();
    let err =
        execute(&client, &Command::Status, false, &mut out).expect_err("closed port must fail");
    assert!(matches!(err, CliError::Unreachable(_)), "{err:?}");
    assert_eq!(err.exit_code(), 2);
    assert!(
        err.to_string()
            .contains("systemctl --user status rust-connect"),
        "{err}"
    );
}

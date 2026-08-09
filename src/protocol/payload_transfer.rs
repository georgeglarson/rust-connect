//! Payload transfer for file sharing
//!
//! Single Responsibility: Handle binary payload transfers over separate
//! TLS connections on dynamic ports (1739-1764), as specified by the
//! KDE Connect protocol.
//!
//! The sender opens a TCP listener on a random port in the transfer range,
//! advertises the port in `payloadTransferInfo`, and the receiver connects
//! to receive the binary payload.
//!
//! Payloads are TLS, not plaintext (Android `LanLink.java`: the accepted
//! socket is wrapped with `convertToSslSocket` in server mode against the
//! trusted-device context). The sender/listener is the TLS SERVER and the
//! receiver is the TLS client, with the same TOFU fingerprint pinning as
//! the main link. The receiver reads exactly `payloadSize` bytes — never
//! to EOF — so a malicious or buggy sender cannot fill the disk.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info};

use crate::protocol::connection::tls;
use crate::protocol::crypto::CertificateManager;
use crate::utils::errors::{Error, Result};

const TRANSFER_MIN_PORT: u16 = 1739;
const TRANSFER_MAX_PORT: u16 = 1764;
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long the sender waits for the receiver to connect to the
/// advertised transfer port before giving up. Aligned to kde's
/// `CompositeUploadJob` timer (`compositeuploadjob.cpp:35-37` —
/// `m_timeout.setInterval(30000)`; `:231-242` `timeoutTriggered()` closes
/// the listening port and fails the job) rather than android's 10s
/// (`LanLink.java#200`): kde is the desktop reference, and this is a
/// desktop-peer daemon like it. The prior 300s left a wedged sender
/// holding a transfer slot 10x longer than either reference (parity-
/// checklist.md § Payload transfers, gap 2).
const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const IO_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Floor on the TOTAL transfer deadline. Small payloads get at least this
/// long no matter what the rate calculation says.
const IO_TOTAL_TIMEOUT_FLOOR: std::time::Duration = std::time::Duration::from_secs(3600);
/// Slowest end-to-end throughput a transfer is expected to sustain: at least
/// 64 KiB/s. The total deadline scales at this rate so a large payload over a
/// slow link is not killed while still making progress. `IO_IDLE_TIMEOUT`
/// remains the stall detector; this bound only applies to a transfer that
/// keeps moving.
const TRANSFER_FLOOR_RATE_BYTES_PER_SEC: u64 = 64 * 1024;

/// Total deadline for a payload of `payload_size` bytes.
fn total_deadline_for(payload_size: u64) -> std::time::Duration {
    let scaled = std::time::Duration::from_secs(payload_size / TRANSFER_FLOOR_RATE_BYTES_PER_SEC);
    IO_TOTAL_TIMEOUT_FLOOR.max(scaled)
}

async fn copy_with_idle_timeout<R, W>(
    reader: &mut R,
    writer: &mut W,
    operation: &str,
    idle_timeout: std::time::Duration,
    total_timeout: std::time::Duration,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 32 * 1024];
    let mut copied = 0_u64;
    let deadline = tokio::time::Instant::now() + total_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::ConnectionTimeout(format!(
                "Payload {operation} exceeded the total transfer deadline"
            )));
        }
        let operation_timeout = idle_timeout.min(remaining);
        let read = tokio::time::timeout(operation_timeout, reader.read(&mut buffer))
            .await
            .map_err(|_| {
                Error::ConnectionTimeout(format!("Timed out waiting for payload {operation}"))
            })?
            .map_err(|e| {
                Error::io(
                    format!("Failed to read payload during {operation}: {e}"),
                    None,
                )
            })?;
        if read == 0 {
            return Ok(copied);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::ConnectionTimeout(format!(
                "Payload {operation} exceeded the total transfer deadline"
            )));
        }
        let operation_timeout = idle_timeout.min(remaining);
        tokio::time::timeout(operation_timeout, writer.write_all(&buffer[..read]))
            .await
            .map_err(|_| {
                Error::ConnectionTimeout(format!("Timed out writing payload during {operation}"))
            })?
            .map_err(|e| {
                Error::io(
                    format!("Failed to write payload during {operation}: {e}"),
                    None,
                )
            })?;
        copied += read as u64;
    }
}

async fn create_destination(path: &std::path::Path) -> Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| {
            Error::io(
                format!("Failed to create destination file: {}", e),
                Some(path.display().to_string()),
            )
        })
}

fn collision_candidate(path: &std::path::Path, copy: usize) -> std::path::PathBuf {
    if copy == 0 {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("shared_file");
    let extension = path.extension().and_then(|value| value.to_str());
    let filename = match extension {
        Some(extension) => format!("{stem} ({copy}).{extension}"),
        None => format!("{stem} ({copy})"),
    };
    path.with_file_name(filename)
}

/// Create `desired_path` exclusively, falling back to `name (1)`, `name (2)`
/// … on collision. Shared with the share plugin's text path, which needs the
/// same never-clobber guarantee for a file it writes directly rather than
/// receiving over a payload socket.
pub(crate) async fn create_unique_destination(
    desired_path: &std::path::Path,
) -> Result<(tokio::fs::File, std::path::PathBuf)> {
    for copy in 0..10_000 {
        let candidate = collision_candidate(desired_path, copy);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(Error::io(
                    format!("Failed to create destination file: {error}"),
                    Some(candidate.display().to_string()),
                ));
            }
        }
    }
    Err(Error::io(
        "Failed to choose a unique destination filename".to_string(),
        Some(desired_path.display().to_string()),
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadTransferInfo {
    /// Sender's address. Optional on the wire: Android may send a
    /// `{port}`-only `payloadTransferInfo`, in which case the receiver falls
    /// back to the peer address of the existing link (the sender is
    /// necessarily the connected peer).
    #[serde(default)]
    pub ip: Option<String>,
    pub port: u16,
    #[serde(rename = "availableStreams", default)]
    pub available_streams: usize,
    #[serde(rename = "totalStreams", default)]
    pub total_streams: usize,
}

pub struct PayloadTransfer {
    cert_manager: std::sync::Arc<CertificateManager>,
    device_id: String,
}

impl PayloadTransfer {
    pub fn new(cert_manager: std::sync::Arc<CertificateManager>, device_id: String) -> Self {
        Self {
            cert_manager,
            device_id,
        }
    }

    async fn connect_receiver(
        &self,
        transfer_info: &PayloadTransferInfo,
    ) -> Result<tls::TlsStream> {
        let ip = transfer_info.ip.as_deref().ok_or_else(|| {
            Error::ConnectionError(
                "Payload transfer info has no address (no ip, no peer fallback)".to_string(),
            )
        })?;
        // Parse the ip and port SEPARATELY and build the SocketAddr: a bare
        // IPv6 literal ("::1") is valid `ip` text (the link peer-address
        // fallback produces exactly that) but "ip:port" string interpolation
        // is not — "[addr]:port" bracketing is what SocketAddr::new adds.
        let ip: std::net::IpAddr = ip
            .parse()
            .map_err(|e| Error::ConnectionError(format!("Invalid transfer address: {}", e)))?;
        let addr = SocketAddr::new(ip, transfer_info.port);

        let tcp_stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                Error::ConnectionTimeout("Timed out connecting to payload transfer".to_string())
            })?
            .map_err(|e| {
                Error::ConnectionError(format!("Failed to connect to payload transfer: {}", e))
            })?;

        // TLS client role, same TOFU pin as the main link (the sender is a
        // paired device whose fingerprint is on file).
        let (tls_stream, _peer_cert) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tls::tls_connect(self.cert_manager.clone(), &self.device_id, tcp_stream),
        )
        .await
        .map_err(|_| {
            Error::ConnectionTimeout("Timed out establishing payload TLS".to_string())
        })??;
        Ok(tls_stream)
    }

    async fn receive_into(
        tls_stream: tls::TlsStream,
        mut file: tokio::fs::File,
        payload_size: u64,
        dest_path: &std::path::Path,
    ) -> Result<u64> {
        // Read EXACTLY payload_size bytes. `io::copy` to EOF would let a
        // sender write unboundedly to disk.
        let mut limited = tls_stream.take(payload_size);
        let bytes_written = match copy_with_idle_timeout(
            &mut limited,
            &mut file,
            "receive",
            IO_IDLE_TIMEOUT,
            total_deadline_for(payload_size),
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                drop(file);
                let _ = tokio::fs::remove_file(dest_path).await;
                return Err(e);
            }
        };

        if bytes_written < payload_size {
            drop(file);
            let _ = tokio::fs::remove_file(dest_path).await;
            return Err(Error::ConnectionError(format!(
                "Payload truncated: expected {} bytes, got {} — partial file deleted",
                payload_size, bytes_written
            )));
        }

        if let Err(error) = file.flush().await {
            drop(file);
            let _ = tokio::fs::remove_file(dest_path).await;
            return Err(Error::io(
                format!("Failed to flush received payload: {error}"),
                Some(dest_path.display().to_string()),
            ));
        }

        info!(
            bytes = bytes_written,
            path = %dest_path.display(),
            event = "payload_received",
            "Received payload file over TLS"
        );

        Ok(bytes_written)
    }

    /// Receive a payload at one exact path, refusing any existing entry.
    pub async fn receive_file(
        &self,
        transfer_info: &PayloadTransferInfo,
        payload_size: u64,
        dest_path: &std::path::Path,
    ) -> Result<u64> {
        let file = create_destination(dest_path).await?;
        let tls_stream = match self.connect_receiver(transfer_info).await {
            Ok(stream) => stream,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(dest_path).await;
                return Err(error);
            }
        };
        Self::receive_into(tls_stream, file, payload_size, dest_path).await
    }

    /// Receive a user-facing download without overwriting. Filename
    /// collisions are resolved atomically as `name (N).ext`; symlinks are
    /// existing entries and therefore skipped rather than followed.
    pub async fn receive_file_unique(
        &self,
        transfer_info: &PayloadTransferInfo,
        payload_size: u64,
        desired_path: &std::path::Path,
    ) -> Result<(u64, std::path::PathBuf)> {
        let (file, destination) = create_unique_destination(desired_path).await?;
        let tls_stream = match self.connect_receiver(transfer_info).await {
            Ok(stream) => stream,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&destination).await;
                return Err(error);
            }
        };
        let bytes = Self::receive_into(tls_stream, file, payload_size, &destination).await?;
        Ok((bytes, destination))
    }

    pub async fn send_file(
        &self,
        file_path: &std::path::Path,
    ) -> Result<(PayloadTransferInfo, tokio::task::JoinHandle<Result<()>>)> {
        // The declared size sets the total transfer deadline; a big file over
        // a slow link must not die at the flat floor while still progressing.
        let file_size = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| Error::io(format!("Cannot read file metadata: {}", e), None))?
            .len();

        let listener = self.find_free_port().await?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| Error::ConnectionError(format!("Cannot get listener address: {}", e)))?;

        let transfer_info = PayloadTransferInfo {
            ip: Some(self.local_ip().await?),
            port: local_addr.port(),
            available_streams: 1,
            total_streams: 1,
        };

        let file_path = file_path.to_path_buf();
        let cert_manager = self.cert_manager.clone();
        let device_id = self.device_id.clone();
        let handle = tokio::spawn(async move {
            match tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()).await {
                Ok(Ok((tcp_stream, _addr))) => {
                    debug!(
                        event = "payload_receiver_connected",
                        "Payload receiver connected"
                    );
                    // TLS server role (LanLink.java convertToSslSocket server
                    // mode): the receiver is a paired device and must present
                    // its cert.
                    let (mut stream, _peer_cert) = tokio::time::timeout(
                        CONNECT_TIMEOUT,
                        tls::tls_accept(cert_manager, Some(&device_id), tcp_stream),
                    )
                    .await
                    .map_err(|_| {
                        Error::ConnectionTimeout("Timed out establishing payload TLS".to_string())
                    })??;
                    let mut file = tokio::fs::File::open(&file_path).await.map_err(|e| {
                        Error::io(format!("Cannot open file for transfer: {}", e), None)
                    })?;
                    let bytes_sent = copy_with_idle_timeout(
                        &mut file,
                        &mut stream,
                        "send",
                        IO_IDLE_TIMEOUT,
                        total_deadline_for(file_size),
                    )
                    .await?;
                    tokio::time::timeout(CONNECT_TIMEOUT, stream.flush())
                        .await
                        .map_err(|_| {
                            Error::ConnectionTimeout("Timed out flushing payload".to_string())
                        })?
                        .map_err(|e| Error::io(format!("Failed to flush payload: {}", e), None))?;
                    info!(
                        bytes = bytes_sent,
                        event = "payload_sent",
                        "Sent payload file over TLS"
                    );
                    Ok(())
                }
                Ok(Err(e)) => Err(Error::ConnectionError(format!(
                    "Failed to accept payload connection: {}",
                    e
                ))),
                Err(_) => Err(Error::ConnectionTimeout(
                    "Timed out waiting for payload receiver".to_string(),
                )),
            }
        });

        Ok((transfer_info, handle))
    }

    async fn find_free_port(&self) -> Result<TcpListener> {
        for port in TRANSFER_MIN_PORT..=TRANSFER_MAX_PORT {
            match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
                Ok(listener) => {
                    debug!(
                        port = port,
                        event = "transfer_port_bound",
                        "Bound transfer port"
                    );
                    return Ok(listener);
                }
                Err(_) => continue,
            }
        }
        Err(Error::ConnectionError(
            "No free ports in transfer range 1739-1764".to_string(),
        ))
    }

    async fn local_ip(&self) -> Result<String> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| Error::ConnectionError(format!("Failed to bind socket: {}", e)))?;
        socket
            .connect("8.8.8.8:80")
            .map_err(|e| Error::ConnectionError(format!("Failed to connect socket: {}", e)))?;
        let local_addr = socket
            .local_addr()
            .map_err(|e| Error::ConnectionError(format!("Failed to get local address: {}", e)))?;
        Ok(local_addr.ip().to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn setup() -> (PayloadTransfer, TempDir) {
        let temp_dir = TempDir::new().expect("Value expected to be present");
        let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
        cert_manager.init().expect("Value expected to be present");
        cert_manager
            .ensure_certificate("test-deviceaaaaaaaaaaaaaaaaaaaaa", "Test")
            .expect("Value expected to be present");
        let transfer =
            PayloadTransfer::new(cert_manager, "test-deviceaaaaaaaaaaaaaaaaaaaaa".to_string());
        (transfer, temp_dir)
    }

    #[test]
    fn test_payload_transfer_info_serialization() {
        let info = PayloadTransferInfo {
            ip: Some("192.168.1.1".to_string()),
            port: 1740,
            available_streams: 1,
            total_streams: 1,
        };
        let json = serde_json::to_string(&info).expect("Serialization of known types cannot fail");
        assert!(json.contains("192.168.1.1"));
        assert!(json.contains("1740"));
        assert!(json.contains("availableStreams"));
        assert!(json.contains("totalStreams"));
    }

    #[test]
    fn test_payload_transfer_info_deserialization() {
        let json = r#"{"ip":"10.0.0.1","port":1750,"availableStreams":1,"totalStreams":1}"#;
        let info: PayloadTransferInfo =
            serde_json::from_str(json).expect("Value expected to be present");
        assert_eq!(info.ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(info.port, 1750);
        assert_eq!(info.available_streams, 1);
        assert_eq!(info.total_streams, 1);
    }

    /// F-2: Android may send a `{port}`-only payloadTransferInfo; it must
    /// parse (ip resolved later from the link's peer address). `port` stays
    /// required.
    #[test]
    fn test_payload_transfer_info_port_only_deserialization() {
        let json = r#"{"port":1740}"#;
        let info: PayloadTransferInfo =
            serde_json::from_str(json).expect("port-only payloadTransferInfo must parse");
        assert_eq!(info.ip, None);
        assert_eq!(info.port, 1740);
        assert_eq!(info.available_streams, 0);
        assert_eq!(info.total_streams, 0);

        let no_port = r#"{"ip":"10.0.0.1"}"#;
        assert!(
            serde_json::from_str::<PayloadTransferInfo>(no_port).is_err(),
            "port remains required"
        );
    }

    #[tokio::test]
    async fn test_find_free_port_succeeds() {
        let (transfer, _temp) = setup();
        let listener = transfer.find_free_port().await;
        assert!(listener.is_ok());
        let port = listener
            .expect("Value expected to be present")
            .local_addr()
            .expect("Value expected to be present")
            .port();
        assert!((TRANSFER_MIN_PORT..=TRANSFER_MAX_PORT).contains(&port));
    }

    #[tokio::test]
    async fn test_send_file_returns_transfer_info() {
        let (transfer, temp) = setup();
        let file_path = temp.path().join("test.txt");
        tokio::fs::write(&file_path, b"hello world")
            .await
            .expect("Value expected to be present");

        let result = transfer.send_file(&file_path).await;
        assert!(result.is_ok());
        let (info, handle) = result.expect("Value expected to be present");
        assert_eq!(info.available_streams, 1);
        assert_eq!(info.total_streams, 1);
        assert!(info.port >= TRANSFER_MIN_PORT && info.port <= TRANSFER_MAX_PORT);

        handle.abort();
    }

    /// Gap 2 (parity-checklist.md § Payload transfers): pins the value
    /// itself. kde's CompositeUploadJob timer is 30000ms
    /// (compositeuploadjob.cpp:36); rust-connect matches kde, the desktop
    /// reference, rather than android's 10s.
    #[test]
    fn test_accept_timeout_matches_kde_desktop_reference() {
        assert_eq!(ACCEPT_TIMEOUT, std::time::Duration::from_secs(30));
    }

    /// Gap 2 behavioral: a sender with nobody connecting must time out at
    /// (approximately) the configured bound, not hang forever and not sit
    /// at the old 300s bound. Time-paused so the test doesn't burn 30 real
    /// seconds; tokio's paused-clock auto-advance fast-forwards through
    /// the real TcpListener::accept() (which never resolves — nothing
    /// connects) once nothing else in the runtime can make progress,
    /// exactly the scenario `tokio::time::timeout` around a stalled I/O
    /// future is meant to be tested under.
    #[tokio::test(start_paused = true)]
    async fn test_accept_times_out_at_the_new_bound_not_the_old_one() {
        let (transfer, temp) = setup();
        let file_path = temp.path().join("test.txt");
        tokio::fs::write(&file_path, b"hello")
            .await
            .expect("write test file");

        let (_info, handle) = transfer
            .send_file(&file_path)
            .await
            .expect("send_file must set up the listener");
        // Nobody connects to the advertised port — the spawned task's
        // accept() call must eventually time out on its own.
        let started = tokio::time::Instant::now();
        let result = handle.await.expect("send task must not panic");
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "an accept with nobody connecting must time out, not hang forever"
        );
        // A generous margin over the 30s bound, but nowhere near the old
        // 300s one — this is what actually distinguishes "fixed" from
        // "still 300s" under a paused clock, where a wrong-but-internally-
        // consistent bound would otherwise pass trivially.
        assert!(
            elapsed <= std::time::Duration::from_secs(35),
            "accept must time out within ~30s (kde compositeuploadjob.cpp:36), \
             not the old 300s bound; elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_send_file_nonexistent_fails() {
        let (transfer, _temp) = setup();
        let result = transfer
            .send_file(std::path::Path::new("/nonexistent/file.txt"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_receive_file_invalid_address_fails() {
        let (transfer, _temp) = setup();
        let info = PayloadTransferInfo {
            ip: Some("not-an-ip".to_string()),
            port: 1740,
            available_streams: 1,
            total_streams: 1,
        };
        let dest = std::path::Path::new("/tmp/test_recv.txt");
        let result = transfer.receive_file(&info, 1024, dest).await;
        assert!(result.is_err());
    }

    /// A bare IPv6 literal in payloadTransferInfo — exactly what the link
    /// peer-address fallback stores (`addr.ip().to_string()`) — must parse.
    /// "ip:port" string interpolation is not valid SocketAddr text for IPv6;
    /// SocketAddr::new adds the brackets.
    #[tokio::test]
    async fn test_receive_file_bare_ipv6_literal_parses() {
        let (transfer, _temp) = setup();
        // Hermetic port: bind then close, so nothing is listening there no
        // matter what other tests are doing.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("Value expected to be present");
        let port = listener
            .local_addr()
            .expect("Value expected to be present")
            .port();
        drop(listener);
        let info = PayloadTransferInfo {
            ip: Some("::1".to_string()),
            port,
            available_streams: 1,
            total_streams: 1,
        };
        let dest = std::path::Path::new("/tmp/test_recv_v6.bin");
        let err = transfer
            .receive_file(&info, 1024, dest)
            .await
            .expect_err("nothing useful listens on the port, but the address must parse");
        assert!(
            !err.to_string().contains("Invalid transfer address"),
            "bare IPv6 literal must build a SocketAddr, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_receive_file_unwritable_destination_fails() {
        let (transfer, _temp) = setup();
        let info = PayloadTransferInfo {
            ip: Some("127.0.0.1".to_string()),
            port: 1740,
            available_streams: 1,
            total_streams: 1,
        };
        let dest = std::path::Path::new("/proc/1/nonexistent_file.txt");
        let result = transfer.receive_file(&info, 1024, dest).await;
        assert!(
            result.is_err(),
            "Should fail when destination path is not writable"
        );
    }

    #[tokio::test]
    async fn test_send_file_directory_as_path_sets_up_listener() {
        let (transfer, _temp) = setup();
        let result = transfer.send_file(std::path::Path::new("/tmp")).await;
        assert!(
            result.is_ok(),
            "send_file should succeed in setting up listener even for directory"
        );
        let (info, handle) = result.expect("Value expected to be present");
        assert!(info.port >= TRANSFER_MIN_PORT && info.port <= TRANSFER_MAX_PORT);
        handle.abort();
    }

    #[tokio::test]
    async fn test_destination_creation_refuses_existing_file() {
        let temp = TempDir::new().expect("tempdir");
        let destination = temp.path().join("report.pdf");
        tokio::fs::write(&destination, b"keep me").await.unwrap();

        assert!(create_destination(&destination).await.is_err());
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"keep me");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_destination_creation_refuses_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let target = temp.path().join("target");
        let destination = temp.path().join("shared-name");
        tokio::fs::write(&target, b"keep me").await.unwrap();
        symlink(&target, &destination).unwrap();

        assert!(create_destination(&destination).await.is_err());
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"keep me");
    }

    #[tokio::test]
    async fn test_unique_destination_preserves_existing_file() {
        let temp = TempDir::new().expect("tempdir");
        let desired = temp.path().join("report.pdf");
        tokio::fs::write(&desired, b"original").await.unwrap();

        let (mut file, selected) = create_unique_destination(&desired).await.unwrap();
        file.write_all(b"second").await.unwrap();
        drop(file);

        assert_eq!(selected, temp.path().join("report (1).pdf"));
        assert_eq!(tokio::fs::read(&desired).await.unwrap(), b"original");
        assert_eq!(tokio::fs::read(&selected).await.unwrap(), b"second");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_unique_destination_skips_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let target = temp.path().join("target");
        let desired = temp.path().join("report.pdf");
        tokio::fs::write(&target, b"original").await.unwrap();
        symlink(&target, &desired).unwrap();

        let (mut file, selected) = create_unique_destination(&desired).await.unwrap();
        file.write_all(b"safe copy").await.unwrap();
        drop(file);

        assert_eq!(selected, temp.path().join("report (1).pdf"));
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"original");
        assert_eq!(tokio::fs::read(&selected).await.unwrap(), b"safe copy");
    }

    #[tokio::test]
    async fn test_copy_timeout_resets_when_progress_is_made() {
        let (mut sender, mut reader) = tokio::io::duplex(64);
        let producer = tokio::spawn(async move {
            for chunk in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                sender.write_all(chunk).await.unwrap();
            }
        });
        let mut output = Vec::new();

        let copied = copy_with_idle_timeout(
            &mut reader,
            &mut output,
            "test",
            std::time::Duration::from_millis(60),
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("continuous progress must outlive the single idle window");
        producer.await.unwrap();

        assert_eq!(copied, 11);
        assert_eq!(output, b"onetwothree");
    }

    #[tokio::test]
    async fn test_copy_total_deadline_stops_slow_drip_peer() {
        let (mut sender, mut reader) = tokio::io::duplex(64);
        let producer = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if sender.write_all(b"x").await.is_err() {
                    break;
                }
            }
        });
        let mut output = Vec::new();

        let result = copy_with_idle_timeout(
            &mut reader,
            &mut output,
            "test",
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(90),
        )
        .await;
        producer.abort();

        assert!(result.is_err(), "continuous trickle must not run forever");
        assert!(!output.is_empty(), "the peer made progress before the cap");
    }

    #[test]
    fn test_total_deadline_floors_at_one_hour() {
        assert_eq!(total_deadline_for(0), std::time::Duration::from_secs(3600));
        assert_eq!(
            total_deadline_for(1024 * 1024),
            std::time::Duration::from_secs(3600),
            "a 1 MiB payload gets the floor, not the 16 seconds the rate implies"
        );
    }

    #[test]
    fn test_total_deadline_scales_with_large_payload() {
        // 1 GiB / 64 KiB per second = 16384 seconds.
        let one_gib: u64 = 1024 * 1024 * 1024;
        assert_eq!(
            total_deadline_for(one_gib),
            std::time::Duration::from_secs(16_384)
        );
        assert!(
            total_deadline_for(one_gib) > std::time::Duration::from_secs(3600),
            "a large payload must outlive the flat one-hour cap"
        );
    }
}

#[cfg(test)]
mod tls_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! P5 conformance: payloads travel over mutually-authenticated TLS with
    //! TOFU pinning, and the receiver reads exactly payloadSize bytes.

    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    const SENDER_ID: &str = "sender-device-aaaaaaaaaaaaaaaaaaaaa";
    const RECEIVER_ID: &str = "receiver-device-aaaaaaaaaaaaaaaaaaa";

    /// Two cert managers that trust each other like paired devices do.
    fn paired_pair() -> (
        Arc<CertificateManager>,
        Arc<CertificateManager>,
        TempDir,
        TempDir,
    ) {
        let t_a = TempDir::new().expect("tempdir");
        let t_b = TempDir::new().expect("tempdir");
        let cm_a = Arc::new(CertificateManager::new(t_a.path().to_path_buf()));
        let cm_b = Arc::new(CertificateManager::new(t_b.path().to_path_buf()));
        cm_a.init().expect("init");
        cm_b.init().expect("init");
        cm_a.ensure_own_certificate(SENDER_ID, "Sender")
            .expect("own");
        cm_b.ensure_own_certificate(RECEIVER_ID, "Receiver")
            .expect("own");

        // Cross-store peer certs (TOFU, as after pairing).
        let (a_pem, _) = cm_a.load_own_certificate().expect("load");
        let (b_pem, _) = cm_b.load_own_certificate().expect("load");
        let a_der = openssl::x509::X509::from_pem(&a_pem)
            .expect("pem")
            .to_der()
            .expect("der");
        let b_der = openssl::x509::X509::from_pem(&b_pem)
            .expect("pem")
            .to_der()
            .expect("der");
        cm_a.store_peer_certificate(RECEIVER_ID, &b_der)
            .expect("store");
        cm_b.store_peer_certificate(SENDER_ID, &a_der)
            .expect("store");

        (cm_a, cm_b, t_a, t_b)
    }

    async fn roundtrip(content: &[u8], declared_size: u64) -> (Result<u64>, Vec<u8>, PathBuf) {
        let (cm_a, cm_b, t_a, t_b) = paired_pair();

        let src = t_a.path().join("src.bin");
        tokio::fs::write(&src, content).await.expect("write src");

        let sender = PayloadTransfer::new(cm_a, RECEIVER_ID.to_string());
        let (mut info, send_handle) = sender.send_file(&src).await.expect("send_file");
        info.ip = Some("127.0.0.1".to_string()); // loopback for the test

        let dest = t_b.path().join("dest.bin");
        let receiver = PayloadTransfer::new(cm_b, SENDER_ID.to_string());
        let result = receiver.receive_file(&info, declared_size, &dest).await;

        let send_result = tokio::time::timeout(std::time::Duration::from_secs(5), send_handle)
            .await
            .expect("send task join");
        // A truncated read closes early; the sender's copy may then fail —
        // that's fine and not what we're asserting here.
        let _ = send_result;

        let got = tokio::fs::read(&dest).await.unwrap_or_default();
        (result, got, dest)
    }

    #[tokio::test]
    async fn test_tls_payload_roundtrip_exact_bytes() {
        let content: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let (result, got, _dest) = roundtrip(&content, content.len() as u64).await;
        assert_eq!(result.expect("receive"), content.len() as u64);
        assert_eq!(got, content, "TLS payload must arrive byte-identical");
    }

    #[tokio::test]
    async fn test_receive_reads_exactly_payload_size_not_eof() {
        // Sender transmits MORE than the declared payloadSize; the receiver
        // must stop at the declared size (disk-fill DoS fix).
        let content = vec![0xABu8; 4096];
        let (result, got, _dest) = roundtrip(&content, 1024).await;
        assert_eq!(result.expect("receive"), 1024);
        assert_eq!(got.len(), 1024, "must read exactly payloadSize bytes");
        assert!(got.iter().all(|&b| b == 0xAB));
    }

    #[tokio::test]
    async fn test_truncated_payload_errors_and_deletes_partial() {
        // Sender transmits FEWER bytes than declared; the receive must fail
        // and the partial file must be removed.
        let content = vec![0xCDu8; 512];
        let (result, _got, dest) = roundtrip(&content, 2048).await;
        assert!(result.is_err(), "truncated payload must error");
        assert!(
            !dest.exists(),
            "partial file must be deleted after truncation"
        );
    }
}

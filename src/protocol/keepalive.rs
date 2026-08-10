//! TCP keepalive configuration
//!
//! Single Responsibility: Configure TCP keepalive on streams.

use socket2::SockRef;
use std::time::Duration;
use tokio::net::TcpStream;

/// TCP_KEEPIDLE (Linux): seconds of idle time before the first keepalive
/// probe. Named so callers deriving a bounded "dead link must surface
/// within N seconds" deadline (the fault suite, `tests/fault_suite.rs`)
/// reference the actual production value instead of re-hardcoding it — a
/// future tuning change here must widen/narrow those deadlines with it,
/// never silently invalidate them.
pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
/// TCP_KEEPINTVL (Linux): seconds between probes once idle.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// TCP_KEEPCNT (Linux): probes sent before the kernel gives up on the peer.
pub const KEEPALIVE_RETRIES: u32 = 3;
/// TCP_USER_TIMEOUT (Linux): bounds how long unacknowledged data may sit
/// before the kernel fails the socket outright (kdeconnectd BUG 476747).
pub const TCP_USER_TIMEOUT: Duration = Duration::from_secs(30);

/// Configures TCP keepalive on a stream.
/// On Linux: 30s idle, 10s interval, 3 retries.
/// On other platforms: OS defaults.
pub fn configure_keepalive(stream: &TcpStream) {
    let sock_ref = SockRef::from(stream);
    let _ = sock_ref.set_keepalive(true);
    #[cfg(target_os = "linux")]
    {
        let _ = sock_ref.set_tcp_keepalive(
            &socket2::TcpKeepalive::new()
                .with_time(KEEPALIVE_IDLE)
                .with_interval(KEEPALIVE_INTERVAL)
                .with_retries(KEEPALIVE_RETRIES),
        );
        let _ = sock_ref.set_tcp_user_timeout(Some(TCP_USER_TIMEOUT));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sock_ref.set_keepalive(true);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    /// Assert via the socket that configure_keepalive actually applies the
    /// options — this is the test that would have caught "the module existed
    /// but was never called on the inbound path" being silently re-broken by
    /// a refactor of the function itself.
    #[tokio::test]
    async fn test_configure_keepalive_sets_socket_options() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");
        let stream = TcpStream::connect(addr)
            .await
            .expect("Value expected to be present");

        configure_keepalive(&stream);

        let sock_ref = SockRef::from(&stream);
        assert!(
            sock_ref.keepalive().expect("Value expected to be present"),
            "SO_KEEPALIVE must be on"
        );
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                sock_ref
                    .tcp_keepalive_time()
                    .expect("Value expected to be present"),
                KEEPALIVE_IDLE,
                "TCP_KEEPIDLE must be 30s (kdeconnectd BUG 476747)"
            );
            assert_eq!(
                sock_ref
                    .tcp_keepalive_interval()
                    .expect("Value expected to be present"),
                KEEPALIVE_INTERVAL,
                "TCP_KEEPINTVL must be 10s"
            );
            assert_eq!(
                sock_ref
                    .tcp_keepalive_retries()
                    .expect("Value expected to be present"),
                KEEPALIVE_RETRIES,
                "TCP_KEEPCNT must be 3"
            );
            assert_eq!(
                sock_ref
                    .tcp_user_timeout()
                    .expect("Value expected to be present"),
                Some(TCP_USER_TIMEOUT),
                "TCP_USER_TIMEOUT must bound unacknowledged keepalive probes"
            );
        }
    }
}

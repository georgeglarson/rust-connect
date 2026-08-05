//! TCP keepalive configuration
//!
//! Single Responsibility: Configure TCP keepalive on streams.

use socket2::SockRef;
use std::time::Duration;
use tokio::net::TcpStream;

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
                .with_time(Duration::from_secs(30))
                .with_interval(Duration::from_secs(10))
                .with_retries(3),
        );
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
                Duration::from_secs(30),
                "TCP_KEEPIDLE must be 30s (kdeconnectd BUG 476747)"
            );
            assert_eq!(
                sock_ref
                    .tcp_keepalive_interval()
                    .expect("Value expected to be present"),
                Duration::from_secs(10),
                "TCP_KEEPINTVL must be 10s"
            );
            assert_eq!(
                sock_ref
                    .tcp_keepalive_retries()
                    .expect("Value expected to be present"),
                3,
                "TCP_KEEPCNT must be 3"
            );
        }
    }
}

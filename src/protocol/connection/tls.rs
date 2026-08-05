//! rustls-based TLS for the KDE Connect link.
//!
//! Conformance target: Android's `SslHelper.kt` (docs/reference/SslHelper.kt).
//!
//! - TLSv1.2 pinned both roles (SslHelper.kt:166 — "1.3 seems to cause issues
//!   in some (older?) devices").
//! - Mutual TLS: the TLS server always REQUESTS the client cert; it is
//!   mandatory only when the peer is trusted (SslHelper.kt:181-185,
//!   needClientAuth vs wantClientAuth). native-tls could not request a client
//!   cert at all — this is the capability the migration exists for.
//! - TOFU hooks live in the custom verifiers: a stored peer fingerprint is a
//!   hard pin checked DURING the handshake (mismatch fails the handshake, not
//!   the post-handshake audit). No stored fingerprint means the peer is
//!   pre-pairing — the cert is accepted here and captured/stored by the
//!   caller after the identity exchange.
//!
//! Interop with Conscrypt in both roles was proven live against the stock
//! KDE Connect Android app on 2026-07-29 by examples/tls_probe.rs; the
//! verifier structure below is the probe's, with accept-all swapped for the
//! TOFU policy.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, error, trace};

use crate::protocol::crypto::CertificateManager;
use crate::utils::errors::{Error, Result};

/// Unified stream type: rustls client and server streams are distinct types,
/// so the enum wraps both. Implements AsyncRead + AsyncWrite.
pub(crate) type TlsStream = tokio_rustls::TlsStream<TcpStream>;

/// Placeholder SNI for the client role. KDE Connect certs are self-signed
/// with CN=deviceId and the peer is authenticated by TOFU fingerprint, not by
/// name — our verifier ignores the server name entirely.
const TLS_SERVER_NAME: &str = "kdeconnect";

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn to_rustls_cert_error(e: Error) -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::Other(rustls::OtherError(
        Arc::new(e),
    )))
}

/// Load the daemon's own identity (fixed own.crt/own.key) as rustls types.
fn load_own_identity(
    cert_manager: &CertificateManager,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let (cert_pem, key_pem) = cert_manager.load_own_certificate()?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::TlsError(format!("Failed to parse own certificate PEM: {}", e)))?;
    if certs.is_empty() {
        return Err(Error::TlsError(
            "Own certificate file contains no certificates".to_string(),
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| Error::TlsError(format!("Failed to parse own key PEM: {}", e)))?;
    Ok((certs, key))
}

/// Client-role verifier (we are the TLS client, verifying the peer's server
/// cert). TOFU: stored fingerprint → hard pin; none → accept and let the
/// caller capture the cert for first-use storage.
#[derive(Debug)]
struct TofuServerCertVerifier {
    cert_manager: Arc<CertificateManager>,
    device_id: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for TofuServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if self.cert_manager.has_peer_fingerprint(&self.device_id) {
            self.cert_manager
                .verify_peer_fingerprint(&self.device_id, end_entity.as_ref())
                .map_err(to_rustls_cert_error)?;
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Server-role verifier (we are the TLS server, verifying the peer's client
/// cert). Mirrors SslHelper: always offer client auth; mandatory only when
/// the peer is trusted (has a stored fingerprint). When the device id is not
/// yet known (outbound path: we learn it from the encrypted identity after
/// the handshake), client auth is requested-but-optional and the fingerprint
/// check happens post-handshake in the caller.
#[derive(Debug)]
struct TofuClientCertVerifier {
    cert_manager: Arc<CertificateManager>,
    device_id: Option<String>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ClientCertVerifier for TofuClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // SslHelper.kt:181-185 — needClientAuth when trusted, wantClientAuth
        // otherwise.
        self.device_id
            .as_deref()
            .is_some_and(|id| self.cert_manager.has_peer_fingerprint(id))
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        if let Some(id) = &self.device_id {
            if self.cert_manager.has_peer_fingerprint(id) {
                self.cert_manager
                    .verify_peer_fingerprint(id, end_entity.as_ref())
                    .map_err(to_rustls_cert_error)?;
            }
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Connect as TLS client over an established TCP stream (the peer accepted
/// our TCP connection and is the TLS server). Presents our own certificate
/// when one exists, per KDE Connect mutual TLS.
///
/// Returns the TLS stream and the peer's certificate DER (if presented).
pub async fn tls_connect(
    cert_manager: Arc<CertificateManager>,
    device_id: &str,
    tcp_stream: TcpStream,
) -> Result<(TlsStream, Option<Vec<u8>>)> {
    trace!(device_id = %device_id, event = "tls_connect_start", "Starting tls_connect (rustls client role)");

    let provider = crypto_provider();
    let verifier = Arc::new(TofuServerCertVerifier {
        cert_manager: cert_manager.clone(),
        device_id: device_id.to_string(),
        provider: provider.clone(),
    });

    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .map_err(|e| Error::TlsError(format!("Failed to build TLS client config: {}", e)))?
        .dangerous()
        .with_custom_certificate_verifier(verifier);

    let config = if cert_manager.has_own_certificate() {
        let (certs, key) = load_own_identity(&cert_manager)?;
        debug!(device_id = %device_id, "Configuring client certificate for TLS handshake");
        builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| Error::TlsError(format!("Failed to configure client auth: {}", e)))?
    } else {
        debug!("No client certificate available, proceeding without client auth");
        builder.with_no_client_auth()
    };

    let connector = TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from(TLS_SERVER_NAME)
        .expect("TLS_SERVER_NAME is a valid DNS name")
        .to_owned();

    let tls_stream = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        connector.connect(server_name, tcp_stream),
    )
    .await
    {
        Err(_) => {
            error!(device_id = %device_id, event = "tls_handshake_timeout", "TLS handshake timed out");
            return Err(Error::ConnectionTimeout(format!(
                "TLS handshake timed out for {}",
                device_id
            )));
        }
        Ok(Err(e)) => {
            error!(device_id = %device_id, error = %e, event = "tls_handshake_error", "TLS handshake failed");
            return Err(Error::TlsError(format!(
                "TLS handshake failed for {}: {}",
                device_id, e
            )));
        }
        Ok(Ok(stream)) => stream,
    };

    let peer_cert_der = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|c| c.as_ref().to_vec());

    debug!(
        device_id = %device_id,
        peer_cert_present = peer_cert_der.is_some(),
        event = "tls_handshake_completed",
        "TLS handshake completed successfully (client role)"
    );

    Ok((tls_stream.into(), peer_cert_der))
}

/// Accept a TLS handshake as server over an established TCP stream (we
/// initiated TCP, so the peer drives the TLS client role). Always requests
/// the peer's client certificate; mandatory only when `device_id` is known
/// and trusted (SslHelper needClientAuth/wantClientAuth semantics).
///
/// Returns the TLS stream and the peer's certificate DER (if presented).
pub async fn tls_accept(
    cert_manager: Arc<CertificateManager>,
    device_id: Option<&str>,
    tcp_stream: TcpStream,
) -> Result<(TlsStream, Option<Vec<u8>>)> {
    trace!(device_id = ?device_id, event = "tls_accept_start", "Starting tls_accept (rustls server role)");

    let (certs, key) = load_own_identity(&cert_manager)?;

    let provider = crypto_provider();
    let verifier = Arc::new(TofuClientCertVerifier {
        cert_manager,
        device_id: device_id.map(str::to_string),
        provider: provider.clone(),
    });

    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .map_err(|e| Error::TlsError(format!("Failed to build TLS server config: {}", e)))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| Error::TlsError(format!("Failed to configure server certificate: {}", e)))?;

    let acceptor = TlsAcceptor::from(Arc::new(config));

    let tls_stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp_stream))
        .await
    {
        Err(_) => {
            error!(device_id = ?device_id, event = "tls_accept_timeout", "TLS accept timed out");
            return Err(Error::ConnectionTimeout(format!(
                "TLS accept timed out for {:?}",
                device_id
            )));
        }
        Ok(Err(e)) => {
            error!(device_id = ?device_id, error = %e, event = "tls_accept_error", "TLS accept failed");
            return Err(Error::TlsError(format!(
                "TLS accept failed for {:?}: {}",
                device_id, e
            )));
        }
        Ok(Ok(stream)) => stream,
    };

    let peer_cert_der = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|c| c.as_ref().to_vec());

    debug!(
        device_id = ?device_id,
        peer_cert_present = peer_cert_der.is_some(),
        event = "tls_accept_completed",
        "TLS handshake completed successfully (server role)"
    );

    Ok((tls_stream.into(), peer_cert_der))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    //! Conformance tests derived from docs/reference/SslHelper.kt. Every test
    //! here pins a behavior the Android app depends on; if one breaks, live
    //! interop breaks.

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const OUR_ID: &str = "our-device-aaaaaaaaaaaaaaaaaaaaaaaa";
    const PEER_ID: &str = "peer-device-aaaaaaaaaaaaaaaaaaaaaa";

    fn setup() -> (Arc<CertificateManager>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let cm = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
        cm.init().expect("init");
        (cm, temp_dir)
    }

    /// Generate a full own-identity (own.crt/own.key) for `cm` under `id`.
    fn make_identity(cm: &CertificateManager, id: &str, name: &str) {
        let (cert_pem, key_pem) = cm.generate_certificate(id, name).expect("generate cert");
        cm.store_own_certificate(&cert_pem, &key_pem)
            .expect("store own cert");
    }

    /// A raw rustls client config for playing the "peer" side of a test —
    /// accepts any server cert, optionally presents a client cert.
    async fn raw_client(
        server_addr: std::net::SocketAddr,
        client_identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
    ) -> std::result::Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
        #[derive(Debug)]
        struct AcceptAll {
            provider: Arc<rustls::crypto::CryptoProvider>,
        }
        impl ServerCertVerifier for AcceptAll {
            fn verify_server_cert(
                &self,
                _e: &CertificateDer<'_>,
                _i: &[CertificateDer<'_>],
                _s: &ServerName<'_>,
                _o: &[u8],
                _n: UnixTime,
            ) -> std::result::Result<ServerCertVerified, rustls::Error> {
                Ok(ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                m: &[u8],
                c: &CertificateDer<'_>,
                d: &DigitallySignedStruct,
            ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                rustls::crypto::verify_tls12_signature(
                    m,
                    c,
                    d,
                    &self.provider.signature_verification_algorithms,
                )
            }
            fn verify_tls13_signature(
                &self,
                m: &[u8],
                c: &CertificateDer<'_>,
                d: &DigitallySignedStruct,
            ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                rustls::crypto::verify_tls13_signature(
                    m,
                    c,
                    d,
                    &self.provider.signature_verification_algorithms,
                )
            }
            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                self.provider
                    .signature_verification_algorithms
                    .supported_schemes()
            }
        }

        let provider = crypto_provider();
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS12])
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAll { provider }));
        let config = match client_identity {
            Some((certs, key)) => builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| e.to_string())?,
            None => builder.with_no_client_auth(),
        };
        let connector = TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect(server_addr)
            .await
            .map_err(|e| e.to_string())?;
        let server_name = ServerName::try_from(TLS_SERVER_NAME)
            .map_err(|e| e.to_string())?
            .to_owned();
        connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| e.to_string())
    }

    /// Load an identity from PEM pairs produced by a second CertificateManager
    /// (simulating a separate device with its own cert).
    fn load_identity_from(
        cm: &CertificateManager,
        id: &str,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let (cert_pem, key_pem) = cm.load_certificate(id).expect("load peer-side identity");
        let certs = CertificateDer::pem_slice_iter(&cert_pem)
            .collect::<std::result::Result<_, _>>()
            .expect("parse certs");
        let key = PrivateKeyDer::from_pem_slice(&key_pem).expect("parse key");
        (certs, key)
    }

    /// SslHelper.kt:181-185 — server requests the client cert (wantClientAuth
    /// when untrusted), client presents it, both sides see the peer's cert,
    /// and the negotiated version is exactly TLSv1.2 (SslHelper.kt:166).
    #[tokio::test]
    async fn test_mutual_tls_12_both_roles() {
        let (our_cm, _t1) = setup();
        make_identity(&our_cm, OUR_ID, "Us");
        let (peer_cm, _t2) = setup();
        peer_cm
            .ensure_certificate(PEER_ID, "Peer")
            .expect("peer cert");
        let peer_identity = load_identity_from(&peer_cm, PEER_ID);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Our side: TLS server, peer id unknown (pre-pairing) → wantClientAuth.
        let server_cm = our_cm.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            tls_accept(server_cm, None, tcp).await
        });

        // Peer side: raw rustls client presenting its cert.
        let client = tokio::spawn(raw_client(addr, Some(peer_identity)));

        let (server_stream, server_seen_cert) = server.await.expect("join").expect("tls_accept");
        let client_stream = client.await.expect("join").expect("raw_client");

        // Version pin: exactly TLSv1.2 on both ends.
        let (_, server_conn) = server_stream.get_ref();
        let tokio_rustls::TlsStream::Server(_) = server_stream else {
            panic!("expected server stream");
        };
        assert_eq!(
            server_conn.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_2)
        );
        let (_, client_conn) = client_stream.get_ref();
        assert_eq!(
            client_conn.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_2)
        );

        // Mutual: server saw the client's cert, client saw the server's cert.
        let server_seen = server_seen_cert.expect("server must capture client cert");
        let client_seen = client_conn
            .peer_certificates()
            .and_then(|c| c.first())
            .expect("client must see server cert");

        // CN checks: each side's captured cert belongs to the expected id.
        assert_eq!(
            crate::protocol::crypto::extract_cn_from_der(&server_seen).expect("cn"),
            PEER_ID
        );
        assert_eq!(
            crate::protocol::crypto::extract_cn_from_der(client_seen.as_ref()).expect("cn"),
            OUR_ID
        );
    }

    /// SslHelper.kt:166 — TLS 1.3 must NOT negotiate. A 1.3-only client is
    /// rejected by our 1.2-pinned server.
    #[tokio::test]
    async fn test_tls13_only_client_rejected() {
        let (our_cm, _t1) = setup();
        make_identity(&our_cm, OUR_ID, "Us");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server_cm = our_cm.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            tls_accept(server_cm, None, tcp).await
        });

        let client = tokio::spawn(async move {
            #[derive(Debug)]
            struct AcceptAll {
                provider: Arc<rustls::crypto::CryptoProvider>,
            }
            impl ServerCertVerifier for AcceptAll {
                fn verify_server_cert(
                    &self,
                    _e: &CertificateDer<'_>,
                    _i: &[CertificateDer<'_>],
                    _s: &ServerName<'_>,
                    _o: &[u8],
                    _n: UnixTime,
                ) -> std::result::Result<ServerCertVerified, rustls::Error> {
                    Ok(ServerCertVerified::assertion())
                }
                fn verify_tls12_signature(
                    &self,
                    m: &[u8],
                    c: &CertificateDer<'_>,
                    d: &DigitallySignedStruct,
                ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                    rustls::crypto::verify_tls12_signature(
                        m,
                        c,
                        d,
                        &self.provider.signature_verification_algorithms,
                    )
                }
                fn verify_tls13_signature(
                    &self,
                    m: &[u8],
                    c: &CertificateDer<'_>,
                    d: &DigitallySignedStruct,
                ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                    rustls::crypto::verify_tls13_signature(
                        m,
                        c,
                        d,
                        &self.provider.signature_verification_algorithms,
                    )
                }
                fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                    self.provider
                        .signature_verification_algorithms
                        .supported_schemes()
                }
            }
            let provider = crypto_provider();
            let config = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| e.to_string())?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAll { provider }))
                .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(config));
            let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
            let name = ServerName::try_from(TLS_SERVER_NAME)
                .map_err(|e| e.to_string())?
                .to_owned();
            connector
                .connect(name, tcp)
                .await
                .map_err(|e| e.to_string())
        });

        let server_result = server.await.expect("join");
        let client_result = client.await.expect("join");
        assert!(
            server_result.is_err() || client_result.is_err(),
            "TLS 1.3-only client must not complete a handshake with our TLS 1.2-pinned server"
        );
    }

    /// TOFU hard pin (client role): a stored fingerprint that does NOT match
    /// the presented cert fails the handshake itself — not a post-handshake
    /// audit.
    #[tokio::test]
    async fn test_fingerprint_mismatch_fails_handshake_client_role() {
        let (our_cm, _t1) = setup();
        make_identity(&our_cm, OUR_ID, "Us");

        // Peer side: a real rustls server with its own cert.
        let (peer_cm, _t2) = setup();
        peer_cm
            .ensure_certificate(PEER_ID, "Peer")
            .expect("peer cert");
        let (peer_certs, peer_key) = load_identity_from(&peer_cm, PEER_ID);

        // Our TOFU store holds a fingerprint for PEER_ID that belongs to a
        // DIFFERENT cert (impostor scenario).
        let (impostor_pem, _) = our_cm
            .generate_certificate(PEER_ID, "Impostor")
            .expect("impostor cert");
        let impostor_der = openssl::x509::X509::from_pem(&impostor_pem)
            .expect("parse")
            .to_der()
            .expect("der");
        let impostor_fp =
            CertificateManager::compute_fingerprint(&impostor_der).expect("fingerprint");
        our_cm
            .store_peer_fingerprint(PEER_ID, &impostor_fp)
            .expect("store fp");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let config = rustls::ServerConfig::builder_with_provider(crypto_provider())
                .with_protocol_versions(&[&rustls::version::TLS12])
                .map_err(|e| e.to_string())?
                .with_no_client_auth()
                .with_single_cert(peer_certs, peer_key)
                .map_err(|e| e.to_string())?;
            TlsAcceptor::from(Arc::new(config))
                .accept(tcp)
                .await
                .map_err(|e| e.to_string())
        });

        let result = tls_connect(our_cm.clone(), PEER_ID, {
            // tls_connect consumes an already-connected TCP stream.
            TcpStream::connect(addr).await.expect("connect")
        })
        .await;

        assert!(
            result.is_err(),
            "stored-fingerprint mismatch must fail the handshake"
        );
        // The server side sees the handshake torn down too (or may complete
        // its half before the alert arrives — either way it must not matter).
        let _ = server.await;
    }

    /// TOFU first use (client role): no stored fingerprint → handshake
    /// succeeds and the caller captures the peer cert for storage.
    #[tokio::test]
    async fn test_tofu_first_use_captures_cert_client_role() {
        let (our_cm, _t1) = setup();
        make_identity(&our_cm, OUR_ID, "Us");

        let (peer_cm, _t2) = setup();
        peer_cm
            .ensure_certificate(PEER_ID, "Peer")
            .expect("peer cert");
        let (peer_certs, peer_key) = load_identity_from(&peer_cm, PEER_ID);
        let expected_der: Vec<u8> = peer_certs[0].as_ref().to_vec();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let config = rustls::ServerConfig::builder_with_provider(crypto_provider())
                .with_protocol_versions(&[&rustls::version::TLS12])
                .map_err(|e| e.to_string())?
                .with_no_client_auth()
                .with_single_cert(peer_certs, peer_key)
                .map_err(|e| e.to_string())?;
            let mut s = TlsAcceptor::from(Arc::new(config))
                .accept(tcp)
                .await
                .map_err(|e| e.to_string())?;
            // Keep the connection alive briefly so the client finishes cleanly.
            let mut buf = [0u8; 16];
            let _ = s.read(&mut buf).await;
            Ok::<_, String>(())
        });

        let (stream, peer_cert) = tls_connect(
            our_cm.clone(),
            PEER_ID,
            TcpStream::connect(addr).await.expect("connect"),
        )
        .await
        .expect("first-use handshake must succeed");

        let captured = peer_cert.expect("peer cert must be captured for TOFU storage");
        assert_eq!(captured, expected_der);
        // Close the client side so the server's keep-alive read returns and
        // the spawned task can finish.
        drop(stream);
        let _ = server.await;
    }

    /// Trusted peer (server role): device id known + fingerprint stored →
    /// client auth is MANDATORY (SslHelper needClientAuth). A client that
    /// presents no cert is rejected.
    #[tokio::test]
    async fn test_trusted_peer_requires_client_cert_server_role() {
        let (our_cm, _t1) = setup();
        make_identity(&our_cm, OUR_ID, "Us");

        // Trust anchor: a stored fingerprint for PEER_ID (contents don't
        // matter — the client will present nothing).
        our_cm
            .store_peer_fingerprint(PEER_ID, "deadbeef")
            .expect("store fp");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server_cm = our_cm.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            tls_accept(server_cm, Some(PEER_ID), tcp).await
        });

        // Raw client WITHOUT a client certificate.
        let client = tokio::spawn(raw_client(addr, None));

        let server_result = server.await.expect("join");
        let client_result = client.await.expect("join");
        assert!(
            server_result.is_err() || client_result.is_err(),
            "a certless client must not complete a handshake with a trusted peer (needClientAuth)"
        );
    }

    /// Trusted peer (server role): stored fingerprint matching the presented
    /// client cert → handshake succeeds and the cert is captured.
    #[tokio::test]
    async fn test_trusted_peer_matching_cert_accepted_server_role() {
        let (our_cm, _t1) = setup();
        make_identity(&our_cm, OUR_ID, "Us");

        let (peer_cm, _t2) = setup();
        peer_cm
            .ensure_certificate(PEER_ID, "Peer")
            .expect("peer cert");
        let (peer_certs, peer_key) = load_identity_from(&peer_cm, PEER_ID);

        let fp = CertificateManager::compute_fingerprint(peer_certs[0].as_ref()).expect("fp");
        our_cm
            .store_peer_fingerprint(PEER_ID, &fp)
            .expect("store fp");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server_cm = our_cm.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            tls_accept(server_cm, Some(PEER_ID), tcp).await
        });

        let client = tokio::spawn(raw_client(addr, Some((peer_certs, peer_key))));

        let (_stream, peer_cert) = server.await.expect("join").expect("tls_accept");
        let _client_stream = client.await.expect("join").expect("raw_client");
        let captured = peer_cert.expect("trusted peer cert must be captured");
        assert_eq!(
            crate::protocol::crypto::extract_cn_from_der(&captured).expect("cn"),
            PEER_ID
        );
    }

    /// Data flows over the rustls stream in both directions (the packet layer
    /// depends on split read/write halves working).
    #[tokio::test]
    async fn test_data_roundtrip_over_tls() {
        let (our_cm, _t1) = setup();
        make_identity(&our_cm, OUR_ID, "Us");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server_cm = our_cm.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let (mut stream, _) = tls_accept(server_cm, None, tcp).await.expect("tls_accept");
            let mut buf = vec![0u8; 4];
            stream.read_exact(&mut buf).await.expect("read");
            stream.write_all(&buf).await.expect("write");
            stream.flush().await.expect("flush");
            buf
        });

        let (mut client_stream, _) = tls_connect(
            our_cm.clone(),
            PEER_ID,
            TcpStream::connect(addr).await.expect("connect"),
        )
        .await
        .expect("tls_connect");

        client_stream.write_all(b"ping").await.expect("write");
        client_stream.flush().await.expect("flush");
        let mut buf = vec![0u8; 4];
        client_stream.read_exact(&mut buf).await.expect("read");

        assert_eq!(&buf, b"ping");
        assert_eq!(&server.await.expect("join"), b"ping");
    }
}

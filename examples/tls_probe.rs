//! Live TLS interop probe: rustls (ring, TLSv1.2 pinned) vs the real KDE
//! Connect Android app (Conscrypt). Tests BOTH protocol roles:
//!
//!   server-role — we initiate TCP to the phone, so we are the TLS *server*
//!                 (and must REQUEST the phone's client cert: native-tls
//!                 cannot do this, which is the migration's whole point).
//!   client-role — the phone initiates TCP to us after our UDP broadcast, so
//!                 the phone is the TLS server and we are the TLS *client*
//!                 (and must PRESENT our cert when the phone requests it).
//!
//! Usage:
//!   cargo run --example tls-probe -- server-role 192.168.0.158:1716 CERT KEY DEV_ID DEV_NAME
//!   cargo run --example tls-probe -- client-role 192.168.0.255 CERT KEY DEV_ID DEV_NAME [TCP_PORT]
//!
//! Prints PASS/FAIL per role with the negotiated cipher suite and whether the
//! peer's certificate arrived. Diagnostic only — accepts any peer cert and
//! reports its SHA-256 instead of verifying.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

type Captured = Arc<Mutex<Vec<CertificateDer<'static>>>>;

#[derive(Debug)]
struct AnyClientCert {
    captured: Captured,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // Probe posture: capture-and-report, don't hard-fail if absent.
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.captured
            .lock()
            .expect("captured lock")
            .push(end_entity.clone().into_owned());
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

#[derive(Debug)]
struct AnyServerCert {
    captured: Captured,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AnyServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.captured
            .lock()
            .expect("captured lock")
            .push(end_entity.clone().into_owned());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

fn load_pem(
    cert_path: &str,
    key_path: &str,
) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let cert_bytes = std::fs::read(cert_path).expect("read cert");
    let key_bytes = std::fs::read(key_path).expect("read key");
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_bytes)
        .collect::<Result<_, _>>()
        .expect("parse cert PEM");
    let key = PrivateKeyDer::from_pem_slice(&key_bytes).expect("parse key PEM");
    (certs, key)
}

fn identity_packet(device_id: &str, device_name: &str, tcp_port: Option<u16>) -> String {
    let mut body = serde_json::json!({
        "deviceId": device_id,
        "deviceName": device_name,
        "protocolVersion": 8,
        "deviceType": "laptop",
        "incomingCapabilities": [],
        "outgoingCapabilities": [],
    });
    if let Some(p) = tcp_port {
        body["tcpPort"] = serde_json::json!(p);
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    serde_json::json!({ "id": now_ms, "type": "kdeconnect.identity", "body": body }).to_string()
}

fn report(role: &str, result: Result<(Option<String>, Option<String>, Captured), String>) -> i32 {
    match result {
        Ok((version, suite, captured)) => {
            let certs = captured.lock().expect("captured lock");
            println!("ROLE {role}: HANDSHAKE OK");
            println!("  protocol: {}", version.as_deref().unwrap_or("?"));
            println!("  suite:    {}", suite.as_deref().unwrap_or("?"));
            if certs.is_empty() {
                println!("  peer cert: NONE PRESENTED");
            } else {
                for (i, c) in certs.iter().enumerate() {
                    println!(
                        "  peer cert[{i}]: sha256={} ({} bytes)",
                        sha256_hex(c.as_ref()),
                        c.as_ref().len()
                    );
                }
            }
            0
        }
        Err(e) => {
            println!("ROLE {role}: FAIL — {e}");
            1
        }
    }
}

async fn run_server_role(
    addr: &str,
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    device_id: &str,
    device_name: &str,
) -> Result<(Option<String>, Option<String>, Captured), String> {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(AnyClientCert {
        captured: captured.clone(),
        provider: provider.clone(),
    });
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .map_err(|e| format!("build server config: {e}"))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("server single cert: {e}"))?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("tcp connect {addr}: {e}"))?;
    // Plaintext identity first (protocol step 3), then we become the TLS server.
    let ident = identity_packet(device_id, device_name, None);
    stream
        .write_all(ident.as_bytes())
        .await
        .map_err(|e| format!("write identity: {e}"))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|e| format!("write identity nl: {e}"))?;
    stream.flush().await.map_err(|e| format!("flush: {e}"))?;

    let tls = tokio::time::timeout(Duration::from_secs(20), acceptor.accept(stream))
        .await
        .map_err(|_| "TLS server handshake timed out (20s)".to_string())?
        .map_err(|e| format!("TLS server handshake: {e}"))?;

    let (_, conn) = tls.get_ref();
    let version = conn.protocol_version().map(|v| format!("{v:?}"));
    let suite = conn
        .negotiated_cipher_suite()
        .map(|s| format!("{:?}", s.suite()));
    Ok((version, suite, captured))
}

async fn run_client_role(
    broadcast: &str,
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    device_id: &str,
    device_name: &str,
    tcp_port: u16,
) -> Result<(Option<String>, Option<String>, Captured), String> {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(AnyServerCert {
        captured: captured.clone(),
        provider: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .map_err(|e| format!("build client config: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .map_err(|e| format!("client auth cert: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", tcp_port))
        .await
        .map_err(|e| format!("bind tcp {tcp_port}: {e}"))?;

    // Broadcast our identity with the tcpPort the phone should connect to.
    // Send from source port 1716 like real implementations do (the running
    // daemon normally holds it; fall back to an ephemeral port if taken).
    let udp = std::net::UdpSocket::bind(("0.0.0.0", 1716))
        .or_else(|_| std::net::UdpSocket::bind(("0.0.0.0", 0)))
        .map_err(|e| format!("udp bind: {e}"))?;
    println!(
        "udp source port: {}",
        udp.local_addr().map(|a| a.port()).unwrap_or(0)
    );
    udp.set_broadcast(true)
        .map_err(|e| format!("udp bcast: {e}"))?;
    let ident = identity_packet(device_id, device_name, Some(tcp_port));
    for _ in 0..3 {
        udp.send_to(ident.as_bytes(), (broadcast, 1716))
            .map_err(|e| format!("udp send: {e}"))?;
        std::thread::sleep(Duration::from_millis(300));
    }
    println!("identity broadcast sent; waiting up to 60s for the phone to connect (open KDE Connect and refresh)…");

    let (mut stream, peer) = tokio::time::timeout(Duration::from_secs(60), listener.accept())
        .await
        .map_err(|_| "no TCP connection from phone within 60s".to_string())?
        .map_err(|e| format!("accept: {e}"))?;
    println!("TCP connection from {peer}");

    // Phone sends its plaintext identity first. Read byte-by-byte up to the
    // newline so we don't buffer past it and eat the TLS ClientHello bytes.
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let b = stream.read_u8().await?;
            if b == b'\n' {
                return Ok::<_, std::io::Error>(());
            }
            line.push(b as char);
        }
    })
    .await
    .map_err(|_| "phone identity read timed out".to_string())?
    .map_err(|e| format!("read identity: {e}"))?;
    println!("phone identity: {}", line.trim());

    // Phone initiated TCP, so the phone is the TLS server; we are the client.
    let server_name = ServerName::try_from("kdeconnect-probe")
        .map_err(|e| format!("server name: {e}"))?
        .to_owned();
    let tls = tokio::time::timeout(
        Duration::from_secs(20),
        connector.connect(server_name, stream),
    )
    .await
    .map_err(|_| "TLS client handshake timed out (20s)".to_string())?
    .map_err(|e| format!("TLS client handshake: {e}"))?;

    let (_, conn) = tls.get_ref();
    let version = conn.protocol_version().map(|v| format!("{v:?}"));
    let suite = conn
        .negotiated_cipher_suite()
        .map(|s| format!("{:?}", s.suite()));
    Ok((version, suite, captured))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 7 {
        eprintln!(
            "usage:\n  {0} server-role PHONE_IP:PORT CERT KEY DEV_ID DEV_NAME\n  {0} client-role BROADCAST_IP CERT KEY DEV_ID DEV_NAME [TCP_PORT]",
            args[0]
        );
        std::process::exit(2);
    }
    let mode = &args[1];
    let certs_key = load_pem(&args[3], &args[4]);
    let code = match mode.as_str() {
        "server-role" => {
            let r = run_server_role(&args[2], certs_key.0, certs_key.1, &args[5], &args[6]).await;
            report(
                "server (we connect → we are TLS server, request client cert)",
                r,
            )
        }
        "client-role" => {
            let port: u16 = args
                .get(7)
                .map(|s| s.parse().expect("port"))
                .unwrap_or(1736);
            let r =
                run_client_role(&args[2], certs_key.0, certs_key.1, &args[5], &args[6], port).await;
            report(
                "client (phone connects → phone is TLS server, we present cert)",
                r,
            )
        }
        other => {
            eprintln!("unknown mode: {other}");
            2
        }
    };
    std::process::exit(code);
}

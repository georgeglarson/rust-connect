//! USB integration tests — test against the real KDE Connect Android app
//!
//! These tests require:
//! 1. An Android device with KDE Connect installed
//! 2. USB debugging enabled (adb)
//! 3. RUST_CONNECT_TEST_USB=1 environment variable set
//! 4. The phone and host on the same WiFi network (for UDP discovery)
//! 5. Port 1716 available on the host (KDE Connect protocol port) — if the
//!    daemon is installed and running, STOP IT FIRST, or the test fails at
//!    the listener bind with `AddrInUse`:
//!      systemctl --user stop rust-connect     # …and start it again after
//!
//! IMPORTANT: The first run will pair with the Android device. Subsequent runs
//! reuse the same certificate from a persistent directory so the Android app
//! recognizes the device.
//!
//! The three live tests (`usb_android_connects_to_us`,
//! `usb_full_protocol_handshake`, `usb_send_file_to_android`) CANNOT run in the
//! same `cargo test` invocation: each binds port 1716 (or connects to the
//! phone's 1716) and they would conflict. Run each individually, e.g.:
//!   RUST_CONNECT_TEST_USB=1 cargo test --test usb_integration usb_android_connects_to_us -- --ignored --nocapture
//!   RUST_CONNECT_TEST_USB=1 cargo test --test usb_integration usb_full_protocol_handshake -- --ignored --nocapture
//!   RUST_CONNECT_TEST_USB=1 cargo test --test usb_integration usb_send_file_to_android -- --ignored --nocapture

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rust_connect::device::DeviceType;
use rust_connect::protocol::{
    CertificateManager, ConnectionManager, Identity, Packet, PacketSerializer,
};

fn adb(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("adb").args(args).output()
}

fn adb_device_connected() -> bool {
    let Ok(output) = adb(&["devices"]) else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().skip(1).any(|line| line.contains("\tdevice"))
}

fn check_usb() -> bool {
    if std::env::var("RUST_CONNECT_TEST_USB").is_err() {
        return false;
    }
    if !adb_device_connected() {
        return false;
    }
    true
}

fn get_android_wifi_ip() -> Option<String> {
    let output = adb(&["shell", "ip", "addr", "show", "wlan0"]).ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("inet ") && !trimmed.contains("127.0.0.1") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                return parts[1].split('/').next().map(String::from);
            }
        }
    }
    None
}

fn get_host_wifi_ip() -> Option<String> {
    let output = Command::new("ip").args(["addr", "show"]).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("inet 192.168.") || trimmed.starts_with("inet 10.") {
            if let Some(ip_cidr) = trimmed.strip_prefix("inet ") {
                if let Some(ip) = ip_cidr.split('/').next() {
                    if ip.starts_with("10.151.") || ip.starts_with("127.") {
                        continue;
                    }
                    return Some(ip.to_string());
                }
            }
        }
    }
    None
}

fn calculate_broadcast_ip(device_ip: &str) -> String {
    let parts: Vec<&str> = device_ip.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.{}.255", parts[0], parts[1], parts[2])
    } else {
        "255.255.255.255".to_string()
    }
}

fn send_discovery_broadcast(
    identity: &Identity,
    bind_addr: SocketAddr,
    broadcast_addr: SocketAddr,
) -> std::io::Result<()> {
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    socket.set_broadcast(true)?;

    let packet = identity.to_packet().unwrap();
    let bytes = PacketSerializer::serialize(&packet).unwrap();
    socket.send_to(&bytes, broadcast_addr)?;
    Ok(())
}

/// Test: Android device connects to us after receiving our UDP discovery broadcast
///
/// Note: This test is ignored by default because it conflicts with
/// `usb_full_protocol_handshake` (both need port 1716). Run individually.
///
/// The phone only dials our listener AFTER it sees our UDP discovery
/// broadcast (it learns our address from the identity packet's `tcpPort`);
/// without the broadcast it reports "Device not reachable".
///
/// This exercises the REAL inbound path, exactly as the production daemon
/// listener wires it (`src/protocol/listener.rs::handle_connection`):
///   accept TCP → `ConnectionManager::accept_incoming` (reads the phone's
///   plaintext identity first — the dialer always writes identity on
///   connect — then runs the TLS handshake with US as TLS client, because
///   the side that accepted the TCP connection is the TLS client per
///   LanLinkProvider.java:292-294) → encrypted identity exchange.
/// The old version of this test asserted the first raw byte was 0x16 (TLS
/// handshake); that was wrong by construction — the phone sends its
/// plaintext identity (`{`-first bytes) before any TLS.
#[tokio::test]
#[ignore]
async fn usb_android_connects_to_us() {
    if !check_usb() {
        return;
    }
    let android_ip = match get_android_wifi_ip() {
        Some(ip) => ip,
        None => {
            eprintln!("SKIP: Could not get Android device WiFi IP");
            return;
        }
    };

    let host_wifi_ip = match get_host_wifi_ip() {
        Some(ip) => ip,
        None => {
            eprintln!("SKIP: Host not on same WiFi subnet as Android");
            return;
        }
    };

    let cert_dir = std::env::temp_dir().join("rust-connect-usb-test-certs");
    std::fs::create_dir_all(&cert_dir).unwrap();
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.clone()));
    cert_manager.init().unwrap();
    let device_id = cert_manager
        .ensure_certificate("b21f4b90a4dc4dc48b2da5d844a09e51", "USB Test Desktop")
        .unwrap();
    eprintln!("Device ID: {}", device_id);

    let identity = Identity::new(
        device_id.clone(),
        "USB Test Desktop".to_string(),
        DeviceType::Desktop,
        vec!["kdeconnect.ping".to_string()],
        vec!["kdeconnect.ping".to_string()],
    );

    let server_cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());
    server_cm.set_device_identity(&device_id, "USB Test Desktop");

    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind("0.0.0.0:1716".parse().unwrap()).unwrap();
    let listener = match socket.listen(1) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("SKIP: Could not bind: {}", e);
            return;
        }
    };

    let broadcast_ip = calculate_broadcast_ip(&android_ip);
    let broadcast_addr: SocketAddr = format!("{}:1716", broadcast_ip).parse().unwrap();
    let bind_addr: SocketAddr = format!("{}:0", host_wifi_ip).parse().unwrap();

    // Send a single discovery broadcast (multiple broadcasts cause duplicate connections)
    send_discovery_broadcast(&identity, bind_addr, broadcast_addr).unwrap();
    eprintln!("Discovery broadcast sent");

    let (stream, peer_addr) =
        match tokio::time::timeout(Duration::from_secs(15), listener.accept()).await {
            Ok(accepted) => {
                let (stream, peer_addr) = accepted.unwrap();
                (stream, peer_addr)
            }
            Err(_) => {
                panic!("Android device did not connect within 15s");
            }
        };
    eprintln!("Android device connected from {}", peer_addr);

    // Production listener wiring (listener.rs::handle_connection):
    // accept_incoming under a 30s timeout.
    let (phone_device_id, phone_identity, gen) = match tokio::time::timeout(
        Duration::from_secs(30),
        server_cm.accept_incoming(stream),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => panic!("accept_incoming failed: {}", e),
        Err(_) => panic!("accept_incoming timed out"),
    };
    eprintln!(
        "accept_incoming succeeded: {} ({}, generation {})",
        phone_device_id, phone_identity.device_name, gen
    );

    // Well-formed deviceId: KDE Connect ids are 32 lowercase hex chars.
    // We assert shape, not a specific value, so the test works against any
    // phone.
    assert_eq!(phone_device_id.len(), 32, "deviceId should be 32 chars");
    assert!(
        phone_device_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "deviceId should be lowercase hex: {}",
        phone_device_id
    );
    assert_ne!(
        phone_device_id, device_id,
        "phone must not echo our own deviceId"
    );

    // Encrypted identity exchange (listener.rs::identity_exchange): the phone
    // sends its encrypted identity, we validate it against the plaintext one
    // and reply with ours. 15s timeout, same as production.
    let exchange_result = tokio::time::timeout(Duration::from_secs(15), async {
        let encrypted = server_cm
            .recv_packet(&phone_device_id)
            .await
            .map_err(|e| format!("failed to read encrypted identity: {}", e))?;
        if !encrypted.is_identity() {
            return Err(format!(
                "expected encrypted identity packet, got {}",
                encrypted.packet_type
            ));
        }
        let enc_identity = Identity::from_packet(encrypted)
            .map_err(|e| format!("malformed encrypted identity: {}", e))?;
        if enc_identity.device_id != phone_device_id {
            return Err(format!(
                "encrypted identity deviceId '{}' does not match plaintext '{}'",
                enc_identity.device_id, phone_device_id
            ));
        }
        if enc_identity.protocol_version != phone_identity.protocol_version {
            return Err(format!(
                "encrypted identity protocolVersion {} differs from plaintext {}",
                enc_identity.protocol_version, phone_identity.protocol_version
            ));
        }
        server_cm
            .send_packet(&phone_device_id, &identity.to_tcp_packet().unwrap())
            .await
            .map_err(|e| format!("failed to send our encrypted identity: {}", e))?;
        Ok::<(), String>(())
    })
    .await;
    match exchange_result {
        Ok(Ok(())) => eprintln!("Encrypted identity exchange completed"),
        Ok(Err(e)) => panic!("identity exchange failed: {}", e),
        Err(_) => panic!("identity exchange timed out"),
    }

    // The connection must be registered and alive.
    assert!(
        server_cm.is_connected(&phone_device_id).await,
        "connection should be registered/alive after the inbound handshake"
    );
    eprintln!("Verified: inbound path works — phone dialed us, TLS as client, connection alive");

    let _ = server_cm.disconnect(&phone_device_id, gen).await;
}

/// Test: Full protocol handshake with the Android device
///
/// IGNORED by default (needs a real device on adb); run:
///   RUST_CONNECT_TEST_USB=1 cargo test --test usb_integration usb_full -- --ignored --nocapture
///
/// Verified live 2026-07-29 against the stock KDE Connect Android app on
/// real hardware (rustls stack, both roles):
/// 1. ✅ TCP connection to Android
/// 2. ✅ Plaintext identity exchange
/// 3. ✅ Mutual TLS handshake (TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256, TLSv1.2;
///      we act as TLS server and the phone presents its client cert on request)
/// 4. ✅ Encrypted identity exchange
/// 5. ✅ Pair request delivered; phone posts the pairing notification
/// 6. ✅ Pairing confirmed (pair=true) after tapping Accept on the phone
/// 7. ✅ Post-pairing plugin traffic received (kdeconnect.battery)
///
/// First run against an unpaired phone requires a manual tap on the pairing
/// notification (within the 30s receive window). Subsequent runs against the
/// same (now-trusted) phone need no interaction.
#[tokio::test]
#[ignore = "Requires a real Android device on adb; first run needs a manual pairing tap"]
async fn usb_full_protocol_handshake() {
    if !check_usb() {
        return;
    }
    let android_ip = match get_android_wifi_ip() {
        Some(ip) => ip,
        None => {
            eprintln!("SKIP: Could not get Android device WiFi IP");
            return;
        }
    };

    let _host_wifi_ip = match get_host_wifi_ip() {
        Some(ip) => ip,
        None => {
            eprintln!("SKIP: Host not on same WiFi subnet as Android");
            return;
        }
    };

    // Use a persistent cert directory so the same cert is reused across test runs.
    // The Android app stores the cert from the first pairing and rejects new certs.
    let cert_dir = std::env::temp_dir().join("rust-connect-usb-test-certs");
    std::fs::create_dir_all(&cert_dir).unwrap();
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.clone()));
    cert_manager.init().unwrap();
    let device_id = cert_manager
        .ensure_certificate("b21f4b90a4dc4dc48b2da5d844a09e51", "USB Test Desktop")
        .unwrap();
    eprintln!("Device ID: {}", device_id);

    let identity = Identity::new(
        device_id.clone(),
        "USB Test Desktop".to_string(),
        DeviceType::Desktop,
        vec![
            "kdeconnect.ping".to_string(),
            "kdeconnect.battery".to_string(),
            "kdeconnect.clipboard".to_string(),
            "kdeconnect.notification".to_string(),
        ],
        vec![
            "kdeconnect.ping".to_string(),
            "kdeconnect.battery".to_string(),
            "kdeconnect.clipboard".to_string(),
            "kdeconnect.notification".to_string(),
        ],
    );

    let server_cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());
    server_cm.set_device_identity(&device_id, "USB Test Desktop");

    // Bind to a port in the KDE Connect range (1716-1764)
    let mut actual_port = 1716u16;
    let _listener = loop {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        let bind_addr: SocketAddr = format!("0.0.0.0:{}", actual_port).parse().unwrap();
        match socket.bind(bind_addr).and_then(|_| socket.listen(1)) {
            Ok(l) => break l,
            Err(_) if actual_port < 1764 => {
                actual_port += 1;
            }
            Err(e) => {
                eprintln!("SKIP: Could not bind to any port in range 1716-1764: {}", e);
                return;
            }
        }
    };
    eprintln!("Listening on port {}", actual_port);

    // Connect directly to the Android app's TCP port.
    // The Android app listens on port 1716. When we connect:
    // 1. We send plaintext identity
    // 2. Android validates and starts TLS as CLIENT
    // 3. We accept TLS as SERVER
    // 4. Both sides exchange encrypted identities
    let android_addr: SocketAddr = format!("{}:1716", android_ip).parse().unwrap();
    eprintln!("Connecting directly to Android at {}", android_addr);

    // Use connect_to_device which handles the full flow
    let accept_result = tokio::time::timeout(Duration::from_secs(15), async {
        server_cm
            .connect_to_device(&identity, android_addr, None)
            .await
    })
    .await;

    let (android_device_id, _android_identity, gen) = match accept_result {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            panic!("Connection failed: {}", e);
        }
        Err(_) => {
            panic!("Android device did not connect within 15s");
        }
    };
    eprintln!(
        "Connected to Android device: {} (generation {})",
        android_device_id, gen
    );

    // Verify the connection is alive
    let is_connected = server_cm.is_connected(&android_device_id).await;
    eprintln!("Connection alive: {}", is_connected);
    if !is_connected {
        panic!("Connection was lost immediately after connect_to_device");
    }

    // Verify the connection is alive
    let is_connected = server_cm.is_connected(&android_device_id).await;
    eprintln!("Connection alive: {}", is_connected);
    if !is_connected {
        panic!("Connection was lost immediately after connect_to_device");
    }

    // IMMEDIATELY send pair request
    eprintln!("Sending pair request to Android...");
    let pair_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let pair_request = Packet::new(
        "kdeconnect.pair".to_string(),
        serde_json::json!({
            "pair": true,
            "timestamp": pair_ts
        }),
    );
    server_cm
        .send_packet(&android_device_id, &pair_request)
        .await
        .unwrap();
    eprintln!("Pair request sent - ACCEPT PAIRING ON YOUR PHONE NOW!");

    // SAS conformance check: the phone's pairing UI shows
    // getVerificationKey(phoneCert, ourCert, pair_ts). Compute our side and
    // print it — the two MUST match visually.
    {
        let peer_cert_der = server_cm
            .get_peer_certificate(&android_device_id)
            .await
            .expect("peer cert must be captured at handshake");
        let (our_cert_pem, _) = cert_manager.load_own_certificate().unwrap();
        let our_cert_der = openssl::x509::X509::from_pem(&our_cert_pem)
            .unwrap()
            .to_der()
            .unwrap();
        let our_pub = CertificateManager::extract_pubkey_der(&our_cert_der).unwrap();
        let peer_pub = CertificateManager::extract_pubkey_der(&peer_cert_der).unwrap();
        let sas = CertificateManager::compute_verification_key(&our_pub, &peer_pub, pair_ts);
        eprintln!("OUR SAS (must match phone UI): {}", sas);
    }
    // NOTE: do NOT send plugin packets (e.g. battery requests) before the
    // phone confirms pairing — the Android app answers plugin traffic from
    // an unpaired device with pair=false (observed live vs a test phone
    // 2026-07-29).

    // Wait for pairing confirmation, then proof the encrypted link carries
    // real traffic. Post-pairing Android does NOT re-send its identity — it
    // immediately pushes plugin packets for our advertised capabilities
    // (e.g. kdeconnect.battery). Any packet received after pairing is
    // confirmed is liveness proof.
    let mut pairing_confirmed = false;
    let mut link_live = false;
    let mut attempts = 0;
    let max_attempts = 30;

    while !link_live && attempts < max_attempts {
        attempts += 1;
        let packet_result = tokio::time::timeout(
            Duration::from_secs(30),
            server_cm.recv_packet(&android_device_id),
        )
        .await;

        match packet_result {
            Ok(Ok(packet)) => {
                if packet.packet_type == "kdeconnect.pair" {
                    let pair_value = packet.body["pair"].as_bool().unwrap_or(false);
                    eprintln!("Received pair packet (pair={})", pair_value);

                    if pair_value {
                        eprintln!("Pairing confirmed by Android");
                        pairing_confirmed = true;
                        server_cm
                            .send_packet(&android_device_id, &Packet::ping())
                            .await
                            .unwrap();
                        eprintln!("Sent ping");
                        // A paired phone ignores a duplicate pair request and
                        // battery updates are only pushed on change, so elicit
                        // a deterministic reply: the battery plugin always
                        // answers a request with the current status.
                        let battery_request = Packet::new(
                            "kdeconnect.battery.request".to_string(),
                            serde_json::json!({}),
                        );
                        server_cm
                            .send_packet(&android_device_id, &battery_request)
                            .await
                            .unwrap();
                        eprintln!("Battery request sent");
                    } else {
                        // pair=false is a rejection/unpair, not a request —
                        // answering it with pair=true would be meaningless.
                        panic!(
                            "Pairing rejected by Android (pair=false) — tap Accept, not Reject, on the notification"
                        );
                    }
                } else if packet.packet_type == "kdeconnect.identity" {
                    let body = &packet.body;
                    let device_name = body["deviceName"].as_str().unwrap_or("unknown");
                    let device_type = body["deviceType"].as_str().unwrap_or("unknown");
                    eprintln!(
                        "Connected to Android: {} (type: {})",
                        device_name, device_type
                    );
                    if pairing_confirmed {
                        link_live = true;
                    } else {
                        server_cm
                            .send_packet(&android_device_id, &Packet::ping())
                            .await
                            .unwrap();
                        eprintln!("Sent ping");
                    }
                } else {
                    // Plugin traffic is only ever sent to paired devices, so
                    // any post-handshake packet proves a live, paired link —
                    // this is also what makes reruns against an
                    // already-trusted phone pass without another pair=true.
                    eprintln!("Received post-pairing packet type: {}", packet.packet_type);
                    link_live = true;
                }
            }
            Ok(Err(e)) => {
                panic!("Failed to receive packet (attempt {}): {}", attempts, e);
            }
            Err(_) => {
                panic!("Timed out waiting for packet (attempt {})", attempts);
            }
        }
    }

    if !link_live {
        panic!(
            "Link never proved live (pairing_confirmed={}) after {} attempts",
            pairing_confirmed, max_attempts
        );
    }

    let _ = server_cm.disconnect(&android_device_id, gen).await;
}

/// Test: Verify Android device info via adb
#[tokio::test]
async fn usb_android_device_info() {
    if !check_usb() {
        return;
    }

    let output = adb(&["shell", "settings", "get", "system", "bluetooth_name"]);
    if let Ok(output) = output {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() && name != "null" {
            eprintln!("Android device name: {}", name);
        }
    }

    if let Some(ip) = get_android_wifi_ip() {
        eprintln!("Android WiFi IP: {}", ip);
    }

    let output = adb(&["shell", "pm", "list", "packages", "org.kde.kdeconnect_tp"]);
    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("org.kde.kdeconnect_tp"),
            "KDE Connect is not installed on the Android device"
        );
        eprintln!("KDE Connect is installed");
    }

    let output = adb(&["shell", "ss", "-tln"]);
    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(":1716"),
            "KDE Connect is not listening on port 1716"
        );
        eprintln!("KDE Connect is listening on port 1716");
    }
}

/// Test: UDP discovery broadcast format is valid
#[tokio::test]
async fn usb_discovery_broadcast_format() {
    if !check_usb() {
        return;
    }
    let cert_dir = std::env::temp_dir().join("rust-connect-usb-test-certs");
    std::fs::create_dir_all(&cert_dir).unwrap();
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.clone()));
    cert_manager.init().unwrap();
    let device_id = cert_manager
        .ensure_certificate("b21f4b90a4dc4dc48b2da5d844a09e51", "USB Test Desktop")
        .unwrap();

    let identity = Identity::new(
        device_id,
        "USB Test Desktop".to_string(),
        DeviceType::Desktop,
        vec!["kdeconnect.ping".to_string()],
        vec!["kdeconnect.ping".to_string()],
    );

    let packet = identity.to_packet().unwrap();
    let bytes = PacketSerializer::serialize(&packet).unwrap();

    assert!(bytes.ends_with(b"\n"), "Packet should end with newline");

    let json_str = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
    let value: serde_json::Value = serde_json::from_str(json_str).unwrap();

    assert_eq!(value["type"], "kdeconnect.identity");
    assert_eq!(value["body"]["deviceName"], "USB Test Desktop");
    assert_eq!(value["body"]["deviceType"], "desktop");
    assert_eq!(value["body"]["protocolVersion"], 8);
    assert!(value["body"]["tcpPort"].is_number());

    eprintln!("Discovery broadcast packet is well-formed");
}

/// Test: Send a file to the Android device over TLS payload transfer
///
/// IGNORED by default (real device; needs a manual pairing tap on first run
/// against an unpaired phone); run:
///   RUST_CONNECT_TEST_USB=1 cargo test --test usb_integration usb_send_file -- --ignored --nocapture
///
/// SINGLE inbound-only link. An earlier version of this test dialed
/// outbound AND had the phone dial inbound after our broadcast; the
/// two-link race tripped Android's duplicate-link handling and the phone
/// closed the link ("Connection closed by ..."), failing the test in ~2s.
/// So this test never dials: we broadcast, the phone dials us (it learns
/// reachability from the UDP identity's tcpPort — without a broadcast it
/// reports "Device not reachable"), and that one accepted connection is
/// the main link for pairing AND the share.request.
///
/// Flow: bind inbound listener (1716-1764 scan) FIRST → broadcast our
/// identity with that tcpPort every ~2s → the phone dials →
/// accept_incoming + encrypted-identity reply leg (production listener
/// wiring, listener.rs::handle_connection/identity_exchange) → send OUR
/// pair request on that link → the operator taps "Request pairing" on the
/// phone → mutual pairing (both sides sent pair=true) auto-accepts on
/// both ends (PairingHandler Requested state; pairing/mod.rs mutual
/// accept) → share.request with top-level payloadSize +
/// payloadTransferInfo → the phone connects back to our ephemeral
/// listener over TLS (we are the TLS server, LanLink.java server mode)
/// and pulls the file.
///
/// Pairing detection: pair=true OR any plugin packet (already-paired
/// proof, same liveness rule `usb_full_protocol_handshake` uses) within a
/// 60s budget. A single recv timeout is tolerated, but a recv error such
/// as "Connection closed" is fatal to the single link and fails fast.
#[tokio::test]
#[ignore = "Requires a real Android device on adb; needs a manual pairing tap"]
async fn usb_send_file_to_android() {
    if !check_usb() {
        return;
    }
    let android_ip = match get_android_wifi_ip() {
        Some(ip) => ip,
        None => {
            eprintln!("SKIP: Could not get Android device WiFi IP");
            return;
        }
    };
    let host_wifi_ip = match get_host_wifi_ip() {
        Some(ip) => ip,
        None => {
            eprintln!("SKIP: Host not on same WiFi subnet as Android");
            return;
        }
    };

    let cert_dir = std::env::temp_dir().join("rust-connect-usb-test-certs");
    std::fs::create_dir_all(&cert_dir).unwrap();
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.clone()));
    cert_manager.init().unwrap();
    let device_id = cert_manager
        .ensure_certificate("b21f4b90a4dc4dc48b2da5d844a09e51", "USB Test Desktop")
        .unwrap();

    let identity = Identity::new(
        device_id.clone(),
        "USB Test Desktop".to_string(),
        DeviceType::Desktop,
        vec!["kdeconnect.share.request".to_string()],
        vec!["kdeconnect.share.request".to_string()],
    );

    let server_cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());
    server_cm.set_device_identity(&device_id, "USB Test Desktop");

    // Single inbound-only link (see the doc comment for the two-link race
    // this avoids). The phone dials us after seeing our broadcast, exactly
    // like usb_android_connects_to_us. Bind the listener FIRST so the
    // broadcast identity advertises a real tcpPort.
    let mut inbound_port = 1716u16;
    let inbound_listener = loop {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        let bind_addr: SocketAddr = format!("0.0.0.0:{}", inbound_port).parse().unwrap();
        match socket.bind(bind_addr).and_then(|_| socket.listen(8)) {
            Ok(l) => break l,
            Err(_) if inbound_port < 1764 => {
                inbound_port += 1;
            }
            Err(e) => {
                eprintln!(
                    "SKIP: Could not bind inbound listener in range 1716-1764: {}",
                    e
                );
                return;
            }
        }
    };
    eprintln!(
        "Inbound listener for phone-initiated pairing on port {}",
        inbound_port
    );

    let mut broadcast_identity = identity.clone();
    broadcast_identity.tcp_port = Some(inbound_port);
    let broadcast_addr: SocketAddr = format!("{}:1716", calculate_broadcast_ip(&android_ip))
        .parse()
        .unwrap();
    let udp_bind_addr: SocketAddr = format!("{}:0", host_wifi_ip).parse().unwrap();

    // Broadcast at most 3 times, 10s apart, and STOP the moment the phone
    // dials (aborted below at the accept). Observed live: every UDP
    // re-announcement makes the phone tear down its current link to us and
    // redial (the original test's "multiple broadcasts cause duplicate
    // connections" warning) — a 2s re-broadcast loop put the phone in a
    // redial storm and our pair request died on the abandoned first link.
    let broadcast_task = tokio::spawn(async move {
        for _ in 0..3 {
            if let Err(e) =
                send_discovery_broadcast(&broadcast_identity, udp_bind_addr, broadcast_addr)
            {
                eprintln!("Discovery broadcast failed: {}", e);
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    eprintln!(">>> Broadcasting — waiting for the phone to dial us");

    // The phone dials after seeing the broadcast. The FIRST accepted
    // connection is the main (only) link for the rest of the test.
    let (stream, peer_addr) =
        tokio::time::timeout(Duration::from_secs(60), inbound_listener.accept())
            .await
            .expect("phone did not dial us within 60s of the first broadcast")
            .expect("inbound accept failed");
    eprintln!(
        "Phone dialed us from {} — running inbound handshake",
        peer_addr
    );
    // Stop announcing immediately: another broadcast would make the phone
    // drop this link and redial.
    broadcast_task.abort();

    // Ignore any further dials (repeated broadcasts can cause duplicates):
    // the single-link design depends on no second link racing the first.
    let reject_task = tokio::spawn(async move {
        while let Ok((_dup, addr)) = inbound_listener.accept().await {
            eprintln!(
                "Ignoring duplicate inbound connection from {} (single-link test)",
                addr
            );
        }
    });

    // Production listener wiring (listener.rs::handle_connection):
    // accept_incoming under the 30s timeout, then the encrypted-identity
    // reply leg (listener.rs::identity_exchange) — without our reply the
    // phone drops the link.
    let (android_device_id, _phone_identity, _gen) = match tokio::time::timeout(
        Duration::from_secs(30),
        server_cm.accept_incoming(stream),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => panic!("accept_incoming failed: {}", e),
        Err(_) => panic!("accept_incoming timed out"),
    };
    eprintln!("accept_incoming succeeded: {}", android_device_id);

    let exchange = tokio::time::timeout(Duration::from_secs(15), async {
        let encrypted = server_cm.recv_packet(&android_device_id).await?;
        if encrypted.is_identity() {
            server_cm
                .send_packet(&android_device_id, &identity.to_tcp_packet().unwrap())
                .await
        } else {
            Err(rust_connect::Error::InvalidPacket(format!(
                "expected encrypted identity, got {}",
                encrypted.packet_type
            )))
        }
    })
    .await;
    match exchange {
        Ok(Ok(())) => eprintln!("Encrypted identity exchange completed"),
        Ok(Err(e)) => panic!("identity exchange failed: {}", e),
        Err(_) => panic!("identity exchange timed out"),
    }

    // Pair (the phone must be paired before it accepts plugin packets).
    let pair_request = Packet::new(
        "kdeconnect.pair".to_string(),
        serde_json::json!({
            "pair": true,
            "timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        }),
    );
    // Send OUR pair request on the single inbound link; mutual initiation
    // auto-accepts when the phone's own pair request arrives.
    server_cm
        .send_packet(&android_device_id, &pair_request)
        .await
        .unwrap();
    eprintln!("Pair request sent on the inbound link.");
    eprintln!(
        ">>> On the phone, open USB Test Desktop and tap \"Request pairing\" — mutual auto-accept completes pairing (or tap Accept if the phone prompts)"
    );

    // Pairing phase: an already-paired phone IGNORES a duplicate pair
    // request (observed live vs a test phone), so a fresh pair=true is not the only
    // success signal — any plugin packet proves a live, paired link (the
    // same rule usb_full_protocol_handshake uses). Poll with short per-recv
    // timeouts inside an overall 60s budget; only fail when the budget
    // expires with neither signal.
    let mut link_live = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !link_live {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(
            remaining.min(Duration::from_secs(10)),
            server_cm.recv_packet(&android_device_id),
        )
        .await
        {
            Ok(Ok(packet)) if packet.packet_type == "kdeconnect.pair" => {
                let v = packet.body["pair"].as_bool().unwrap_or(false);
                if v {
                    link_live = true;
                    eprintln!("Pairing confirmed by Android (pair=true)");
                } else {
                    panic!("Pairing rejected by Android (pair=false) — tap Accept, not Reject");
                }
            }
            Ok(Ok(packet)) => {
                // Plugin traffic is only ever sent to paired devices.
                link_live = true;
                eprintln!(
                    "Plugin packet received ({}): phone is already paired, link is live",
                    packet.packet_type
                );
            }
            // A recv error (e.g. "Connection closed") is fatal on the
            // single link — fail fast instead of waiting out the budget.
            Ok(Err(e)) => panic!("recv failed (fatal on the single link): {}", e),
            Err(_) => {
                eprintln!(
                    "...still waiting for pair confirmation or plugin traffic (tap Accept on the phone if prompted)"
                );
            }
        }
    }
    assert!(
        link_live,
        "no pair confirmation and no plugin traffic within 60s — is the phone paired?"
    );

    // Paired — stop broadcasting and rejecting duplicate dials.
    broadcast_task.abort();
    reject_task.abort();

    // Build a 1 MiB patterned file.
    let content: Vec<u8> = (0..1_048_576u32).map(|i| (i % 253) as u8).collect();
    let src = std::env::temp_dir().join("rust-connect-usb-share-test.bin");
    std::fs::write(&src, &content).unwrap();

    let transfer = rust_connect::protocol::payload_transfer::PayloadTransfer::new(
        cert_manager.clone(),
        android_device_id.clone(),
    );
    let (transfer_info, send_handle) = transfer.send_file(&src).await.unwrap();

    let share_packet = Packet::new(
        "kdeconnect.share.request".to_string(),
        serde_json::json!({ "filename": "rust-connect-usb-share-test.bin" }),
    )
    .with_payload_size(content.len() as u64)
    .with_payload_transfer_info(serde_json::json!({
        "ip": transfer_info.ip,
        "port": transfer_info.port,
        "availableStreams": transfer_info.available_streams,
        "totalStreams": transfer_info.total_streams,
    }));
    server_cm
        .send_packet(&android_device_id, &share_packet)
        .await
        .unwrap();
    eprintln!(
        "Share request sent: {} bytes on port {}",
        content.len(),
        transfer_info.port
    );

    // The phone connects back over TLS and pulls the file.
    tokio::time::timeout(Duration::from_secs(60), send_handle)
        .await
        .expect("send task timed out — phone never pulled the payload")
        .expect("send task panicked")
        .expect("payload send failed");
    eprintln!(
        "Payload sent over TLS — check the phone's Downloads for rust-connect-usb-share-test.bin"
    );
}

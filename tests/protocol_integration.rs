//! Integration tests for protocol and device lifecycle flows

use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use rust_connect::device::{Device, DeviceRegistry, DeviceState, DeviceType};
use rust_connect::protocol::connection_loop::{run_packet_loop, LoopResult};
use rust_connect::protocol::{
    CertificateManager, ConnectionManager, Identity, Packet, PacketSerializer,
};
use rustls::pki_types::pem::PemObject;

fn create_test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    (state, temp_dir)
}

#[tokio::test]
async fn test_full_app_initialization() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
    let state = Arc::new(AppState::new_without_input(settings).unwrap());

    state.initialize().await.unwrap();

    let plugins = state.plugin_registry.list().await;
    assert!(!plugins.is_empty(), "should have plugins loaded");
    assert!(plugins.contains(&"ping".to_string()));
    assert!(plugins.contains(&"battery".to_string()));

    assert!(state.packet_router.has_handler("kdeconnect.ping").await);
    assert!(state.packet_router.has_handler("kdeconnect.battery").await);
    assert!(
        state
            .packet_router
            .has_handler("kdeconnect.notification")
            .await
    );

    let packet = Packet::ping();
    state.packet_router.route("test", packet).await.unwrap();
}

/// Conformance: the wire shape we emit must match what upstream peers send.
/// Both kdeconnect-kde (`core/deviceinfo.h:123-133 toIdentityPacket`,
/// `core/networkpacket.cpp:43-63 serialize`) and kdeconnect-android
/// (`NetworkPacket.kt` + `LanLinkProvider.kt:567` broadcast) write the same
/// top-level fields: `id` (number), `type` (string), `body` (object), with
/// `body` carrying `deviceId`, `deviceName`, `deviceType`, `protocolVersion`,
/// `tcpPort`, `incomingCapabilities`, `outgoingCapabilities`.
#[test]
fn test_identity_packet_format_matches_kde_connect() {
    use rust_connect::protocol::packet::PacketSerializer;
    use rust_connect::protocol::types::Identity;

    // Fixture: tests/fixtures/upstream-wire/identity/basic.json
    //   kdeconnect-kde@f5ed3ed8 core/deviceinfo.h:123-133
    //   kdeconnect-kde@f5ed3ed8 core/networkpacket.cpp:43-63
    //   kdeconnect-android@a88f6fa0 NetworkPacket.kt
    // Synthetic device id/name only — field names, casing, types come from
    // upstream.
    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/upstream-wire/identity/basic.json");
    let fixture_text = std::fs::read_to_string(&fixture_path).expect("read identity fixture");
    let fixture: serde_json::Value = serde_json::from_str(&fixture_text).expect("parse fixture");

    let id = Identity::new(
        "test_device_id_a".to_string(),
        "Test Device".to_string(),
        DeviceType::Desktop,
        vec!["kdeconnect.ping".to_string()],
        vec!["kdeconnect.ping".to_string()],
    );
    let packet = id.to_packet().unwrap();
    let bytes = PacketSerializer::serialize(&packet).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(std::str::from_utf8(&bytes).expect("utf-8 wire"))
            .expect("parse our wire");

    // Field-by-field: name, casing, type must all match upstream.
    assert_eq!(parsed["type"], fixture["type"]);
    assert_eq!(parsed["body"]["deviceId"], fixture["body"]["deviceId"]);
    assert_eq!(parsed["body"]["deviceName"], fixture["body"]["deviceName"]);
    assert_eq!(parsed["body"]["deviceType"], fixture["body"]["deviceType"]);
    assert_eq!(
        parsed["body"]["protocolVersion"],
        fixture["body"]["protocolVersion"]
    );
    assert_eq!(parsed["body"]["tcpPort"], fixture["body"]["tcpPort"]);
    assert_eq!(
        parsed["body"]["incomingCapabilities"],
        fixture["body"]["incomingCapabilities"]
    );
    assert_eq!(
        parsed["body"]["outgoingCapabilities"],
        fixture["body"]["outgoingCapabilities"]
    );
    assert!(
        parsed["id"].is_number(),
        "id must be a number per networkpacket.cpp:46"
    );

    // Top-level shape: id, type, body are the only required keys; our
    // packet has no payload so payloadSize / payloadTransferInfo are absent.
    let top = parsed.as_object().unwrap();
    assert!(top.contains_key("id"));
    assert!(top.contains_key("type"));
    assert!(top.contains_key("body"));
    assert!(!top.contains_key("payloadSize"));
    assert!(!top.contains_key("payloadTransferInfo"));
}

#[tokio::test]
async fn test_broadcast_roundtrip_between_two_services() {
    use rust_connect::device::types::DeviceType;
    use rust_connect::protocol::{DiscoveryService, Identity, PacketSerializer};
    use socket2::{Domain, Protocol, Socket, Type as SockType};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    let id = Identity::new(
        Identity::generate_device_id(),
        "Device A".to_string(),
        DeviceType::Desktop,
        vec!["kdeconnect.ping".to_string()],
        vec!["kdeconnect.ping".to_string()],
    );

    let port = 0u16;
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let socket = Socket::new(Domain::IPV4, SockType::DGRAM, Some(Protocol::UDP)).unwrap();
    socket.set_reuse_address(true).unwrap();
    socket.set_nonblocking(true).unwrap();
    socket.set_broadcast(true).unwrap();
    socket.bind(&addr.into()).unwrap();
    let std_socket: std::net::UdpSocket = socket.into();
    let udp = tokio::net::UdpSocket::from_std(std_socket).unwrap();

    let service1 = DiscoveryService {
        socket: udp,
        identity: id.clone(),
        broadcast_addr: "127.0.0.1:9".parse().unwrap(),
        broadcast_interval: tokio::time::Duration::from_secs(5),
    };

    let port2 = 0u16;
    let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port2);
    let socket2 = Socket::new(Domain::IPV4, SockType::DGRAM, Some(Protocol::UDP)).unwrap();
    socket2.set_reuse_address(true).unwrap();
    socket2.set_nonblocking(true).unwrap();
    socket2.set_broadcast(true).unwrap();
    socket2.bind(&addr2.into()).unwrap();
    let std_socket2: std::net::UdpSocket = socket2.into();
    let udp2 = tokio::net::UdpSocket::from_std(std_socket2).unwrap();

    let id2 = Identity::new(
        Identity::generate_device_id(),
        "Device B".to_string(),
        DeviceType::Desktop,
        vec!["kdeconnect.ping".to_string()],
        vec!["kdeconnect.ping".to_string()],
    );
    let service2 = DiscoveryService {
        socket: udp2,
        identity: id2,
        broadcast_addr: "127.0.0.1:9".parse().unwrap(),
        broadcast_interval: tokio::time::Duration::from_secs(5),
    };

    let service2_addr = service2.socket.local_addr().unwrap();
    let packet = id.to_packet().unwrap();
    let bytes = PacketSerializer::serialize(&packet).unwrap();
    service1
        .socket
        .send_to(&bytes, service2_addr)
        .await
        .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), service2.listen()).await;

    match result {
        Ok(Ok((identity, _addr))) => {
            assert_eq!(identity.device_name, "Device A");
            assert_eq!(identity.device_type, "desktop");
        }
        Ok(Err(e)) => {
            assert!(
                e.to_string().contains("ignored_own"),
                "Unexpected error: {}",
                e
            );
        }
        Err(_) => {
            panic!("Should have received a packet within 2 seconds");
        }
    }
}

#[tokio::test]
async fn test_device_lifecycle_flow() {
    let (state, _temp) = create_test_state();

    let device = Device::new(
        "test-device-1".to_string(),
        "Test Phone".to_string(),
        DeviceType::Phone,
        7,
    );

    state.registry.add(device.clone()).await.unwrap();
    assert!(state.registry.contains(&device.id).await);
    assert_eq!(state.registry.count().await, 1);

    state
        .lifecycle
        .transition(&device.id, DeviceState::Pairing)
        .await
        .unwrap();
    assert_eq!(
        state.lifecycle.get_state(&device.id).await.unwrap(),
        DeviceState::Pairing
    );

    state
        .lifecycle
        .transition(&device.id, DeviceState::Paired)
        .await
        .unwrap();
    state
        .lifecycle
        .transition(&device.id, DeviceState::Connected)
        .await
        .unwrap();

    let loaded = state.registry.get(&device.id).await.unwrap();
    assert!(loaded.is_paired());
    assert!(loaded.is_connected());
    assert!(loaded.paired_at.is_some());

    state
        .lifecycle
        .transition(&device.id, DeviceState::Disconnected)
        .await
        .unwrap();
    let loaded = state.registry.get(&device.id).await.unwrap();
    assert!(!loaded.is_connected());
}

#[tokio::test]
async fn test_event_broadcasting_during_lifecycle() {
    let (state, _temp) = create_test_state();
    let mut rx = state.broadcaster.subscribe();

    let device = Device::new(
        "event-test-device".to_string(),
        "Event Test".to_string(),
        DeviceType::Desktop,
        7,
    );

    state.registry.add(device).await.unwrap();
    state
        .lifecycle
        .transition(&"event-test-device".to_string(), DeviceState::Pairing)
        .await
        .unwrap();

    let event = rx.recv().await.unwrap();
    match event {
        rust_connect::DeviceEvent::StateChanged {
            device_id,
            old_state,
            new_state,
        } => {
            assert_eq!(device_id, "event-test-device");
            assert_eq!(old_state, DeviceState::Discovered);
            assert_eq!(new_state, DeviceState::Pairing);
        }
        _ => panic!("Expected StateChanged event"),
    }
}

#[tokio::test]
async fn test_device_registry_persistence() {
    use chrono::Utc;

    let temp_dir = tempfile::TempDir::new().unwrap();
    let path = temp_dir.path().join("devices.json");

    let registry = Arc::new(DeviceRegistry::with_persistence(path.clone()));

    let mut d1 = Device::new(
        "dev-1".to_string(),
        "Device 1".to_string(),
        DeviceType::Phone,
        7,
    );
    d1.state = DeviceState::Paired;
    d1.paired_at = Some(Utc::now());

    let mut d2 = Device::new(
        "dev-2".to_string(),
        "Device 2".to_string(),
        DeviceType::Desktop,
        7,
    );
    d2.state = DeviceState::Paired;
    d2.paired_at = Some(Utc::now());

    registry.add(d1).await.unwrap();
    registry.add(d2).await.unwrap();
    registry.save_to_disk().await.unwrap();

    let registry2 = Arc::new(DeviceRegistry::with_persistence(path));
    registry2.load_from_disk().await.unwrap();

    assert_eq!(registry2.count().await, 2);
    assert!(registry2.contains(&"dev-1".to_string()).await);
    assert!(registry2.contains(&"dev-2".to_string()).await);
}

#[tokio::test]
async fn test_connected_to_discovered_rejected() {
    let (state, _temp) = create_test_state();

    let device = Device::new(
        "test-device".to_string(),
        "Test".to_string(),
        DeviceType::Phone,
        7,
    );

    state.registry.add(device).await.unwrap();
    state
        .lifecycle
        .transition(&"test-device".to_string(), DeviceState::Connected)
        .await
        .unwrap();

    let result = state
        .lifecycle
        .transition(&"test-device".to_string(), DeviceState::Discovered)
        .await;
    assert!(result.is_err());
    assert_eq!(
        state
            .lifecycle
            .get_state(&"test-device".to_string())
            .await
            .unwrap(),
        DeviceState::Connected
    );
}

#[tokio::test]
async fn test_packet_serialization_roundtrip() {
    let packet = Packet::new(
        "kdeconnect.ping".to_string(),
        serde_json::json!({"timestamp": 12345}),
    );

    let bytes = PacketSerializer::serialize(&packet).unwrap();
    let deserialized = PacketSerializer::deserialize(&bytes).unwrap();

    assert_eq!(packet.packet_type, deserialized.packet_type);
    assert_eq!(packet.body, deserialized.body);
}

#[tokio::test]
async fn test_plugin_loading_and_routing() {
    let (state, _temp) = create_test_state();
    state.init_plugins().await;

    let plugins = state.plugin_registry.list().await;
    assert!(plugins.contains(&"ping".to_string()));
    assert!(plugins.contains(&"battery".to_string()));

    assert!(state.packet_router.has_handler("kdeconnect.ping").await);
    assert!(state.packet_router.has_handler("kdeconnect.battery").await);

    let packet = Packet::ping();
    state.packet_router.route("test", packet).await.unwrap();
}

#[tokio::test]
async fn test_connection_manager_tls_roundtrip() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().unwrap();

    let server_cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());
    let client_cm = ConnectionManager::new(cert_manager.clone()).unwrap();
    server_cm.set_device_identity("server-self-aaaaaaaaaaaaaaaaaaaa", "Test Server");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = server_cm.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        server
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .unwrap();
    });

    client_cm
        .connect(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .unwrap();
    server_handle.await.unwrap();

    let packet = Packet::ping();
    client_cm
        .send_packet(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &packet)
        .await
        .unwrap();

    let received = server_cm
        .recv_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .unwrap();
    assert_eq!(received.packet_type, "kdeconnect.ping");

    let gen = client_cm
        .get_generation(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .unwrap();
    client_cm
        .disconnect(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), gen)
        .await
        .unwrap();
    assert!(
        !client_cm
            .is_connected(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
    );
}

#[tokio::test]
async fn test_full_lifecycle_discover_pair_connect_disconnect() {
    let (state, _temp) = create_test_state();
    let mut rx = state.broadcaster.subscribe();
    state.init_plugins().await;

    let device_id = "lifecycle-device".to_string();

    let device = Device::new(
        device_id.clone(),
        "Lifecycle Test".to_string(),
        DeviceType::Phone,
        7,
    );

    state.registry.add(device).await.unwrap();
    assert_eq!(
        state.lifecycle.get_state(&device_id).await.unwrap(),
        DeviceState::Discovered
    );

    state
        .lifecycle
        .transition(&device_id, DeviceState::Pairing)
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    assert!(matches!(
        event,
        rust_connect::DeviceEvent::StateChanged {
            new_state: DeviceState::Pairing,
            ..
        }
    ));

    state
        .lifecycle
        .transition(&device_id, DeviceState::Paired)
        .await
        .unwrap();
    let loaded = state.registry.get(&device_id).await.unwrap();
    assert!(loaded.is_paired());
    assert!(loaded.paired_at.is_some());

    state
        .lifecycle
        .transition(&device_id, DeviceState::Connected)
        .await
        .unwrap();
    let loaded = state.registry.get(&device_id).await.unwrap();
    assert!(loaded.is_connected());

    let packet = Packet::ping();
    state.packet_router.route("test", packet).await.unwrap();

    state
        .lifecycle
        .transition(&device_id, DeviceState::Disconnected)
        .await
        .unwrap();
    assert!(!state.registry.get(&device_id).await.unwrap().is_connected());

    assert_eq!(state.registry.count().await, 1);
    assert!(state.registry.contains(&device_id).await);
    assert!(state.lifecycle.get_state(&device_id).await.unwrap() == DeviceState::Disconnected);

    state
        .lifecycle
        .transition(&device_id, DeviceState::Paired)
        .await
        .unwrap();
    state
        .lifecycle
        .transition(&device_id, DeviceState::Connected)
        .await
        .unwrap();
    assert!(state.registry.get(&device_id).await.unwrap().is_connected());
}

#[tokio::test]
async fn test_pairing_flow_via_state() {
    let (state, _temp) = create_test_state();

    let device_id = "pair-test-device".to_string();
    let device = Device::new(
        device_id.clone(),
        "Pair Test".to_string(),
        DeviceType::Desktop,
        7,
    );
    state.registry.add(device).await.unwrap();

    state
        .pairing_handler
        .initiate_pairing(&device_id)
        .await
        .unwrap();
    assert!(state.pairing_handler.has_pending_request(&device_id).await);

    state
        .pairing_handler
        .accept_pairing(&device_id)
        .await
        .unwrap();
    assert!(state.pairing_handler.is_paired(&device_id).await);
    assert!(state
        .pairing_handler
        .paired_since(&device_id)
        .await
        .is_some());

    state.pairing_handler.unpair(&device_id).await.unwrap();
    assert!(!state.pairing_handler.is_paired(&device_id).await);

    assert!(state.pairing_handler.paired_devices().await.is_empty());
}

#[tokio::test]
async fn test_multiple_devices_lifecycle() {
    let (state, _temp) = create_test_state();

    for i in 0..5 {
        let id = format!("multi-{}", i);
        let device = Device::new(id.clone(), format!("Device {}", i), DeviceType::Phone, 7);
        state.registry.add(device).await.unwrap();
        state
            .lifecycle
            .transition(&id, DeviceState::Pairing)
            .await
            .unwrap();
        state
            .lifecycle
            .transition(&id, DeviceState::Paired)
            .await
            .unwrap();
    }

    assert_eq!(state.registry.count().await, 5);
    let paired = state.registry.list_by_state(DeviceState::Paired).await;
    assert_eq!(paired.len(), 5);

    let discovered = state.registry.list_by_state(DeviceState::Discovered).await;
    assert!(discovered.is_empty());
}

#[tokio::test]
async fn test_connection_replacement() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
    let state = Arc::new(AppState::new_without_input(settings).unwrap());

    let cm = state.connection_manager.clone();
    cm.set_device_identity("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client");
    state
        .cert_manager
        .ensure_certificate("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client")
        .unwrap();

    let server_cm = Arc::new(ConnectionManager::new(state.cert_manager.clone()).unwrap());
    server_cm.set_device_identity("serveraaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Server");
    state
        .cert_manager
        .ensure_certificate("serveraaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Server")
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = server_cm.clone();
    let server_handle = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            server
                .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
                .await
                .unwrap();
        }
    });

    cm.connect(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .unwrap();
    assert!(
        cm.is_connected(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
    );

    let handle = cm
        .get_connection_info(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await;
    assert!(handle.is_some());
    let first_conn_time = handle.unwrap().connected_at;

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    cm.connect(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .unwrap();
    assert!(
        cm.is_connected(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            .await
    );

    let handle = cm
        .get_connection_info(&"serveraaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await;
    assert!(handle.is_some());
    let second_conn_time = handle.unwrap().connected_at;

    assert!(
        second_conn_time > first_conn_time,
        "Second connection should be newer than first"
    );

    server_handle.abort();
}

async fn setup_peer_pair(
    state: &Arc<AppState>,
) -> (Arc<ConnectionManager>, tokio::task::JoinHandle<()>, u64) {
    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let cert_manager2 = Arc::new(CertificateManager::new(temp_dir2.path().to_path_buf()));
    cert_manager2.init().unwrap();

    let cm2 = Arc::new(ConnectionManager::new(cert_manager2.clone()).unwrap());
    cm2.set_device_identity("peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Peer");
    cert_manager2
        .ensure_certificate("peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Peer")
        .unwrap();
    state
        .cert_manager
        .ensure_certificate("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test")
        .unwrap();
    cert_manager2
        .ensure_certificate("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test")
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_cm = cm2.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        server_cm
            .accept_test("clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), stream)
            .await
            .unwrap();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    state
        .connection_manager
        .set_device_identity("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test");
    let _client_generation = state
        .connection_manager
        .connect(&"peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), addr)
        .await
        .unwrap();

    (cm2, server_handle, _client_generation)
}

/// Spawn the REAL packet loop for the link set up by `setup_peer_pair` and
/// return its cancel token and handle. The pair-flow tests below drive this
/// — not a local reimplementation — so they certify `run_packet_loop`
/// itself (the old `run_pair_loop_once` shadow had drifted to the pre-P4
/// semantics and certified behavior the real loop no longer has).
fn spawn_real_packet_loop(
    state: &Arc<AppState>,
    device_id: &str,
    generation: u64,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<LoopResult>,
) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let state = state.clone();
    let device_id = device_id.to_owned();
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_packet_loop(state, loop_cancel, &device_id, generation, 0).await
    });
    (cancel, handle)
}

/// Poll an async condition until it holds or ~2s elapse.
async fn wait_until<F, Fut>(mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..80 {
        if cond().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn test_connection_loop_pair_packet_auto_accepts_when_pending() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    state
        .pairing_handler
        .initiate_pairing(&device_id)
        .await
        .unwrap();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    let pair_response = Packet::pair_response(true);
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &pair_response,
        )
        .await
        .unwrap();

    let paired = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.is_paired(&device_id).await }
    })
    .await;
    assert!(
        paired,
        "Real loop must auto-accept pair=true when an outgoing request is pending"
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

/// Pairing completion on an established connection must emit plugin
/// init packets — here through the loop's we-initiated accept branch (the
/// mutual race branch and the REST accept branch share the same helper).
#[tokio::test]
async fn test_connection_loop_pair_accept_sends_plugin_init_packets() {
    let (state, _temp) = create_test_state();
    state.initialize().await.unwrap();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    state
        .pairing_handler
        .initiate_pairing(&device_id)
        .await
        .unwrap();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    let pair_response = Packet::pair_response(true);
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &pair_response,
        )
        .await
        .unwrap();

    let paired = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.is_paired(&device_id).await }
    })
    .await;
    assert!(paired, "fixture: loop must complete the pairing");

    // Plugin init advertisements must arrive on the peer side of the link
    // after the pairing completed. Several plugins advertise; read until
    // runcommand's shows up.
    //
    // One generous deadline rather than per-window timeouts: the old 8x5s
    // loop treated an empty window as a reason to stop, so under full-suite
    // parallel load a single slow packet failed the test (measured at ~10%
    // of full-suite runs). Drain until the deadline; only a link error stops
    // the wait early.
    let mut saw_runcommand = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(
            remaining.min(std::time::Duration::from_secs(5)),
            peer_cm.recv_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
        )
        .await
        {
            Ok(Ok(pkt)) if pkt.packet_type == "kdeconnect.runcommand" => {
                saw_runcommand = true;
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert!(
        saw_runcommand,
        "plugin init packets must follow a completed pairing"
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_pair_packet_stores_incoming() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    let pair_response = Packet::pair_response(true);
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &pair_response,
        )
        .await
        .unwrap();

    let stored = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.has_incoming_request(&device_id).await }
    })
    .await;
    assert!(
        stored,
        "Incoming request should be stored, not auto-accepted"
    );
    assert!(
        !state.pairing_handler.is_paired(&device_id).await,
        "Should NOT be paired — incoming request should be stored, not auto-accepted"
    );
    assert_eq!(
        state.pairing_handler.pair_state(&device_id).await,
        rust_connect::protocol::PairState::RequestedByPeer
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_pair_reject_clears_pending() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    state
        .pairing_handler
        .initiate_pairing(&device_id)
        .await
        .unwrap();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    let pair_reject = Packet::pair_response(false);
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &pair_reject,
        )
        .await
        .unwrap();

    let cleared = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { !state.pairing_handler.has_pending_request(&device_id).await }
    })
    .await;
    assert!(cleared, "Pending request should be cleared after rejection");
    assert!(
        !state.pairing_handler.is_paired(&device_id).await,
        "Should not be paired after rejection"
    );

    // A rejection while a pairing is pending drops the link (Android
    // behavior), not just the state.
    let loop_exited = tokio::time::timeout(std::time::Duration::from_secs(5), loop_handle).await;
    assert!(
        loop_exited.is_ok(),
        "the packet loop must disconnect after a pair rejection"
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

/// Android PairingHandler.kt validates the 1800s window only on pair=true:
/// a pair=false with a stale timestamp must STILL unpair and drop the link
/// (a clock-skewed peer's unpair is processed by Android). Regression test
/// for the gate applying to every pair packet.
#[tokio::test]
async fn test_connection_loop_stale_pair_false_still_unpairs() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    state
        .pairing_handler
        .receive_pair_request(&device_id, Some(1_700_000_000))
        .await
        .unwrap();
    state
        .pairing_handler
        .accept_pairing(&device_id)
        .await
        .unwrap();
    assert!(state.pairing_handler.is_paired(&device_id).await);

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    // pair=false dated an hour ago: outside the window, but the window must
    // not apply to it.
    let stale_unpair = Packet::new(
        "kdeconnect.pair".to_string(),
        serde_json::json!({
            "pair": false,
            "timestamp": chrono::Utc::now().timestamp() - 3600
        }),
    );
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &stale_unpair,
        )
        .await
        .unwrap();

    let unpaired = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { !state.pairing_handler.is_paired(&device_id).await }
    })
    .await;
    assert!(
        unpaired,
        "a stale pair=false must still unpair (Android validates timestamps only on pair=true)"
    );

    let loop_exited = tokio::time::timeout(std::time::Duration::from_secs(5), loop_handle).await;
    assert!(
        loop_exited.is_ok(),
        "the packet loop must disconnect after an unpair"
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

/// Android PairingHandler.kt:100: an unpair packet for a device we never
/// paired is ignored — no state change, and the link stays up.
#[tokio::test]
async fn test_connection_loop_unpair_while_not_paired_is_ignored() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, mut loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    let pair_reject = Packet::pair_response(false);
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &pair_reject,
        )
        .await
        .unwrap();

    // Give the loop a moment to (not) act: it must still be running, and a
    // ping right behind the pair=false must still be routable on the link.
    let exited =
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut loop_handle).await;
    assert!(
        exited.is_err(),
        "an unpair for a never-paired device must not drop the link"
    );
    assert_eq!(
        state.pairing_handler.pair_state(&device_id).await,
        rust_connect::protocol::PairState::NotPaired
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

/// Android PairingHandler.kt:60-68: a pair request from an already-paired
/// device unpairs BOTH sides and starts over as a fresh incoming request.
/// The old version of this test asserted "still paired after
/// reconfirmation" — the pre-P4 defect — against a shadow loop; this drives
/// the real loop and asserts the current, correct semantics.
#[tokio::test]
async fn test_connection_loop_pair_while_paired_unpairs_and_starts_over() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    state
        .pairing_handler
        .receive_pair_request(&device_id, Some(1_700_000_000))
        .await
        .unwrap();
    state
        .pairing_handler
        .accept_pairing(&device_id)
        .await
        .unwrap();

    assert!(state.pairing_handler.is_paired(&device_id).await);

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    let pair_resp = Packet::pair_response(true);
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &pair_resp)
        .await
        .unwrap();

    let fresh_request = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.has_incoming_request(&device_id).await }
    })
    .await;
    assert!(
        fresh_request,
        "Pair-while-paired must register a fresh incoming request"
    );
    assert!(
        !state.pairing_handler.is_paired(&device_id).await,
        "Pair-while-paired must unpair (Android semantics), not silently reconfirm"
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

/// The 1800s pair-timestamp window applies to incoming pair REQUESTS from
/// v8+ peers (PairingHandler.kt:71-83), driven through the real loop: a
/// request whose timestamp is outside the window is skipped (the
/// `pair_timestamp_rejected` continue), while a fresh one on the same link
/// is stored. (Accepts in Requested state carry no such check — upstream
/// pairingDone() is unconditional there.)
#[tokio::test]
async fn test_connection_loop_stale_pair_timestamp_ignored_fresh_accepted() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    // Stale (one hour old): outside the 1800s window — the loop must skip it.
    let stale = Packet::pair_request_with_timestamp(chrono::Utc::now().timestamp() - 3600);
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &stale)
        .await
        .unwrap();

    let stored_on_stale = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.has_incoming_request(&device_id).await }
    })
    .await;
    assert!(
        !stored_on_stale,
        "a pair request with a stale timestamp must not register an incoming request"
    );
    assert_eq!(
        state.pairing_handler.pair_state(&device_id).await,
        rust_connect::protocol::PairState::NotPaired
    );

    // Fresh: inside the window — the loop must store the request.
    let fresh = Packet::pair_request_with_timestamp(chrono::Utc::now().timestamp());
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &fresh)
        .await
        .unwrap();

    let stored = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.has_incoming_request(&device_id).await }
    })
    .await;
    assert!(stored, "a fresh pair request must be stored by the loop");
    assert_eq!(
        state.pairing_handler.pair_state(&device_id).await,
        rust_connect::protocol::PairState::RequestedByPeer
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

/// Upstream accept semantics: an accept of OUR outgoing pairing request is
/// `{"pair": true}` with NO timestamp (PairingHandler.kt acceptPairing()
/// sends none), and packetReceived handles PairState.Requested with an
/// unconditional pairingDone() — a timestamp-less accept must COMPLETE the
/// pairing. (The v8 timestamp is required only on incoming pair REQUESTS
/// from protocol >= 8 peers.)
#[tokio::test]
async fn test_connection_loop_pair_true_without_timestamp_accept_completes() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    state
        .pairing_handler
        .initiate_pairing(&device_id)
        .await
        .unwrap();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    // Timestamp-less accept — exactly what a real Android phone or
    // kdeconnect-kde desktop sends — must complete the pairing.
    let no_ts = Packet::new(
        "kdeconnect.pair".to_string(),
        serde_json::json!({ "pair": true }),
    );
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &no_ts)
        .await
        .unwrap();

    let paired = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.is_paired(&device_id).await }
    })
    .await;
    assert!(
        paired,
        "a timestamp-less pair accept must complete the pairing (upstream acceptPairing sends no timestamp)"
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

/// The window is two-sided (the loop checks `(ts - now).abs()`): a
/// future-dated pair request is skipped too — no incoming request is
/// stored — and a fresh request right behind it lands.
#[tokio::test]
async fn test_connection_loop_future_pair_timestamp_ignored_fresh_stored() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    // Future-dated by an hour: outside the window on the other side.
    let stale = Packet::pair_request_with_timestamp(chrono::Utc::now().timestamp() + 3600);
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &stale)
        .await
        .unwrap();

    let stored_on_stale = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.has_incoming_request(&device_id).await }
    })
    .await;
    assert!(
        !stored_on_stale,
        "a future-dated pair request must not register an incoming request"
    );
    assert_eq!(
        state.pairing_handler.pair_state(&device_id).await,
        rust_connect::protocol::PairState::NotPaired
    );

    let fresh = Packet::pair_request_with_timestamp(chrono::Utc::now().timestamp());
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &fresh)
        .await
        .unwrap();

    let stored = wait_until(|| {
        let state = state.clone();
        let device_id = device_id.clone();
        async move { state.pairing_handler.has_incoming_request(&device_id).await }
    })
    .await;
    assert!(stored, "a fresh pair request must be stored by the loop");
    assert_eq!(
        state.pairing_handler.pair_state(&device_id).await,
        rust_connect::protocol::PairState::RequestedByPeer
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_non_pair_packet_is_routed() {
    let (state, _temp) = create_test_state();
    state.init_plugins().await;

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (peer_cm, server_handle, _generation) = setup_peer_pair(&state).await;

    let cm = state.connection_manager.clone();
    let router = state.packet_router.clone();
    let device_id_clone = device_id.clone();
    let loop_handle = tokio::spawn(async move {
        tokio::select! {
            result = cm.recv_packet(&device_id_clone) => {
                let Ok(packet) = result else { return };
                if !packet.is_pair() {
                    let _ = router.route(&device_id_clone, packet).await;
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
        }
    });

    let ping = Packet::ping();
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &ping)
        .await
        .unwrap();

    loop_handle.await.unwrap();

    state
        .connection_manager
        .disconnect(&device_id, _generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_drops_plugin_packet_from_unpaired_peer() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (state, _temp) = create_test_state();
    let routed = Arc::new(AtomicUsize::new(0));
    let routed_for_handler = routed.clone();
    state
        .packet_router
        .register("kdeconnect.ping", move |_device_id, _packet| {
            let routed = routed_for_handler.clone();
            async move {
                routed.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }
        })
        .await;

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &Packet::ping(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert_eq!(
        routed.load(Ordering::SeqCst),
        0,
        "an unpaired TLS peer must not reach plugin packet handlers"
    );

    cancel.cancel();
    let _ = loop_handle.await;
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_survives_panicking_plugin() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (state, _temp) = create_test_state();

    // A deliberately panicking handler plus a counting sibling on the same
    // packet type: the panic must be contained, the sibling must run, and
    // the link must stay up.
    let routed = Arc::new(AtomicUsize::new(0));
    let routed_for_handler = routed.clone();
    state
        .packet_router
        .register("kdeconnect.ping", move |_device_id, _packet| {
            let routed = routed_for_handler.clone();
            async move {
                routed.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }
        })
        .await;
    state
        .packet_router
        .register("kdeconnect.ping", |_device_id, _packet| async move {
            panic!("deliberate test panic");
        })
        .await;

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    // Non-pair packets from an unpaired peer are dropped before routing.
    state
        .pairing_handler
        .force_accept_pairing(&device_id)
        .await
        .unwrap();

    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &Packet::ping(),
        )
        .await
        .unwrap();

    let sibling_ran = wait_until(|| {
        let routed = routed.clone();
        async move { routed.load(Ordering::SeqCst) > 0 }
    })
    .await;
    assert!(
        sibling_ran,
        "the healthy sibling must run despite the panicking handler"
    );
    assert!(
        state.connection_manager.is_connected(&device_id).await,
        "a panicking plugin must not sever the connection"
    );
    assert!(
        !loop_handle.is_finished(),
        "the packet loop must survive a plugin panic"
    );

    cancel.cancel();
    let _ = loop_handle.await;
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_returns_disconnected_on_recv_error() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;

    let conn_cancel = tokio_util::sync::CancellationToken::new();
    let state_clone = state.clone();
    let device_id_clone = device_id.clone();
    let loop_handle = tokio::spawn(async move {
        run_packet_loop(state_clone, conn_cancel, &device_id_clone, generation, 0).await
    });

    peer_cm
        .disconnect(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            peer_cm
                .get_generation(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
                .await
                .unwrap(),
        )
        .await
        .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), loop_handle).await;
    assert!(
        result.is_ok(),
        "packet loop should finish after peer disconnect"
    );
    assert!(result.is_ok_and(|inner| matches!(inner.unwrap(), LoopResult::Disconnected)));

    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_cancel_token_returns_disconnected() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (_peer_cm, server_handle, generation) = setup_peer_pair(&state).await;

    let conn_cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = conn_cancel.clone();
    let state_clone = state.clone();
    let device_id_clone = device_id.clone();
    let loop_handle = tokio::spawn(async move {
        run_packet_loop(state_clone, cancel_clone, &device_id_clone, generation, 0).await
    });

    conn_cancel.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), loop_handle).await;
    assert!(result.is_ok(), "packet loop should finish on cancel");
    assert!(result.is_ok_and(|inner| matches!(inner.unwrap(), LoopResult::Disconnected)));

    server_handle.abort();
}

#[tokio::test]
async fn test_connection_loop_shutdown_returns_shutdown() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (_peer_cm, server_handle, generation) = setup_peer_pair(&state).await;

    let conn_cancel = tokio_util::sync::CancellationToken::new();
    let state_clone = state.clone();
    let device_id_clone = device_id.clone();
    let loop_handle = tokio::spawn(async move {
        run_packet_loop(state_clone, conn_cancel, &device_id_clone, generation, 0).await
    });

    state.shutdown.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), loop_handle).await;
    assert!(result.is_ok(), "packet loop should finish on shutdown");
    assert!(result.is_ok_and(|inner| matches!(inner.unwrap(), LoopResult::Shutdown)));

    server_handle.abort();
}

fn make_identity(device_id: &str, device_name: &str) -> Identity {
    Identity::new(
        device_id.to_string(),
        device_name.to_string(),
        DeviceType::Phone,
        vec![],
        vec![],
    )
}

#[tokio::test]
async fn test_identity_exchange_valid() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (peer_cm, server_handle, _generation) = setup_peer_pair(&state).await;

    let identity = make_identity("peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Peer");
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &identity.to_packet().unwrap(),
        )
        .await
        .unwrap();

    let cm = state.connection_manager.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        cm.recv_packet(&device_id),
    )
    .await;

    assert!(result.is_ok(), "Should receive identity packet");
    let packet = result.unwrap().unwrap();
    assert!(packet.is_identity(), "Packet should be an identity packet");

    let received = Identity::from_packet(packet).unwrap();
    assert_eq!(received.device_id, "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    state
        .connection_manager
        .disconnect(&device_id, _generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_identity_exchange_mismatch_disconnects() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (peer_cm, server_handle, _generation) = setup_peer_pair(&state).await;

    let identity = make_identity("wrong-device-idaaaaaaaaaaaaaaaaa", "Test Peer");
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &identity.to_packet().unwrap(),
        )
        .await
        .unwrap();

    let cm = state.connection_manager.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        cm.recv_packet(&device_id),
    )
    .await;

    assert!(result.is_ok(), "Should receive packet");
    let packet = result.unwrap().unwrap();
    assert!(packet.is_identity(), "Packet should be identity");

    let received = Identity::from_packet(packet.clone()).unwrap();
    assert_eq!(
        received.device_id, "wrong-device-idaaaaaaaaaaaaaaaaa",
        "Encrypted identity deviceId should be different"
    );

    state
        .connection_manager
        .disconnect(&device_id, _generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_identity_exchange_non_identity_packet_rejects() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (peer_cm, server_handle, _generation) = setup_peer_pair(&state).await;

    let ping = Packet::ping();
    peer_cm
        .send_packet(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), &ping)
        .await
        .unwrap();

    let cm = state.connection_manager.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        cm.recv_packet(&device_id),
    )
    .await;

    assert!(result.is_ok(), "Should receive packet");
    let packet = result.unwrap().unwrap();
    assert!(
        !packet.is_identity(),
        "Packet should NOT be identity — it's a ping"
    );
    assert!(packet.packet_type == "kdeconnect.ping");

    state
        .connection_manager
        .disconnect(&device_id, _generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_identity_exchange_pair_request_for_unpaired() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();

    assert!(
        !state.pairing_handler.is_paired(&device_id).await,
        "Should start unpaired"
    );

    let (peer_cm, server_handle, _generation) = setup_peer_pair(&state).await;

    let identity = make_identity("peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Peer");
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &identity.to_packet().unwrap(),
        )
        .await
        .unwrap();

    let cm = state.connection_manager.clone();
    let pairing = state.pairing_handler.clone();
    let device_id_clone = device_id.clone();
    let our_identity = make_identity("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client");
    let loop_handle = tokio::spawn(async move {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cm.recv_packet(&device_id_clone),
        )
        .await;
        if let Ok(Ok(pkt)) = result {
            if pkt.is_identity() {
                let _ = cm
                    .send_packet(&device_id_clone, &our_identity.to_packet().unwrap())
                    .await;
                if !pairing.is_paired(&device_id_clone).await {
                    let _ = pairing.initiate_pairing(&device_id_clone).await;
                    let _ = cm
                        .send_packet(&device_id_clone, &Packet::pair_request())
                        .await;
                }
            }
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        state.pairing_handler.has_pending_request(&device_id).await,
        "Should have pending outgoing pair request for unpaired device"
    );

    loop_handle.await.unwrap();
    state
        .connection_manager
        .disconnect(&device_id, _generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_identity_exchange_no_pair_request_for_paired() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    state
        .pairing_handler
        .receive_pair_request(&device_id, Some(1_700_000_000))
        .await
        .unwrap();
    state
        .pairing_handler
        .accept_pairing(&device_id)
        .await
        .unwrap();
    assert!(state.pairing_handler.is_paired(&device_id).await);

    let (peer_cm, server_handle, _generation) = setup_peer_pair(&state).await;

    let identity = make_identity("peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Peer");
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &identity.to_packet().unwrap(),
        )
        .await
        .unwrap();

    let cm = state.connection_manager.clone();
    let pairing = state.pairing_handler.clone();
    let device_id_clone = device_id.clone();
    let our_identity = make_identity("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client");
    let loop_handle = tokio::spawn(async move {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cm.recv_packet(&device_id_clone),
        )
        .await;
        if let Ok(Ok(pkt)) = result {
            if pkt.is_identity() {
                let _ = cm
                    .send_packet(&device_id_clone, &our_identity.to_packet().unwrap())
                    .await;
                if !pairing.is_paired(&device_id_clone).await {
                    let _ = pairing.initiate_pairing(&device_id_clone).await;
                    let _ = cm
                        .send_packet(&device_id_clone, &Packet::pair_request())
                        .await;
                }
            }
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !state.pairing_handler.has_pending_request(&device_id).await,
        "Should NOT have pending pair request for already-paired device"
    );

    loop_handle.await.unwrap();
    state
        .connection_manager
        .disconnect(&device_id, _generation)
        .await
        .unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_identity_exchange_send_failure_disconnects() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let (peer_cm, server_handle, _generation) = setup_peer_pair(&state).await;

    let identity = make_identity("peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Peer");
    peer_cm
        .send_packet(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            &identity.to_packet().unwrap(),
        )
        .await
        .unwrap();

    peer_cm
        .disconnect(
            &"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            peer_cm
                .get_generation(&"clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
                .await
                .unwrap(),
        )
        .await
        .unwrap();

    let cm = state.connection_manager.clone();
    let our_identity = make_identity("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client");
    let device_id_clone = device_id.clone();
    let loop_handle = tokio::spawn(async move {
        let result = cm.recv_packet(&device_id_clone).await;
        if result.is_ok() {
            let send_result = cm
                .send_packet(&device_id_clone, &our_identity.to_packet().unwrap())
                .await;
            assert!(
                send_result.is_err(),
                "Send should fail after peer disconnect"
            );
        }
    });

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), loop_handle).await;
    assert!(result.is_ok(), "Identity exchange task should complete");

    server_handle.abort();
}

#[tokio::test]
async fn test_daemon_identity_persists_across_restarts() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let data_dir = temp_dir.path().to_path_buf();

    let device_id_path = data_dir.join("device_id");

    assert!(
        !device_id_path.exists(),
        "device_id file should not exist yet"
    );

    let settings = AppSettings::new_with_data_dir(data_dir.clone());
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    state.initialize().await.unwrap();

    state
        .cert_manager
        .ensure_certificate("test-id-123aaaaaaaaaaaaaaaaaaaaa", "Test Device")
        .unwrap();
    state
        .connection_manager
        .set_device_identity("test-id-123aaaaaaaaaaaaaaaaaaaaa", "Test Device");
    std::fs::write(&device_id_path, "test-id-123aaaaaaaaaaaaaaaaaaaaa").unwrap();

    assert!(device_id_path.exists(), "device_id file should exist now");

    let settings2 = AppSettings::new_with_data_dir(data_dir.clone());
    let state2 = Arc::new(AppState::new_without_input(settings2).unwrap());
    state2.initialize().await.unwrap();

    let loaded_id = std::fs::read_to_string(&device_id_path).unwrap();
    assert_eq!(
        loaded_id, "test-id-123aaaaaaaaaaaaaaaaaaaaa",
        "Device ID should persist across restarts"
    );

    state2
        .connection_manager
        .set_device_identity(&loaded_id, "Test Device");
    assert!(
        state2.cert_manager.has_certificate(&loaded_id),
        "Certificate should exist for persisted device ID"
    );
}

#[derive(Debug)]
struct AcceptAnyServerCert {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn make_tokio_tls_connector() -> tokio_rustls::TlsConnector {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

fn make_tokio_tls_acceptor(
    cert_manager: &CertificateManager,
    device_id: &str,
) -> tokio_rustls::TlsAcceptor {
    let (cert_pem, key_pem) = cert_manager.load_certificate(device_id).unwrap();
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_slice_iter(&cert_pem)
            .collect::<Result<_, _>>()
            .unwrap();
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(&key_pem).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS12])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

fn test_server_name() -> rustls::pki_types::ServerName<'static> {
    rustls::pki_types::ServerName::try_from("kdeconnect")
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn test_accept_incoming_full_path() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().unwrap();

    let cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());

    let remote_id = "remote-peeraaaaaaaaaaaaaaaaaaaaa";
    let remote_name = "Remote Peer";
    cert_manager
        .ensure_certificate(remote_id, remote_name)
        .unwrap();
    cert_manager
        .ensure_certificate("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client")
        .unwrap();
    cm.set_device_identity("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client");

    let remote_identity = make_identity(remote_id, remote_name);
    let identity_bytes =
        PacketSerializer::serialize(&remote_identity.to_packet().unwrap()).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let acceptor = make_tokio_tls_acceptor(&cert_manager, remote_id);

    let remote_handle = tokio::spawn(async move {
        let (tcp_stream, _) = listener.accept().await.unwrap();

        let mut tcp_stream = tcp_stream;
        use tokio::io::AsyncWriteExt;
        tcp_stream.write_all(&identity_bytes).await.unwrap();
        tcp_stream.flush().await.unwrap();

        let mut tls_stream = acceptor.accept(tcp_stream).await.unwrap();

        let mut line = Vec::new();
        tokio::io::BufReader::new(&mut tls_stream)
            .read_until(b'\n', &mut line)
            .await
            .unwrap();
        let packet = PacketSerializer::deserialize(&line).unwrap();
        assert!(packet.is_identity());
    });

    let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let accept_res = cm.accept_incoming(tcp_stream).await.unwrap();
    let (device_id, remote_id_result, _generation) = accept_res;

    assert_eq!(device_id, remote_id);
    assert_eq!(remote_id_result.device_id, remote_id);
    assert_eq!(remote_id_result.device_name, remote_name);
    assert!(cm.is_connected(&device_id).await);

    cm.disconnect(&device_id, _generation).await.unwrap();
    remote_handle.abort();
}

#[tokio::test]
async fn test_accept_as_client_full_path() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().unwrap();

    let cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());

    let remote_id = "remote-serveraaaaaaaaaaaaaaaaaaa";
    cert_manager
        .ensure_certificate(remote_id, "Remote")
        .unwrap();
    cm.set_device_identity("clientaaaaaaaaaaaaaaaaaaaaaaaaaa", "Test Client");

    let acceptor = make_tokio_tls_acceptor(&cert_manager, remote_id);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let remote_handle = tokio::spawn(async move {
        let (tcp_stream, _) = listener.accept().await.unwrap();
        let _tls = acceptor.accept(tcp_stream).await.unwrap();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let _generation = cm
        .accept_as_client(remote_id.to_string(), tcp_stream)
        .await
        .unwrap();

    assert!(cm.is_connected(&remote_id.to_string()).await);

    cm.disconnect(&remote_id.to_string(), _generation)
        .await
        .unwrap();
    remote_handle.abort();
}

#[tokio::test]
async fn test_connect_to_device_full_path() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().unwrap();

    let cm = Arc::new(ConnectionManager::new(cert_manager.clone()).unwrap());

    let our_id = "our-deviceaaaaaaaaaaaaaaaaaaaaaa";
    let remote_id = "remote-deviceaaaaaaaaaaaaaaaaaaa";
    cert_manager
        .ensure_certificate(our_id, "Our Device")
        .unwrap();
    cert_manager
        .ensure_certificate(remote_id, "Remote Device")
        .unwrap();

    std::fs::write(temp_dir.path().join("device_id"), our_id).unwrap();
    cm.set_device_identity(our_id, "Our Device");

    let our_identity = make_identity(our_id, "Our Device");
    let remote_identity = make_identity(remote_id, "Remote Device");

    let connector = make_tokio_tls_connector();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let remote_handle = tokio::spawn(async move {
        let (tcp_stream, _) = listener.accept().await.unwrap();

        let mut buf_reader = tokio::io::BufReader::new(tcp_stream);
        let mut line = Vec::new();
        buf_reader.read_until(b'\n', &mut line).await.unwrap();
        let packet = PacketSerializer::deserialize(&line).unwrap();
        assert!(packet.is_identity());
        let parsed = Identity::from_packet(packet).unwrap();
        assert_eq!(parsed.device_id, our_id);

        let tcp_stream = buf_reader.into_inner();
        let tls_stream = connector
            .connect(test_server_name(), tcp_stream)
            .await
            .unwrap();

        let mut tls_reader = tokio::io::BufReader::new(tls_stream);

        let mut line = Vec::new();
        tls_reader.read_until(b'\n', &mut line).await.unwrap();
        let enc_packet = PacketSerializer::deserialize(&line).unwrap();
        assert!(enc_packet.is_identity());

        let resp = remote_identity.to_packet().unwrap();
        let resp_bytes = PacketSerializer::serialize(&resp).unwrap();
        use tokio::io::AsyncWriteExt;
        tls_reader.get_mut().write_all(&resp_bytes).await.unwrap();
        tls_reader.get_mut().flush().await.unwrap();

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    let (device_id, returned_identity, _generation) = cm
        .connect_to_device(&our_identity, addr, None)
        .await
        .unwrap();

    assert_eq!(device_id, remote_id);
    assert_eq!(returned_identity.device_id, remote_id);
    assert!(cm.is_connected(&device_id).await);

    cm.disconnect(&device_id, _generation).await.unwrap();
    remote_handle.abort();
}

/// Parity gap 1 (docs/parity-checklist.md): an unpaired peer's
/// non-pair packets are dropped — but the peer must be TOLD, once, with
/// {pair:false} (kde device.cpp:391-394; Android auto-unpairs). Otherwise
/// the phone keeps the link and keeps believing it's paired (unpair desync).
#[tokio::test]
async fn test_connection_loop_unpaired_peer_is_told_pair_false_once() {
    let (state, _temp) = create_test_state();

    let device_id = "peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    // Deliberately NOT paired: the device is linked but unknown to the
    // pairing handler.
    let (peer_cm, server_handle, generation) = setup_peer_pair(&state).await;
    let (cancel, _loop_handle) = spawn_real_packet_loop(&state, &device_id, generation);

    let our_id = "clientaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    peer_cm.send_packet(&our_id, &Packet::ping()).await.unwrap();

    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        peer_cm.recv_packet(&our_id),
    )
    .await
    .expect("an unpaired peer must be told {pair:false}")
    .expect("reply must be a valid packet");
    assert!(
        reply.is_pair(),
        "the reply must be a pair packet, got {}",
        reply.packet_type
    );
    assert_eq!(
        reply.body.get("pair").and_then(|v| v.as_bool()),
        Some(false),
        "the reply must be pair:false"
    );

    // Once per unpaired stretch — a second non-pair packet draws no reply.
    peer_cm.send_packet(&our_id, &Packet::ping()).await.unwrap();
    let second = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        peer_cm.recv_packet(&our_id),
    )
    .await;
    assert!(
        second.is_err(),
        "pair:false must be sent once, not per packet, got {second:?}"
    );

    cancel.cancel();
    state
        .connection_manager
        .disconnect(&device_id, generation)
        .await
        .unwrap();
    server_handle.abort();
}

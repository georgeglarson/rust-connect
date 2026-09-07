//! Service manager
//!
//! Single Responsibility: Start and stop long-running services (discovery, TCP listener, API).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::app::AppState;
use crate::device::{Device, DeviceType};
use crate::protocol::{DiscoveryService, Identity};
use crate::utils::Result;

/// Handles for all running services.
pub struct ServiceHandles {
    pub discovery: DiscoveryHandles,
    pub tcp: Option<tokio::task::JoinHandle<()>>,
    pub api: Option<tokio::task::JoinHandle<()>>,
}

/// Handles for the discovery service.
pub struct DiscoveryHandles {
    pub listen_handle: tokio::task::JoinHandle<()>,
    /// mDNS announce + browse (None when the mDNS daemon failed to start —
    /// UDP broadcast discovery still stands).
    pub mdns_handle: Option<tokio::task::JoinHandle<()>>,
    /// Network-change reactor (Task 2.2, vk #994): watches for interface/
    /// address changes and suspend/resume, re-announcing on each debounced
    /// event.
    pub network_change_handle: tokio::task::JoinHandle<()>,
    /// Bounded UDP-broadcast fallback (Task 2.2 piece 3, vk #994): fills
    /// the gap when mDNS is down and no device is connected. See
    /// `services::broadcast_fallback` module docs for the policy.
    pub fallback_handle: tokio::task::JoinHandle<()>,
}

/// Starts all services and returns their handles.
pub async fn start_services(
    state: Arc<AppState>,
    identity: Identity,
    shutdown: CancellationToken,
) -> Result<ServiceHandles> {
    let (tcp_listener, actual_port) =
        crate::protocol::listener::TcpListenerService::bind_port(state.settings.tcp_port)
            .await
            .map_err(|e| crate::utils::errors::Error::ConnectionError(e.to_string()))?;

    info!(
        port = actual_port,
        preferred = state.settings.tcp_port,
        event = "tcp_listener_bound",
        "TCP listener bound"
    );

    let mut identity = identity;
    identity.tcp_port = Some(actual_port);
    // Task 2.3 gap A plumbing: ConnectionManager::get_identity() needs the
    // REAL bound port (not the DEFAULT_TCP_PORT constant) so the reverse-
    // connection fallback's UDP identity carries a tcpPort Android will
    // actually accept when 1716 was taken and we bound elsewhere in
    // 1716-1764.
    state.connection_manager.set_tcp_port(actual_port);

    let discovery = start_discovery(&state, identity.clone(), shutdown.clone()).await?;
    let tcp = start_tcp_listener(&state, tcp_listener, identity.clone(), shutdown.clone());
    let api = start_api_server(&state, shutdown.clone()).await?;

    info!(event = "daemon_ready", "Daemon ready - all systems active");

    Ok(ServiceHandles {
        discovery,
        tcp,
        api,
    })
}

/// Stops all services and persists state to disk.
pub async fn stop_services(handles: ServiceHandles, state: &Arc<AppState>) {
    let timeout = std::time::Duration::from_secs(5);
    let _ = tokio::time::timeout(timeout, handles.discovery.listen_handle).await;
    if let Some(h) = handles.discovery.mdns_handle {
        let _ = tokio::time::timeout(timeout, h).await;
    }
    let _ = tokio::time::timeout(timeout, handles.discovery.network_change_handle).await;
    let _ = tokio::time::timeout(timeout, handles.discovery.fallback_handle).await;
    if let Some(h) = handles.tcp {
        let _ = tokio::time::timeout(timeout, h).await;
    }
    if let Some(h) = handles.api {
        let _ = tokio::time::timeout(timeout, h).await;
    }

    if let Err(e) = state.registry.save_to_disk().await {
        warn!(error = %e, "Failed to persist device registry on shutdown");
    }
    if let Err(e) = state.pairing_handler.save_to_disk().await {
        warn!(error = %e, "Failed to persist pairing state on shutdown");
    }
}

async fn start_discovery(
    state: &Arc<AppState>,
    identity: Identity,
    shutdown: CancellationToken,
) -> Result<DiscoveryHandles> {
    let discovery =
        Arc::new(DiscoveryService::new(identity.clone(), state.settings.udp_port).await?);

    info!(event = "discovery_ready", "Discovery service initialized");

    let state_clone = state.clone();
    let discovery_listen = discovery.clone();
    let listen_shutdown = shutdown.clone();
    let listen_handle = tokio::spawn(async move {
        discovery_listen
            .start_listening(
                move |remote_identity, addr| {
                    on_device_discovered(state_clone.clone(), remote_identity, addr)
                },
                listen_shutdown,
            )
            .await;
    });

    // mDNS: the PRIMARY discovery channel in both reference implementations
    // (see protocol::mdns_discovery docs). A resolve tells us where a peer
    // is; we answer with our UDP identity and they dial us — the same
    // handshake a UDP broadcast starts. Failure to start mDNS (no
    // multicast on the host) degrades to UDP-only — never fatal.
    let mdns: Option<Arc<crate::protocol::mdns_discovery::MdnsDiscoveryService>> =
        match crate::protocol::mdns_discovery::MdnsDiscoveryService::new(&identity) {
            Ok(mdns) => Some(Arc::new(mdns)),
            Err(e) => {
                warn!(error = %e, event = "mdns_start_failed", "mDNS discovery unavailable, continuing with UDP broadcast only");
                None
            }
        };

    // Tracks mDNS health for the broadcast-fallback policy (Task 2.2
    // piece 3, vk #994 — see services::broadcast_fallback module docs).
    // Starts false when mDNS failed to start at all; flipped false again
    // below if its run task ever exits before shutdown was requested.
    let mdns_healthy = Arc::new(AtomicBool::new(mdns.is_some()));

    let mdns_handle = mdns.clone().map(|mdns| {
        let state_clone = state.clone();
        let mdns_shutdown = shutdown.clone();
        let mdns_healthy = mdns_healthy.clone();
        tokio::spawn(async move {
            mdns.run(
                move |peer| on_mdns_device_resolved(state_clone.clone(), peer),
                mdns_shutdown.clone(),
            )
            .await;
            if !mdns_shutdown.is_cancelled() {
                // The run loop only returns early (before shutdown was
                // requested) when browsing failed to start or the daemon
                // channel closed — i.e. mDNS died on its own. See
                // MdnsDiscoveryService::run's doc comment for the exact
                // exit paths.
                mdns_healthy.store(false, Ordering::Relaxed);
                warn!(
                    event = "mdns_died",
                    "mDNS discovery stopped unexpectedly; UDP broadcast fallback will engage"
                );
            }
        })
    });

    // Announce on start (upstream behavior: kdeconnect-kde
    // lanlinkprovider.cpp:149; kdeconnect-android LanLinkProvider.java:567).
    // mDNS's own "announce on start" already happened inside
    // MdnsDiscoveryService::new above (registers immediately); this is the
    // UDP leg's counterpart. Best-effort — a failure here doesn't block
    // startup, matching how every other discovery-channel failure degrades
    // in this function.
    if let Err(e) = discovery.broadcast().await {
        warn!(
            error = %e,
            event = "startup_broadcast_failed",
            "Failed to send the startup UDP broadcast"
        );
    }

    // Network-change reactor (Task 2.2, vk #994): re-announce (UDP
    // broadcast once + mDNS reannounce) on a debounced network-change
    // event, closing the mdns_discovery.rs "restart announcing on network
    // change" TODO.
    let network_change_handle =
        crate::services::discovery_coordinator::spawn_network_change_reactor(
            discovery.clone(),
            mdns,
            identity,
            shutdown.clone(),
        );

    // Bounded UDP-broadcast fallback (Task 2.2 piece 3, vk #994):
    // broadcasts on a backoff schedule ONLY while mDNS is down and no
    // device is connected. Replaces the old unconditional 60s-forever
    // broadcast loop (parity-checklist.md "Broadcast cadence" row) — a
    // healthy host now broadcasts only on start and on network change,
    // matching both references, with this as the documented divergence
    // covering mDNS absence. See services::broadcast_fallback module
    // docs for the full policy and its rationale.
    let fallback_handle =
        tokio::spawn(crate::services::broadcast_fallback::run_broadcast_fallback(
            discovery.clone(),
            mdns_healthy,
            state.connection_manager.clone(),
            shutdown.clone(),
        ));

    Ok(DiscoveryHandles {
        listen_handle,
        mdns_handle,
        network_change_handle,
        fallback_handle,
    })
}

/// An mDNS resolve tells us WHERE a peer is. The handshake that tells them
/// where WE are is a UDP identity unicast to that address, after which they
/// dial our `tcpPort` — exactly what both references do (kdeconnect-kde
/// `mdnshdiscovery.cpp:26-38`, Android `MdnsDiscovery.onServiceResolved`).
///
/// Until vk #1101 this path dialed the SRV port directly. kdeconnectd can
/// announce SRV port 0 (`mdnshdiscovery.cpp:18` captures
/// `LanLinkProvider::tcpPort()` at construction; `lanlinkprovider.cpp:54`
/// initialises it to 0 and `:139` fills it in only when the TCP server
/// binds), so every such dial failed with ECONNREFUSED and only the
/// reverse-connection fallback ever connected the peer. Now the unicast IS
/// the path, and the registry learns about the peer from its TCP identity,
/// like every other inbound link.
fn on_mdns_device_resolved(state: Arc<AppState>, peer: crate::protocol::mdns_discovery::MdnsPeer) {
    on_mdns_device_resolved_with_udp_port(state, peer, crate::protocol::types::fallback_udp_port())
}

/// `on_mdns_device_resolved`'s real implementation, parameterized by the
/// UDP port the unicast targets: 1716 in production, `TEST_UDP_PORT` in
/// test builds, and a private capture socket in unit tests — the same seam
/// `connect_to_device_with_fallback_port` has (2026-09-06 audit A2).
fn on_mdns_device_resolved_with_udp_port(
    state: Arc<AppState>,
    peer: crate::protocol::mdns_discovery::MdnsPeer,
    udp_port: u16,
) {
    info!(
        device_id = %peer.device_id,
        device_name = %peer.device_name,
        address = %peer.address,
        srv_port = peer.srv_port,
        protocol_version = peer.protocol_version,
        event = "mdns_device_resolved",
        "Discovered device via mDNS"
    );

    tokio::spawn(async move {
        let device_id = peer.device_id.clone();

        // Self-guard (both references: "Discovered myself, ignoring" —
        // mdnshdiscovery.cpp:27-30).
        let our_id = state
            .connection_manager
            .device_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if device_id == our_id {
            return;
        }
        if crate::protocol::is_split_brain(&peer.address, &our_id, &device_id) {
            warn!(
                device_id = %device_id,
                device_name = %peer.device_name,
                address = %peer.address,
                event = "split_brain_suspected",
                "Another KDE Connect implementation is announcing from THIS host: \
                 two daemons will compete for the same paired phones"
            );
            if state.connection_manager.split_brain_policy()
                == crate::protocol::SplitBrainPolicy::Refuse
            {
                // Named it; now act on it — no unicast (2026-09-06 audit A2).
                return;
            }
        }

        // Already linked: nothing to tell them (Android MdnsDiscovery.kt
        // onServiceFound: visibleDevices guard).
        if state.connection_manager.is_connected(&device_id).await {
            return;
        }

        let our_identity = match state.connection_manager.get_identity() {
            Some(id) => id,
            None => {
                // Only an empty device id gets here (get_identity's own
                // guard) — unreachable in a normal run, but a silent
                // return would hide a mis-ordered startup. DEBUG, not
                // WARN: every resolve during shutdown would hit it
                // *(cypher, inkling vs mimo-v25-pro)*.
                debug!(
                    device_id = %device_id,
                    event = "mdns_unicast_skipped_no_identity",
                    "No local identity yet; not answering this resolve"
                );
                return;
            }
        };
        match crate::protocol::udp_unicast::unicast_identity(&our_identity, peer.address, udp_port)
            .await
        {
            Ok(target) => info!(
                device_id = %device_id,
                target = %target,
                event = "mdns_identity_unicast_sent",
                "Sent our identity to an mDNS-resolved peer; they dial us"
            ),
            Err(e) => warn!(
                device_id = %device_id,
                address = %peer.address,
                error = %e,
                event = "mdns_identity_unicast_failed",
                "Failed to send our identity to an mDNS-resolved peer"
            ),
        }
    });
}

fn on_device_discovered(state: Arc<AppState>, identity: Identity, addr: std::net::SocketAddr) {
    let device_id = identity.device_id.clone();
    let device_name = identity.device_name.clone();
    let device_type = DeviceType::parse_device_type(&identity.device_type.clone());

    info!(
        device_id = %identity.device_id,
        device_name = %identity.device_name,
        address = %addr,
        protocol_version = identity.protocol_version,
        event = "device_discovered",
        "Discovered device on network"
    );

    let device = Device::new(
        device_id.clone(),
        device_name,
        device_type,
        identity.protocol_version,
    )
    .with_capabilities(
        identity.incoming_capabilities.clone(),
        identity.outgoing_capabilities.clone(),
    );

    tokio::spawn(async move {
        if let Err(e) = state.registry.upsert_device(device).await {
            warn!(error = %e, device_id = %device_id, event = "device_upsert_failed", "Failed to upsert discovered device");
        }

        crate::services::connection_orchestrator::spawn_discovered_connection(
            state, identity, addr,
        );
    });
}

fn start_tcp_listener(
    state: &Arc<AppState>,
    listener: tokio::net::TcpListener,
    identity: Identity,
    shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let state = state.clone();
    Some(tokio::spawn(async move {
        let svc = crate::protocol::listener::TcpListenerService::new(state, identity);
        if let Err(e) = svc.run_from_bound(listener, shutdown).await {
            warn!(error = %e, event = "tcp_listener_error", "TCP listener error");
        }
    }))
}

async fn start_api_server(
    state: &Arc<AppState>,
    shutdown: CancellationToken,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    if !state.settings.api_enabled {
        return Ok(None);
    }

    let api_state = state.clone();
    let api_port = state.settings.api_port;
    let api_bind = state.settings.api_bind.clone();
    let router = crate::api::build_router(api_state);

    if state.settings.api_keys.is_empty() {
        warn!(
            event = "api_no_keys_configured",
            "No API keys configured: the API will reject ALL authenticated requests"
        );
    }

    // Tuple form so IPv6 bind addresses ("::1") get correct bracket handling;
    // format!("{}:{}") would produce invalid "::1:9090".
    let listener = tokio::net::TcpListener::bind((api_bind.as_str(), api_port))
        .await
        .map_err(|error| {
            crate::utils::errors::Error::ConnectionError(format!(
                "failed to bind API port {api_port} on {api_bind}: {error}; another rust-connect instance is already running"
            ))
        })?;

    info!(
        bind = %api_bind,
        port = api_port,
        event = "api_server_starting",
        "Starting API server"
    );
    Ok(Some(tokio::spawn(async move {
        // into_make_service_with_connect_info provides ConnectInfo<SocketAddr>;
        // the rate limiter buckets per client IP and breaks without it.
        // A4 (2026-09-02 audit): without graceful shutdown this task never
        // ended and every daemon stop paid a 5 s join timeout for it.
        if let Err(e) = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        {
            tracing::error!(error = %e, event = "api_server_failed", "API server stopped");
        }
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::config::settings::AppSettings;

    const OUR_ID: &str = "svc-mgr-our-device-aaaaaaaaaaaaaaa";
    const PEER_ID: &str = "svc-mgr-peer-device-aaaaaaaaaaaaaa";

    fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
        let settings = AppSettings::new_with_data_dir(temp_dir.path().to_path_buf());
        let state =
            Arc::new(AppState::new_without_input(settings).expect("Value expected to be present"));
        state.connection_manager.set_device_identity(OUR_ID, "Us");
        (state, temp_dir)
    }

    fn peer_identity(tcp_port: u16) -> Identity {
        let mut identity = Identity::new(
            PEER_ID.to_string(),
            "Peer".to_string(),
            DeviceType::Phone,
            vec![],
            vec![],
        );
        identity.tcp_port = Some(tcp_port);
        identity
    }

    #[tokio::test]
    async fn test_start_services_fails_when_api_port_is_taken() {
        let (base_state, _t) = test_state();
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let port = occupied
            .local_addr()
            .expect("Value expected to be present")
            .port();

        let mut settings = base_state.settings.clone();
        settings.tcp_port = 0;
        settings.udp_port = 0;
        settings.api_bind = "127.0.0.1".to_string();
        settings.api_port = port;
        let state =
            Arc::new(AppState::new_without_input(settings).expect("Value expected to be present"));
        state.connection_manager.set_device_identity(OUR_ID, "Us");
        let shutdown = CancellationToken::new();

        let error = match start_services(state, peer_identity(0), shutdown).await {
            Ok(_) => panic!("an enabled API port conflict must be fatal"),
            Err(error) => error,
        };

        assert!(error.to_string().contains(&port.to_string()));
        assert!(error
            .to_string()
            .contains("another rust-connect instance is already running"));
    }
    /// A4 (2026-09-02 audit): every stop of the daemon took exactly 15 s
    /// because the discovery listen loop, the TCP accept loop and the API
    /// server never observed the shutdown token, so `stop_services` burned
    /// three 5 s join timeouts before persisting anything. A cancelled
    /// token must bring every service task home promptly.
    #[tokio::test]
    async fn test_stop_services_returns_promptly_after_shutdown() {
        let (base_state, _t) = test_state();
        let mut settings = base_state.settings.clone();
        settings.tcp_port = 0;
        settings.udp_port = 0;
        settings.api_bind = "127.0.0.1".to_string();
        settings.api_port = 0;
        let state =
            Arc::new(AppState::new_without_input(settings).expect("Value expected to be present"));
        state.connection_manager.set_device_identity(OUR_ID, "Us");
        let shutdown = CancellationToken::new();

        let handles = start_services(state.clone(), peer_identity(0), shutdown.clone())
            .await
            .expect("services must start on ephemeral ports");

        shutdown.cancel();
        let started = std::time::Instant::now();
        stop_services(handles, &state).await;
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(2),
            "stop_services took {took:?}; the service tasks are not observing the shutdown token"
        );
    }

    use crate::protocol::mdns_discovery::MdnsPeer;

    fn mdns_peer(address: std::net::IpAddr, srv_port: u16) -> MdnsPeer {
        MdnsPeer {
            device_id: PEER_ID.to_string(),
            device_name: "Peer".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 8,
            address,
            srv_port,
        }
    }

    /// A socket standing in for the peer's UDP 1716.
    async fn capture_socket() -> (tokio::net::UdpSocket, u16) {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind capture");
        let port = socket.local_addr().expect("local_addr").port();
        (socket, port)
    }

    /// The identity the peer would receive, or `None` if nothing arrives
    /// within `within`.
    async fn recv_identity(
        capture: &tokio::net::UdpSocket,
        within: std::time::Duration,
    ) -> Option<Identity> {
        let mut buf = vec![0u8; 65536];
        let (len, _) = tokio::time::timeout(within, capture.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        let packet = crate::protocol::packet::PacketSerializer::deserialize(&buf[..len]).ok()?;
        Identity::from_packet(packet).ok()
    }

    /// vk #1101: a resolve is answered with OUR identity over UDP (the
    /// reference behaviour, mdnshdiscovery.cpp:36) — never with a dial,
    /// even when the SRV port would have accepted one. Fails before the
    /// change: the handler dialed `listener` and registered the peer.
    #[tokio::test]
    async fn test_mdns_resolve_unicasts_our_identity_and_never_dials() {
        let (state, _t) = test_state();
        let (capture, udp_port) = capture_socket().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let srv_port = listener.local_addr().expect("local_addr").port();

        // `Identity::new` defaults tcp_port to 1716 (types.rs:186), so a
        // handler that built a fresh Identity instead of asking the
        // connection manager would pass an `is_some()` check. Pin the
        // real bound port *(cypher, codex + qwen-38max)*.
        state.connection_manager.set_tcp_port(1764);

        on_mdns_device_resolved_with_udp_port(
            state.clone(),
            mdns_peer(std::net::Ipv4Addr::LOCALHOST.into(), srv_port),
            udp_port,
        );

        let received = recv_identity(&capture, std::time::Duration::from_secs(2))
            .await
            .expect("our identity must reach the peer's UDP port");
        assert_eq!(received.device_id, OUR_ID);
        assert_eq!(
            received.tcp_port,
            Some(1764),
            "the UDP identity must carry the port we actually listen on"
        );

        let dial =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await;
        assert!(
            dial.is_err(),
            "an mDNS resolve must not be dialed, even on a live port"
        );
        assert!(
            state.registry.get(&PEER_ID.to_string()).await.is_err(),
            "an mDNS resolve writes nothing to the registry; the peer's TCP identity does"
        );
    }

    /// The #1101 scenario itself: kdeconnectd announcing SRV port 0. Before
    /// the change this dialed 127.0.0.1:0 (instant ECONNREFUSED), and only
    /// the reverse-connection fallback — aimed at fallback_udp_port(), not
    /// at this capture — ever told the peer about us.
    #[tokio::test]
    async fn test_mdns_resolve_with_srv_port_zero_still_reaches_the_peer() {
        let (state, _t) = test_state();
        let (capture, udp_port) = capture_socket().await;

        on_mdns_device_resolved_with_udp_port(
            state.clone(),
            mdns_peer(std::net::Ipv4Addr::LOCALHOST.into(), 0),
            udp_port,
        );

        let received = recv_identity(&capture, std::time::Duration::from_secs(2))
            .await
            .expect("a port-0 SRV record must not stop the handshake");
        assert_eq!(received.device_id, OUR_ID);
        assert!(state.registry.get(&PEER_ID.to_string()).await.is_err());
    }

    /// mDNS TXT carries no capabilities; the old handler upserted unknown
    /// devices and guarded against clobbering known ones. Now it writes
    /// nothing at all: a known device is unchanged, and the unicast still
    /// goes out because the device is not connected.
    #[tokio::test]
    async fn test_mdns_resolve_leaves_the_registry_untouched() {
        let (state, _t) = test_state();
        state
            .registry
            .add(
                Device::new(
                    PEER_ID.to_string(),
                    "Peer".to_string(),
                    DeviceType::Phone,
                    8,
                )
                .with_capabilities(
                    vec!["kdeconnect.battery".to_string()],
                    vec!["kdeconnect.battery".to_string()],
                ),
            )
            .await
            .expect("add");
        let before = state.registry.get(&PEER_ID.to_string()).await.expect("get");
        let (capture, udp_port) = capture_socket().await;

        on_mdns_device_resolved_with_udp_port(
            state.clone(),
            mdns_peer(std::net::Ipv4Addr::LOCALHOST.into(), 1716),
            udp_port,
        );
        recv_identity(&capture, std::time::Duration::from_secs(2))
            .await
            .expect("the unicast still goes out for a known, unconnected device");

        let after = state.registry.get(&PEER_ID.to_string()).await.expect("get");
        assert_eq!(after.incoming_capabilities, before.incoming_capabilities);
        assert_eq!(after.outgoing_capabilities, before.outgoing_capabilities);
        assert_eq!(after.name, before.name);
        assert_eq!(after.protocol_version, before.protocol_version);
    }

    /// 2026-09-06 audit A2, carried over: under `SplitBrainPolicy::Refuse`
    /// (the production default) a foreign id resolving from one of our own
    /// addresses gets neither a registry entry nor our identity.
    #[tokio::test]
    async fn test_mdns_resolve_split_brain_is_refused() {
        let (state, _t) = test_state();
        state
            .connection_manager
            .set_split_brain_policy(crate::protocol::SplitBrainPolicy::Refuse);
        let (capture, udp_port) = capture_socket().await;

        // Foreign id, loopback source: another daemon on this host.
        on_mdns_device_resolved_with_udp_port(
            state.clone(),
            mdns_peer(std::net::Ipv4Addr::LOCALHOST.into(), 1716),
            udp_port,
        );

        assert!(
            recv_identity(&capture, std::time::Duration::from_millis(500))
                .await
                .is_none(),
            "a split-brain id must not be handed our identity"
        );
        assert!(state.registry.get(&PEER_ID.to_string()).await.is_err());
    }

    /// Our own announcement resolved back (multicast loopback) is ignored
    /// before any unicast.
    #[tokio::test]
    async fn test_mdns_resolve_self_is_ignored() {
        let (state, _t) = test_state();
        let (capture, udp_port) = capture_socket().await;
        let mut me = mdns_peer(std::net::Ipv4Addr::LOCALHOST.into(), 1716);
        me.device_id = OUR_ID.to_string();

        on_mdns_device_resolved_with_udp_port(state.clone(), me, udp_port);

        assert!(
            recv_identity(&capture, std::time::Duration::from_millis(500))
                .await
                .is_none(),
            "our own announcement must not trigger a unicast"
        );
        assert!(state.registry.get(&OUR_ID.to_string()).await.is_err());
    }

    /// The third kept guard, and the one the no-cooldown answer leans on:
    /// a resolve for a device we are already linked to sends nothing
    /// *(cypher: ten voices; the only guard the first draft left
    /// untested)*. `is_connected` consults the `test_generations` shadow
    /// (`connection/mod.rs:879-898`) — NOT `mark_fake_connected_for_test`,
    /// which feeds only the capability gate.
    #[tokio::test]
    async fn test_mdns_resolve_already_connected_does_not_unicast() {
        let (state, _t) = test_state();
        let (capture, udp_port) = capture_socket().await;
        state
            .connection_manager
            .mark_generation_for_test(PEER_ID, 1);

        on_mdns_device_resolved_with_udp_port(
            state.clone(),
            mdns_peer(std::net::Ipv4Addr::LOCALHOST.into(), 1716),
            udp_port,
        );

        assert!(
            recv_identity(&capture, std::time::Duration::from_millis(500))
                .await
                .is_none(),
            "a connected device must not be handed our identity again"
        );
        state.connection_manager.unmark_generation_for_test(PEER_ID);
    }

    /// The spec's first constraint, pinned at the production entry point:
    /// the wrapper must route the unicast through `fallback_udp_port()`.
    /// Every other test injects the port through the seam, so a wrapper
    /// written against `DEFAULT_UDP_PORT` — or a literal 1716 — would
    /// compile, pass them all, and hand a fixture identity to a live
    /// daemon on this host (the 2026-09-06 incident class)
    /// *(cypher, kimi-k3 + qwen-38max missing test 1)*.
    #[tokio::test]
    async fn test_mdns_resolve_production_wrapper_targets_the_test_udp_port() {
        let (state, _t) = test_state();
        let capture = tokio::net::UdpSocket::bind((
            std::net::Ipv4Addr::LOCALHOST,
            crate::protocol::types::TEST_UDP_PORT,
        ))
        .await
        .expect("bind the test-build UDP port; only this test binds it");

        on_mdns_device_resolved(
            state.clone(),
            mdns_peer(std::net::Ipv4Addr::LOCALHOST.into(), 1716),
        );

        let received = recv_identity(&capture, std::time::Duration::from_secs(2))
            .await
            .expect("the production wrapper must unicast to fallback_udp_port(), i.e. TEST_UDP_PORT here");
        assert_eq!(received.device_id, OUR_ID);
    }
}

//! Service manager
//!
//! Single Responsibility: Start and stop long-running services (discovery, TCP listener, API).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
    let listen_handle = tokio::spawn(async move {
        discovery_listen
            .start_listening(move |remote_identity, addr| {
                on_device_discovered(state_clone.clone(), remote_identity, addr)
            })
            .await;
    });

    // mDNS: the PRIMARY discovery channel in both reference implementations
    // (see protocol::mdns_discovery docs). A resolve is fed into the same
    // dial path a received UDP identity takes. Failure to start mDNS (no
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
                move |identity, addr| on_mdns_device_resolved(state_clone.clone(), identity, addr),
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

/// Handle an mDNS-resolved peer. The dial itself goes through the SAME
/// `spawn_discovered_connection` a received UDP identity takes — only the
/// guards specific to mDNS's shape live here.
fn on_mdns_device_resolved(state: Arc<AppState>, identity: Identity, addr: std::net::SocketAddr) {
    info!(
        device_id = %identity.device_id,
        device_name = %identity.device_name,
        address = %addr,
        protocol_version = identity.protocol_version,
        event = "mdns_device_resolved",
        "Discovered device via mDNS"
    );

    tokio::spawn(async move {
        let device_id = identity.device_id.clone();

        // Self-guard before any registry write (both references:
        // "Discovered myself, ignoring" — mdnshdiscovery.cpp:27-30).
        {
            let our_id = state
                .connection_manager
                .device_id
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if device_id == our_id {
                return;
            }
        }

        // Already connected: nothing to do (Android MdnsDiscovery.kt
        // onServiceFound: visibleDevices guard).
        if state.connection_manager.is_connected(&device_id).await {
            return;
        }

        // Register only UNKNOWN devices. mDNS TXT carries no capability
        // lists (protocol::mdns_discovery docs), and upsert_device
        // overwrites a known device's capabilities — an mDNS resolve for a
        // UDP-known device would clobber them with empty lists. A new
        // device's caps arrive with the TCP identity exchange on connect.
        if state.registry.get(&device_id).await.is_err() {
            let device_type = DeviceType::parse_device_type(&identity.device_type.clone());
            let device = Device::new(
                device_id.clone(),
                identity.device_name.clone(),
                device_type,
                identity.protocol_version,
            );
            if let Err(e) = state.registry.upsert_device(device).await {
                warn!(error = %e, device_id = %device_id, event = "device_upsert_failed", "Failed to upsert mDNS-discovered device");
            }
        }

        crate::services::connection_orchestrator::spawn_discovered_connection(
            state, identity, addr,
        );
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
    _shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let state = state.clone();
    Some(tokio::spawn(async move {
        let svc = crate::protocol::listener::TcpListenerService::new(state, identity);
        if let Err(e) = svc.run_from_bound(listener).await {
            warn!(error = %e, event = "tcp_listener_error", "TCP listener error");
        }
    }))
}

async fn start_api_server(
    state: &Arc<AppState>,
    _shutdown: CancellationToken,
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
        if let Err(e) = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
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
    #[tokio::test]
    async fn test_mdns_resolve_unknown_device_registers_and_dials() {
        let (state, _t) = test_state();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        on_mdns_device_resolved(state.clone(), peer_identity(addr.port()), addr);

        let mut registered = false;
        for _ in 0..80 {
            if state.registry.get(&PEER_ID.to_string()).await.is_ok() {
                registered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(registered, "an mDNS-resolved device must be registered");

        let dial = tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept()).await;
        assert!(
            dial.is_ok(),
            "an mDNS-resolved device must be dialed on its announced port"
        );
    }

    /// A device already known via UDP keeps its capability lists: mDNS TXT
    /// carries none, and a naive upsert would clobber them.
    #[tokio::test]
    async fn test_mdns_resolve_known_device_preserves_capabilities() {
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
            .expect("Value expected to be present");

        // A dead port: the dial attempt fails harmlessly; what matters is
        // the registry state afterwards.
        let dead_port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            probe.local_addr().expect("probe addr").port()
        };
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", dead_port)
            .parse()
            .expect("Value expected to be present");

        // mDNS resolves carry empty capability lists (see module docs).
        on_mdns_device_resolved(state.clone(), peer_identity(dead_port), addr);

        // Let the spawned task run its registry branch.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let device = state
            .registry
            .get(&PEER_ID.to_string())
            .await
            .expect("Value expected to be present");
        assert_eq!(
            device.incoming_capabilities,
            vec!["kdeconnect.battery".to_string()],
            "mDNS must not clobber capabilities a UDP identity provided"
        );
        assert_eq!(
            device.outgoing_capabilities,
            vec!["kdeconnect.battery".to_string()]
        );
    }

    /// Our own announcement resolved back (multicast loopback) must be
    /// ignored before any registry write or dial.
    #[tokio::test]
    async fn test_mdns_resolve_self_is_ignored() {
        let (state, _t) = test_state();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Value expected to be present");
        let addr = listener.local_addr().expect("Value expected to be present");

        let mut self_identity = peer_identity(addr.port());
        self_identity.device_id = OUR_ID.to_string();
        on_mdns_device_resolved(state.clone(), self_identity, addr);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            state.registry.get(&OUR_ID.to_string()).await.is_err(),
            "our own id must never enter the registry via mDNS"
        );
        let dial =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await;
        assert!(dial.is_err(), "our own id must never be dialed");
    }
}

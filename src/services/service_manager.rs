//! Service manager
//!
//! Single Responsibility: Start and stop long-running services (discovery, TCP listener, API).

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
    pub broadcast_handle: tokio::task::JoinHandle<()>,
    pub listen_handle: tokio::task::JoinHandle<()>,
    /// mDNS announce + browse (None when the mDNS daemon failed to start —
    /// UDP broadcast discovery still stands).
    pub mdns_handle: Option<tokio::task::JoinHandle<()>>,
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

    let discovery = start_discovery(&state, identity.clone(), shutdown.clone()).await?;
    let tcp = start_tcp_listener(&state, tcp_listener, identity.clone(), shutdown.clone());
    let api = start_api_server(&state, shutdown.clone()).await;

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
    let _ = tokio::time::timeout(timeout, handles.discovery.broadcast_handle).await;
    let _ = tokio::time::timeout(timeout, handles.discovery.listen_handle).await;
    if let Some(h) = handles.discovery.mdns_handle {
        let _ = tokio::time::timeout(timeout, h).await;
    }
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
    let discovery = Arc::new(
        DiscoveryService::new(identity.clone(), state.settings.broadcast_interval_secs).await?,
    );

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

    let discovery_broadcast = discovery.clone();
    // Discovery broadcast runs for the daemon's whole lifetime — KDE Connect
    // peers rely on continuous broadcast to find additional devices. It stops
    // only at shutdown. (It used to be cancelled on the first connection to
    // mask the April pairing-storm bug; the fix is correct pairing, not a
    // mute discovery service.)
    let broadcast_shutdown = shutdown.clone();
    let broadcast_handle = tokio::spawn(async move {
        discovery_broadcast
            .start_broadcasting(broadcast_shutdown)
            .await;
    });

    // mDNS: the PRIMARY discovery channel in both reference implementations
    // (see protocol::mdns_discovery docs). A resolve is fed into the same
    // dial path a received UDP identity takes. Failure to start mDNS (no
    // multicast on the host) degrades to UDP-only — never fatal.
    // TODO: once mDNS is live-validated against real hardware, revisit the
    // UDP broadcast cadence (60s default today) — with mDNS healthy it
    // becomes a fallback, not the rediscovery path.
    let mdns_handle = match crate::protocol::mdns_discovery::MdnsDiscoveryService::new(&identity) {
        Ok(mdns) => {
            let state_clone = state.clone();
            Some(tokio::spawn(async move {
                mdns.run(
                    move |identity, addr| {
                        on_mdns_device_resolved(state_clone.clone(), identity, addr)
                    },
                    shutdown,
                )
                .await;
            }))
        }
        Err(e) => {
            warn!(error = %e, event = "mdns_start_failed", "mDNS discovery unavailable, continuing with UDP broadcast only");
            None
        }
    };

    Ok(DiscoveryHandles {
        broadcast_handle,
        listen_handle,
        mdns_handle,
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
) -> Option<tokio::task::JoinHandle<()>> {
    if !state.settings.api_enabled {
        return None;
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
    let listener = tokio::net::TcpListener::bind((api_bind.as_str(), api_port)).await;

    match listener {
        Ok(listener) => {
            info!(
                bind = %api_bind,
                port = api_port,
                event = "api_server_starting",
                "Starting API server"
            );
            Some(tokio::spawn(async move {
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
            }))
        }
        Err(e) => {
            warn!(bind = %api_bind, port = api_port, error = %e, event = "api_bind_failed", "API address in use, continuing without API server");
            None
        }
    }
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

    /// An unknown mDNS-resolved device is registered and dialed — the same
    /// outcome a received UDP identity produces.
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

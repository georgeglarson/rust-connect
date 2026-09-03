//! mDNS (DNS-SD) discovery — the primary discovery channel in BOTH
//! reference implementations, complementing our UDP broadcast.
//!
//! Protocol shape (grounded in the references):
//!
//! - Service type `_kdeconnect._udp` — yes, `_udp`, even though the data
//!   connection is TCP (kdeconnect-kde `mdnshdiscovery.cpp:15`,
//!   `avahidiscovery.cpp:16`; Android `MdnsDiscovery.kt` `SERVICE_TYPE`).
//! - Instance name = device id (kdeconnect-kde `mdnshdiscovery.cpp:18`;
//!   Android `createNsdServiceInfo`: "we use the deviceId"), service port =
//!   the TCP port peers should dial (same sources).
//! - TXT records `id`, `name`, `type`, `protocol` (kdeconnect-kde
//!   `mdnshdiscovery.cpp:21-24`, `avahidiscovery.cpp:136-139`; Android
//!   `createNsdServiceInfo` `setAttribute` calls). Capability lists are NOT
//!   carried — Android's comment: the fields "aren't really used for
//!   anything, since we can't include enough info for it to be useful".
//! - The references answer a resolve by sending a UDP identity to the
//!   resolved address (`sendUdpIdentityPacket`, `mdnshdiscovery.cpp:36`,
//!   Android `onServiceResolved`) and let the peer dial. Both carry the
//!   same TODO: with protocol v8 the resolve already carries everything
//!   needed to dial directly (`mdnshdiscovery.cpp:31-35`). We take the v8
//!   path: a resolved service is turned into an `Identity` + address and
//!   fed into the exact path a received UDP identity takes
//!   (`service_manager::on_mdns_device_resolved` →
//!   `connection_orchestrator::spawn_discovered_connection`).

use std::net::SocketAddr;
use std::sync::Arc;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::device::types::DeviceType;
use crate::protocol::types::{Identity, DEFAULT_TCP_PORT};
use crate::utils::errors::{Error, Result};

/// The KDE Connect DNS-SD service type. `_udp` is NOT a typo — see the
/// module docs for the citations.
#[cfg(not(any(test, feature = "test-helpers")))]
pub const SERVICE_TYPE: &str = "_kdeconnect._udp.local.";
/// Test builds announce and browse a service type no real KDE Connect
/// peer knows (2026-09-02 audit, D2): `cargo test` was publishing fixture
/// identities that the paired A15 dialed and that the live daemon on the
/// same host registered. Loopback scoping is not enough — Linux delivers
/// looped multicast to every local member of the group — so the type
/// itself changes. Every `cargo test` build carries `test-helpers` (the
/// crate is its own dev-dependency with that feature); `cargo build
/// --locked`, which the interop harness and the release use, never does.
#[cfg(any(test, feature = "test-helpers"))]
pub const SERVICE_TYPE: &str = "_kdeconnect-test._udp.local.";

/// mDNS announcer + browser for this device.
pub struct MdnsDiscoveryService {
    daemon: Arc<ServiceDaemon>,
    /// Our registered service's fullname, needed to unregister cleanly.
    /// `RwLock`, not a plain field: `reannounce` (Task 2.2) is called from
    /// a network-change-handling task concurrently with `run`'s own browse
    /// loop, both holding only a shared reference to the SAME instance —
    /// same interior-mutability shape every other backend-bearing plugin
    /// in this codebase uses for state shared across tasks.
    fullname: std::sync::RwLock<String>,
}

/// Builds the `ServiceInfo` for `identity`, exactly as the references
/// announce (module docs): instance name = device id, port = our TCP
/// listener port, TXT records `id`/`name`/`type`/`protocol`.
///
/// `enable_addr_auto()` matters beyond initial registration: `mdns-sd`'s
/// own daemon polls the host's interfaces on a timer
/// (`ServiceDaemon::set_ip_check_interval`,
/// `IP_CHECK_INTERVAL_IN_SECS_DEFAULT` = 5s, verified in the vendored
/// source) and automatically re-announces any `addr_auto`-enabled service
/// when it finds a new address — so this crate ALREADY self-heals address
/// changes within 5 seconds on its own, with zero code here. What that
/// doesn't give us is IMMEDIACY (there's no public "check now" API, only
/// "change the future polling interval") or the UDP broadcast leg, which
/// has no auto-refresh mechanism of its own at all — see
/// `MdnsDiscoveryService::reannounce` and `discovery_coordinator.rs`.
fn build_service_info(identity: &Identity) -> Result<ServiceInfo> {
    let port = identity.tcp_port.unwrap_or(DEFAULT_TCP_PORT);
    let host_name = format!(
        "{}.local.",
        hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "rust-connect".to_string())
    );
    // TXT records, exactly the references' set (module docs).
    let protocol = identity.protocol_version.to_string();
    let properties = [
        ("id", identity.device_id.as_str()),
        ("name", identity.device_name.as_str()),
        ("type", identity.device_type.as_str()),
        ("protocol", protocol.as_str()),
    ];

    let mut service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &identity.device_id,
        &host_name,
        "",
        port,
        &properties[..],
    )
    .map_err(|e| Error::DiscoveryError(format!("Failed to build mDNS service info: {}", e)))?;
    // Announce on whatever addresses the host has right now, AND keep
    // following changes afterward — see this function's doc comment.
    service_info = service_info.enable_addr_auto();
    Ok(service_info)
}

impl MdnsDiscoveryService {
    /// Create the daemon and announce `identity`. The instance name is our
    /// device id and the port our TCP listener port — what the references
    /// announce (module docs).
    pub fn new(identity: &Identity) -> Result<Self> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| Error::DiscoveryError(format!("Failed to start mDNS daemon: {}", e)))?;

        let service_info = build_service_info(identity)?;
        let port = identity.tcp_port.unwrap_or(DEFAULT_TCP_PORT);
        let fullname = service_info.get_fullname().to_string();
        daemon.register(service_info).map_err(|e| {
            Error::DiscoveryError(format!("Failed to register mDNS service: {}", e))
        })?;

        info!(
            fullname = %fullname,
            port = port,
            event = "mdns_announcing",
            "Announcing identity via mDNS"
        );

        Ok(Self {
            daemon: Arc::new(daemon),
            fullname: std::sync::RwLock::new(fullname),
        })
    }

    /// Re-announce `identity` on network change (Task 2.2, vk #994; the
    /// TODO this closes — both references restart announcing on network
    /// change, `mdnshdiscovery.cpp:149,192` /
    /// `LanLinkProvider.java:567,572-584`). Unregisters the current
    /// announcement (sends mDNS goodbye packets) then registers fresh
    /// `ServiceInfo` on the SAME daemon — the daemon and its browse loop
    /// keep running throughout, only the announcement itself restarts.
    /// This is a latency improvement over `mdns-sd`'s own 5-second
    /// auto-poll (`build_service_info`'s doc comment), not a fix for a
    /// total absence of self-healing — that already exists.
    pub fn reannounce(&self, identity: &Identity) -> Result<()> {
        let current_fullname = self
            .fullname
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Err(e) = self.daemon.unregister(&current_fullname) {
            warn!(
                error = %e,
                event = "mdns_reannounce_unregister_failed",
                "Failed to unregister mDNS service before re-announcing; \
                 continuing to register the fresh announcement anyway"
            );
        }

        let service_info = build_service_info(identity)?;
        let fullname = service_info.get_fullname().to_string();
        self.daemon.register(service_info).map_err(|e| {
            Error::DiscoveryError(format!(
                "Failed to re-register mDNS service on network change: {}",
                e
            ))
        })?;
        *self.fullname.write().unwrap_or_else(|e| e.into_inner()) = fullname.clone();

        info!(
            fullname = %fullname,
            event = "mdns_reannounced",
            "Re-announced identity via mDNS after a network change"
        );
        Ok(())
    }

    /// Browse for peers until `shutdown` is cancelled, then unregister our
    /// announcement and shut the daemon down. Every resolved service is
    /// converted to the SAME `(Identity, SocketAddr)` shape a received UDP
    /// identity arrives in and handed to `on_resolved`. Takes `&self`, not
    /// `self`, so a caller can hold the SAME `Arc<MdnsDiscoveryService>`
    /// and call `reannounce` concurrently (Task 2.2) — previously this
    /// consumed `self`, which made that impossible.
    pub async fn run<F>(&self, on_resolved: F, shutdown: CancellationToken)
    where
        F: Fn(Identity, SocketAddr),
    {
        let receiver = match self.daemon.browse(SERVICE_TYPE) {
            Ok(receiver) => receiver,
            Err(e) => {
                warn!(error = %e, event = "mdns_browse_failed", "Failed to start mDNS browsing");
                return;
            }
        };

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                event = receiver.recv_async() => {
                    match event {
                        Ok(ServiceEvent::ServiceResolved(resolved)) => {
                            let service = ServiceView::from(&*resolved);
                            match resolved_to_identity(&service) {
                                Some((identity, addr)) => {
                                    debug!(
                                        device_id = %identity.device_id,
                                        address = %addr,
                                        event = "mdns_service_resolved",
                                        "Resolved mDNS service"
                                    );
                                    on_resolved(identity, addr);
                                }
                                None => {
                                    debug!(
                                        fullname = %resolved.get_fullname(),
                                        event = "mdns_resolve_skipped",
                                        "Resolved mDNS service is not a usable KDE Connect peer"
                                    );
                                }
                            }
                        }
                        // ServiceFound/ServiceRemoved/Search*: removal is
                        // ignored — like Android's onServiceLost, which does
                        // nothing (stale links die by keepalive, not by
                        // discovery gossip).
                        Ok(_) => {}
                        // The browse channel only closes when the daemon is
                        // gone; nothing left to do.
                        Err(_) => break,
                    }
                }
            }
        }

        self.stop_announcing();
        let _ = self.daemon.shutdown();
        info!(event = "mdns_stopped", "mDNS discovery stopped");
    }

    /// Unregister our announcement (sends mDNS goodbye packets).
    pub fn stop_announcing(&self) {
        let fullname = self
            .fullname
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Err(e) = self.daemon.unregister(&fullname) {
            warn!(error = %e, event = "mdns_unregister_failed", "Failed to unregister mDNS service");
        }
    }
}

/// The fields `resolved_to_identity` needs from a resolved service,
/// abstracted so the conversion is unit-testable (`ResolvedService` is
/// `#[non_exhaustive]` and cannot be constructed outside mdns-sd).
struct ServiceView<'a> {
    fullname: &'a str,
    port: u16,
    addresses: Vec<std::net::IpAddr>,
    txt: std::collections::HashMap<String, String>,
}

impl<'a> From<&'a ResolvedService> for ServiceView<'a> {
    fn from(resolved: &'a ResolvedService) -> Self {
        Self {
            fullname: resolved.get_fullname(),
            port: resolved.get_port(),
            addresses: resolved
                .get_addresses()
                .iter()
                .map(|scoped| scoped.to_ip_addr())
                .collect(),
            txt: resolved.get_properties().clone().into_property_map_str(),
        }
    }
}

/// Convert a resolved service to the `(Identity, SocketAddr)` a received
/// UDP identity would carry. `None` for anything that is not a usable KDE
/// Connect peer (missing/invalid TXT, no private address).
fn resolved_to_identity(service: &ServiceView) -> Option<(Identity, SocketAddr)> {
    // Device id: the `id` TXT record, falling back to the instance name —
    // both are the device id in both reference implementations (module
    // docs).
    let instance_name = service
        .fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|prefix| prefix.strip_suffix('.'))
        .unwrap_or(service.fullname);
    let device_id = service
        .txt
        .get("id")
        .map(String::as_str)
        .unwrap_or(instance_name)
        .to_string();
    if crate::protocol::crypto::validate_device_id(&device_id).is_err() {
        return None;
    }

    // Without a parseable protocol version we can't safely dial (the
    // downgrade guard and the identity exchange both key on it); every real
    // announcer sends it.
    let protocol_version: u32 = service.txt.get("protocol")?.parse().ok()?;

    let device_name = service
        .txt
        .get("name")
        .cloned()
        .unwrap_or_else(|| device_id.clone());
    let device_type = service
        .txt
        .get("type")
        .map(String::as_str)
        .unwrap_or("desktop")
        .to_string();

    // Prefer a private IPv4 (LAN protocol; a public address here is a
    // spoof), fall back to any private address (IPv6 ULA/link-local).
    let address = service
        .addresses
        .iter()
        .find(|ip| ip.is_ipv4() && crate::protocol::is_private_address(ip))
        .or_else(|| {
            service
                .addresses
                .iter()
                .find(|ip| crate::protocol::is_private_address(ip))
        })?;

    let addr = SocketAddr::new(*address, service.port);

    let device_type = DeviceType::parse_device_type(&device_type);
    let mut identity = Identity::new(
        device_id,
        device_name,
        device_type,
        // No capability lists in mDNS TXT (module docs) — they arrive with
        // the TCP identity exchange after the dial.
        vec![],
        vec![],
    );
    identity.protocol_version = protocol_version;
    identity.tcp_port = Some(service.port);

    Some((identity, addr))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use tokio::sync::mpsc;

    fn our_identity() -> Identity {
        let mut identity = Identity::new(
            "mdns-our-device-aaaaaaaaaaaaaaaaa".to_string(),
            "mDNS Test Device".to_string(),
            DeviceType::Desktop,
            vec![],
            vec![],
        );
        identity.tcp_port = Some(17161);
        identity
    }

    /// Build a ServiceView as the browse loop would deliver it.
    fn service_view<'a>(
        fullname: &'a str,
        ip: std::net::IpAddr,
        port: u16,
        properties: &[(&str, &str)],
    ) -> ServiceView<'a> {
        ServiceView {
            fullname,
            port,
            addresses: vec![ip],
            txt: properties
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn test_resolved_to_identity_reads_reference_shape() {
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        let resolved = service_view(
            &good_name,
            "192.168.1.50"
                .parse()
                .expect("Value expected to be present"),
            1716,
            &[
                ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
                ("name", "test phone"),
                ("type", "phone"),
                ("protocol", "8"),
            ],
        );

        let (identity, addr) =
            resolved_to_identity(&resolved).expect("a reference-shaped service must convert");
        assert_eq!(identity.device_id, "peer-device-aaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(identity.device_name, "test phone");
        assert_eq!(identity.device_type, "phone");
        assert_eq!(identity.protocol_version, 8);
        assert_eq!(identity.tcp_port, Some(1716));
        assert_eq!(
            addr,
            SocketAddr::new(
                "192.168.1.50"
                    .parse()
                    .expect("Value expected to be present"),
                1716
            )
        );
        // mDNS TXT carries no capabilities (module docs).
        assert!(identity.incoming_capabilities.is_empty());
        assert!(identity.outgoing_capabilities.is_empty());
    }

    #[test]
    fn test_resolved_to_identity_falls_back_to_instance_name_for_id() {
        // No `id` TXT: the instance name is the device id in both reference
        // implementations (module docs).
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        let resolved = service_view(
            &good_name,
            "192.168.1.50"
                .parse()
                .expect("Value expected to be present"),
            1716,
            &[("protocol", "8")],
        );

        let (identity, _) = resolved_to_identity(&resolved).expect("instance-name id must convert");
        assert_eq!(identity.device_id, "peer-device-aaaaaaaaaaaaaaaaaaaaaa");
        // Missing name/type get documented defaults.
        assert_eq!(identity.device_name, identity.device_id);
        assert_eq!(identity.device_type, "desktop");
    }

    #[test]
    fn test_resolved_to_identity_rejects_unusable_services() {
        let good_name = &format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}")[..];
        let good_ip: std::net::IpAddr = "192.168.1.50"
            .parse()
            .expect("Value expected to be present");

        // Missing protocol TXT: nothing to safely dial.
        let no_protocol = service_view(
            good_name,
            good_ip,
            1716,
            &[("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa")],
        );
        assert!(resolved_to_identity(&no_protocol).is_none());

        // Garbage protocol TXT.
        let bad_protocol = service_view(
            good_name,
            good_ip,
            1716,
            &[
                ("protocol", "eight"),
                ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
            ],
        );
        assert!(resolved_to_identity(&bad_protocol).is_none());

        // Invalid device id (path-unsafe).
        let bad_id = service_view(
            good_name,
            good_ip,
            1716,
            &[("protocol", "8"), ("id", "../etc/passwd")],
        );
        assert!(resolved_to_identity(&bad_id).is_none());

        // Only a public address: a LAN peer doesn't announce one.
        let public = service_view(
            good_name,
            "8.8.8.8".parse().expect("Value expected to be present"),
            1716,
            &[
                ("protocol", "8"),
                ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
            ],
        );
        assert!(resolved_to_identity(&public).is_none());
    }

    /// End-to-end, in-process: announce ourselves and confirm a browser
    /// resolves the service with the reference shape (instance = device id,
    /// port, TXT records). mDNS multicast is looped back by the local
    /// network stack, so no real network is involved.
    #[tokio::test]
    async fn test_announce_then_browse_resolves_ourselves() {
        let identity = our_identity();
        let service = MdnsDiscoveryService::new(&identity).expect("announce must succeed");
        assert_eq!(
            *service.fullname.read().expect("read fullname"),
            format!("{}.{}", identity.device_id, SERVICE_TYPE),
            "the instance name must be the device id"
        );

        // `run` takes `&self` (Task 2.2: a caller needs to hold the same
        // instance to call `reannounce` concurrently), so the spawned
        // 'static future needs an owned handle to borrow from.
        let service = Arc::new(service);
        let run_service = service.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let run_shutdown = shutdown.clone();
        let browser = tokio::spawn(async move {
            run_service
                .run(
                    move |identity, addr| {
                        let _ = tx.send((identity, addr));
                    },
                    run_shutdown,
                )
                .await;
        });

        let (discovered, addr) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if let Some((identity, addr)) = rx.recv().await {
                    if identity.device_id == our_identity().device_id {
                        break (identity, addr);
                    }
                }
            }
        })
        .await
        .expect("our own announcement must be resolved within 15s");

        assert_eq!(discovered.device_name, "mDNS Test Device");
        assert_eq!(discovered.device_type, "desktop");
        assert_eq!(discovered.protocol_version, 8);
        assert_eq!(discovered.tcp_port, Some(17161));
        assert_eq!(addr.port(), 17161);
        assert!(
            crate::protocol::is_private_address(&addr.ip()),
            "the resolved address must be a LAN address, got {}",
            addr.ip()
        );

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), browser)
            .await
            .expect("browsing must stop on shutdown")
            .expect("join");
    }

    /// D2 (2026-09-02 audit): `cargo test` must never publish a fixture
    /// that a real KDE Connect peer would act on. The `svc-mgr-peer-device…`
    /// and `mdns-our-device…` fixtures were discovered by the paired A15
    /// over `_kdeconnect._udp` and, on 2026-09-02, landed in the LIVE
    /// daemon's device registry on this host. Loopback scoping is not
    /// enough (Linux delivers looped multicast to every local member of
    /// the group), so test builds announce under a test-only service type
    /// that no phone and no production daemon browses.
    #[tokio::test]
    async fn test_announcer_in_test_builds_is_invisible_to_production_browsers() {
        let identity = our_identity();
        let _service = MdnsDiscoveryService::new(&identity).expect("announce");

        let browser = ServiceDaemon::new().expect("browser daemon");
        let receiver = browser
            .browse("_kdeconnect._udp.local.")
            .expect("browse the production service type");
        let seen = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                match receiver.recv_async().await {
                    Ok(ServiceEvent::ServiceResolved(resolved))
                        if resolved.get_fullname().starts_with(&identity.device_id) =>
                    {
                        break true;
                    }
                    Ok(_) => continue,
                    Err(_) => break false,
                }
            }
        })
        .await
        .unwrap_or(false);
        let _ = browser.shutdown();

        assert!(
            !seen,
            "a production-type browser resolved the test fixture {}: cargo test is announcing to real peers",
            identity.device_id
        );
    }

    /// Task 2.2 (vk #994): `reannounce` must cause a REAL, observable
    /// re-announcement — not just update internal bookkeeping. Proven by
    /// changing the identity's device name and confirming a browser
    /// resolves the NEW name after `reannounce`, attributable specifically
    /// to the explicit call (mdns-sd's own internal 5s auto-poll only
    /// refreshes addresses on the already-registered ServiceInfo; it has
    /// no way to pick up a changed device name on its own).
    #[tokio::test]
    async fn test_reannounce_publishes_a_real_update() {
        let identity = our_identity();
        let service = Arc::new(MdnsDiscoveryService::new(&identity).expect("announce"));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let run_service = service.clone();
        let run_shutdown = shutdown.clone();
        let browser = tokio::spawn(async move {
            run_service
                .run(
                    move |identity, addr| {
                        let _ = tx.send((identity, addr));
                    },
                    run_shutdown,
                )
                .await;
        });

        // Drain the initial announcement first so it can't be mistaken
        // for the post-reannounce one.
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if let Some((identity, _)) = rx.recv().await {
                    if identity.device_id == our_identity().device_id {
                        break;
                    }
                }
            }
        })
        .await
        .expect("initial announcement must be resolved within 15s");

        let mut renamed = identity.clone();
        renamed.device_name = "mDNS Test Device (renamed)".to_string();
        service
            .reannounce(&renamed)
            .expect("reannounce must succeed");

        let (discovered, _addr) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if let Some((identity, addr)) = rx.recv().await {
                    if identity.device_id == our_identity().device_id
                        && identity.device_name == "mDNS Test Device (renamed)"
                    {
                        break (identity, addr);
                    }
                }
            }
        })
        .await
        .expect("the reannounced identity must be resolved within 15s");

        assert_eq!(discovered.device_name, "mDNS Test Device (renamed)");

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), browser)
            .await
            .expect("browsing must stop on shutdown")
            .expect("join");
    }
}

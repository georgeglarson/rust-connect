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
//!   Android `onServiceResolved`) and let the peer dial. So do we
//!   (`service_manager::on_mdns_device_resolved` →
//!   `protocol::udp_unicast::unicast_identity`). Both references carry a
//!   TODO about dialing directly from the resolve under protocol v8; this
//!   crate did that from 2026-08 until vk #1101 (2026-09), when a peer
//!   announcing SRV port 0 showed why they haven't: kdeconnectd's announcer
//!   captures `LanLinkProvider::tcpPort()` at construction
//!   (`mdnshdiscovery.cpp:18`), which is 0 until `onStart` binds
//!   (`lanlinkprovider.cpp:54,139`). The SRV port is not trustworthy, and
//!   the identity exchange already carries the real one.
//! - IPv4 only on this path: the TCP listener is IPv4-only
//!   (`listener.rs:71`), so a unicast to a peer's IPv6 address would
//!   invite a dial nothing answers; `resolved_to_peer` skips IPv6-only
//!   resolves (`mdns_resolve_skipped`, `reason = "ipv6_only"`). Restoring
//!   IPv6 here means a dual-stack listener first (vk #1101 backlog).

use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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

/// Per-minute outbound-send threshold above which the storm sensor logs
/// `event = "mdns_send_storm"` at WARN. Set to 600/min ≈ 10/s; steady
/// state on laptop measured 2026-09-06 is ≈0.8/s, so this is ~12× normal
/// headroom but well below the ~6000/min observed in the 2026-09-04 storm.
pub(crate) const MDNS_STORM_THRESHOLD_PER_MIN: i64 = 600;

/// How often the browse loop asks the mdns-sd daemon for a metrics
/// snapshot and runs `storm_verdict` against the previous one. The brief
/// specified 60 s; with `MissedTickBehavior::Skip` a tick that fires while
/// the loop is busy elsewhere just gets dropped, so we never queue up a
/// storm of our own.
pub(crate) const MDNS_METRICS_SAMPLE_PERIOD: Duration = Duration::from_secs(60);

/// Counters that increment when the mdns-sd daemon actually puts a packet
/// on the wire — the OUTBOUND-SEND aggregate for `storm_verdict`. Citations
/// are against vendored mdns-sd 0.20.3 (`service_daemon.rs`):
///
/// * `register-resend` — proactive announcement refresh (line 4023,
///   inside `register_resend_service_info`)
/// * `unregister-resend` — goodbye packet retransmit (line 3926, inside
///   `unregister_service_with_response`)
/// * `respond` — packet-sent counter for query responses (lines 3456,
///   3511 — `send_response` and `send_delayed_response`)
/// * `cache-refresh-ptr` / `cache-refresh-srv-txt` / `cache-refresh-addr`
///   — packets-sent counter for proactive cache refresh queries
///   (lines 4103-4105, inside the timer-driven refresh path)
///
/// Explicitly NOT in the aggregate:
/// * `register` / `unregister` — incremented in the command handler when
///   the daemon *receives* the command (lines 3586, 3882), not on a
///   packet-sent path. Counting them would mix command frequency with
///   packet frequency.
/// * `known-answer-suppression` — counts suppressed answers, not sent
///   packets (line 3461).
/// * `timer`, `cached-*`, `dns-registry-*` — state counts (pending
///   timers, cache size, registry state), not packet counts.
const SEND_COUNTERS: &[&str] = &[
    "cache-refresh-addr",
    "cache-refresh-ptr",
    "cache-refresh-srv-txt",
    "register-resend",
    "respond",
    "unregister-resend",
];

/// One sampling window's worth of mdns-sd metrics. Returned by
/// `storm_verdict` only when the outbound-send aggregate is at or above
/// the per-sample threshold — `None` otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StormReport {
    /// Sum of the moved `SEND_COUNTERS` deltas over the window. Used as
    /// the threshold check value.
    pub sends: i64,
    /// All counters that moved strictly up over the window, sorted by
    /// name. Counters that went down (daemon restart, internal reset)
    /// contribute 0 and are absent — never a negative, never a panic.
    pub deltas: Vec<(String, i64)>,
}

/// Pure: given two mdns-sd metrics snapshots, return a `StormReport` iff
/// the outbound-send aggregate over the window is at or above
/// `threshold_per_sample`. The first sample is always silent (no previous
/// snapshot) — the caller is responsible for skipping it. A counter that
/// went DOWN contributes 0, never a negative, and never panics.
pub(crate) fn storm_verdict(
    prev: &mdns_sd::Metrics,
    cur: &mdns_sd::Metrics,
    threshold_per_sample: i64,
) -> Option<StormReport> {
    // BTreeSet gives us a sorted iteration over the union of keys —
    // satisfies "deltas sorted by name".
    let mut all_keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    all_keys.extend(prev.keys().map(String::as_str));
    all_keys.extend(cur.keys().map(String::as_str));

    let mut deltas: Vec<(String, i64)> = Vec::new();
    let mut sends: i64 = 0;
    for key in all_keys {
        let prev_val = prev.get(key).copied().unwrap_or(0);
        let cur_val = cur.get(key).copied().unwrap_or(0);
        let delta = cur_val.saturating_sub(prev_val);
        if delta > 0 {
            deltas.push((key.to_string(), delta));
            if SEND_COUNTERS.contains(&key) {
                sends += delta;
            }
        }
    }

    if sends >= threshold_per_sample {
        Some(StormReport { sends, deltas })
    } else {
        None
    }
}

/// One tick of the storm sampler: ask the daemon for a metrics snapshot,
/// compare it against the previous one (if any), and log accordingly.
/// Always a DEBUG on success and a WARN on storm. Every failure path logs
/// once at DEBUG and skips the sample — `last_metrics` is left untouched
/// so the next tick still has a valid baseline.
async fn sample_metrics(daemon: &Arc<ServiceDaemon>, last_metrics: &mut Option<mdns_sd::Metrics>) {
    let receiver = match daemon.get_metrics() {
        Ok(rx) => rx,
        Err(e) => {
            debug!(
                error = %e,
                event = "mdns_metrics_get_failed",
                "mDNS get_metrics command failed; skipping sample"
            );
            return;
        }
    };
    let cur = match tokio::time::timeout(Duration::from_secs(2), receiver.recv_async()).await {
        Ok(Ok(metrics)) => metrics,
        Ok(Err(_)) | Err(_) => {
            debug!(
                event = "mdns_metrics_recv_failed",
                "mDNS metrics snapshot unavailable; skipping sample"
            );
            return;
        }
    };

    // First sample is silent — store as baseline, no comparison.
    let Some(prev) = last_metrics.take() else {
        *last_metrics = Some(cur);
        return;
    };

    let report = storm_verdict(&prev, &cur, MDNS_STORM_THRESHOLD_PER_MIN);
    *last_metrics = Some(cur);

    if let Some(report) = report {
        let deltas = report
            .deltas
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        warn!(
            sends_per_minute = report.sends,
            threshold = MDNS_STORM_THRESHOLD_PER_MIN,
            deltas = %deltas,
            event = "mdns_send_storm",
            "mDNS send rate exceeded storm threshold"
        );
    } else {
        let sends: i64 = SEND_COUNTERS
            .iter()
            .map(|k| {
                let p = prev.get(*k).copied().unwrap_or(0);
                let c = last_metrics
                    .as_ref()
                    .and_then(|m| m.get(*k).copied())
                    .unwrap_or(0);
                c.saturating_sub(p)
            })
            .sum();
        debug!(
            sends_per_minute = sends,
            event = "mdns_metrics",
            "mDNS metrics sample"
        );
    }
}

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

        // Test builds must keep mDNS traffic on the loopback (D2 fixed
        // the TYPE so phones don't recognize fixture announces; this
        // gate keeps the multicast itself off the LAN so avahi/etc.
        // don't churn parsing it). The `ServiceDaemon` applies
        // `IfSelection` entries to its interface list as a sequence of
        // overrides (service_daemon.rs `apply_intf_selections`: starts
        // with every interface enabled, walks `if_selections`, last
        // match wins), so an `enable_interface(LoopbackV4)` alone is a
        // no-op — we must first `disable_interface(All)`, then
        // re-enable only loopback. Verified empirically against the
        // vendored mdns-sd 0.20.3 source.
        #[cfg(any(test, feature = "test-helpers"))]
        {
            daemon
                .disable_interface(mdns_sd::IfKind::All)
                .map_err(|e| {
                    Error::DiscoveryError(format!(
                        "Failed to disable non-loopback interfaces for test build: {}",
                        e
                    ))
                })?;
            daemon
                .enable_interface(mdns_sd::IfKind::LoopbackV4)
                .map_err(|e| {
                    Error::DiscoveryError(format!(
                        "Failed to enable loopback interface for test build: {}",
                        e
                    ))
                })?;
        }

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
    /// converted to an [`MdnsPeer`] — who and where, no port to dial —
    /// and handed to `on_resolved`. Takes `&self`, not `self`, so a
    /// caller can hold the SAME `Arc<MdnsDiscoveryService>` and call
    /// `reannounce` concurrently (Task 2.2) — previously this consumed
    /// `self`, which made that impossible.
    pub async fn run<F>(&self, on_resolved: F, shutdown: CancellationToken)
    where
        F: Fn(MdnsPeer),
    {
        let receiver = match self.daemon.browse(SERVICE_TYPE) {
            Ok(receiver) => receiver,
            Err(e) => {
                warn!(error = %e, event = "mdns_browse_failed", "Failed to start mDNS browsing");
                return;
            }
        };

        // Storm sensor (2026-09-04 forensics): a third select! arm that
        // asks the mdns-sd daemon for a metrics snapshot every
        // `MDNS_METRICS_SAMPLE_PERIOD` and logs the outbound-send
        // aggregate. The daemon's `get_metrics()` returns a one-shot
        // `Receiver<Metrics>` (`service_daemon.rs:610`), bounded(1), so
        // we await `recv_async` with a 2 s timeout — flume's waiters
        // are event-driven, so the timer bound is the ceiling, not the
        // floor. A failed get_metrics (queue full, daemon gone) logs
        // once at DEBUG and skips the sample; it must never end the
        // browse loop.
        let mut metrics_tick = tokio::time::interval(MDNS_METRICS_SAMPLE_PERIOD);
        metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_metrics: Option<mdns_sd::Metrics> = None;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                event = receiver.recv_async() => {
                    match event {
                        Ok(ServiceEvent::ServiceResolved(resolved)) => {
                            let service = ServiceView::from(&*resolved);
                            match resolved_to_peer(&service) {
                                Some(peer) => {
                                    debug!(
                                        device_id = %peer.device_id,
                                        address = %peer.address,
                                        srv_port = peer.srv_port,
                                        event = "mdns_service_resolved",
                                        "Resolved mDNS service"
                                    );
                                    on_resolved(peer);
                                }
                                None => {
                                    // `reason` is the one field the IPv6 decision
                                    // (vk #1101) needs to be findable in a journal.
                                    let reason = resolve_skip_reason(&service.addresses);
                                    debug!(
                                        fullname = %resolved.get_fullname(),
                                        reason = reason,
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
                _ = metrics_tick.tick() => {
                    sample_metrics(&self.daemon, &mut last_metrics).await;
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

/// What an mDNS resolve tells us about a peer: who they are and where
/// they are. Deliberately NOT an `Identity`, and deliberately carrying
/// no port to dial — the references never dial from a resolve (module
/// docs), and a peer's SRV port can be wrong: kdeconnectd announces the
/// port its `LanLinkProvider` had at construction time, which is 0 until
/// the TCP server binds (`mdnshdiscovery.cpp:18`,
/// `lanlinkprovider.cpp:54,139`; vk #1101). `srv_port` is kept for the
/// resolve log line only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsPeer {
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub protocol_version: u32,
    pub address: std::net::IpAddr,
    pub srv_port: u16,
}

/// The address rule of the mDNS leg, in ONE place (vk #1101 review): a
/// peer is reachable only at a private IPv4 address — the TCP listener
/// binds IPv4 only (`listener.rs:71`), so a unicast anywhere else invites
/// a dial nothing answers, and `ServiceView`'s bare `IpAddr`s make
/// link-local IPv6 unsendable. The selection in `resolved_to_peer` and
/// the skip diagnosis in `run` MUST share this predicate: two
/// hand-written copies of the rule existed for one day before this was
/// collapsed, and the dual-stack listener follow-up will change the rule
/// again — one edit point, or the journal line lies about the skip.
fn usable_peer_address(ip: &std::net::IpAddr) -> bool {
    ip.is_ipv4() && crate::protocol::is_private_address(ip)
}

/// Why a resolve `resolved_to_peer` rejected was skipped, for the
/// `mdns_resolve_skipped` journal line. `"ipv6_only"` is the findable
/// label the accepted IPv6 regression (vk #1101) needs; an IPv4-mapped
/// IPv6 (`::ffff:192.168.1.5`) lands there too — mdns-sd's AAAA decoder
/// (`read_ipv6`, dns_parser.rs) does not reject mapped rdata, and Rust
/// classifies it as V6, which the selection below agrees is unusable.
fn resolve_skip_reason(addresses: &[std::net::IpAddr]) -> &'static str {
    if addresses.iter().any(|ip| ip.is_ipv6()) && !addresses.iter().any(usable_peer_address) {
        "ipv6_only"
    } else {
        "unusable"
    }
}

/// Convert a resolved service to an [`MdnsPeer`]. `None` for anything that
/// is not a usable KDE Connect peer (missing/invalid TXT, no private IPv4
/// address). The SRV port is NOT validated: nothing dials it.
///
/// IPv4 only, on purpose (vk #1101): the handshake this feeds ends with
/// the PEER dialing OUR address, and the TCP listener binds IPv4 only
/// (`listener.rs:71`), so a unicast to a peer's IPv6 address invites a
/// dial nothing answers. Link-local IPv6 is worse still — `ServiceView`
/// carries bare `IpAddr`s, the scope id is gone, and the send itself
/// fails. Until the listener is dual-stack, an IPv6-only resolve is
/// skipped (logged `mdns_resolve_skipped`, `reason = "ipv6_only"`); such
/// a peer still connects by dialing us over v4 or via UDP broadcast.
fn resolved_to_peer(service: &ServiceView) -> Option<MdnsPeer> {
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

    // Without a parseable protocol version we can't safely identify the
    // peer (the downgrade guard and the identity exchange both key on it);
    // every real announcer sends it.
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

    // LAN protocol: a public address here is a spoof; an IPv6 address is
    // one we cannot complete the handshake on (doc comment above).
    let address = service
        .addresses
        .iter()
        .find(|ip| usable_peer_address(ip))?;

    Some(MdnsPeer {
        device_id,
        device_name,
        device_type,
        protocol_version,
        address: *address,
        srv_port: service.port,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::DeviceType;
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
    fn test_resolved_to_peer_reads_reference_shape() {
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        let ip: std::net::IpAddr = "192.168.1.50".parse().expect("ip");
        let resolved = service_view(
            &good_name,
            ip,
            1716,
            &[
                ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
                ("name", "test phone"),
                ("type", "phone"),
                ("protocol", "8"),
            ],
        );

        let peer = resolved_to_peer(&resolved).expect("a reference-shaped service must convert");
        assert_eq!(peer.device_id, "peer-device-aaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(peer.device_name, "test phone");
        assert_eq!(peer.device_type, "phone");
        assert_eq!(peer.protocol_version, 8);
        assert_eq!(peer.address, ip);
        assert_eq!(peer.srv_port, 1716);
    }

    #[test]
    fn test_resolved_to_peer_falls_back_to_instance_name_for_id() {
        // No `id` TXT: the instance name is the device id in both reference
        // implementations (module docs).
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        let resolved = service_view(
            &good_name,
            "192.168.1.50".parse().expect("ip"),
            1716,
            &[("protocol", "8")],
        );

        let peer = resolved_to_peer(&resolved).expect("instance-name id must convert");
        assert_eq!(peer.device_id, "peer-device-aaaaaaaaaaaaaaaaaaaaaa");
        // Missing name/type get documented defaults.
        assert_eq!(peer.device_name, peer.device_id);
        assert_eq!(peer.device_type, "desktop");
    }

    #[test]
    fn test_resolved_to_peer_rejects_unusable_services() {
        let good_name = &format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}")[..];
        let good_ip: std::net::IpAddr = "192.168.1.50".parse().expect("ip");

        // Missing protocol TXT: the downgrade guard keys on it.
        let no_protocol = service_view(
            good_name,
            good_ip,
            1716,
            &[("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa")],
        );
        assert!(resolved_to_peer(&no_protocol).is_none());

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
        assert!(resolved_to_peer(&bad_protocol).is_none());

        // Invalid device id (path-unsafe).
        let bad_id = service_view(
            good_name,
            good_ip,
            1716,
            &[("protocol", "8"), ("id", "../etc/passwd")],
        );
        assert!(resolved_to_peer(&bad_id).is_none());

        // Only a public address: a LAN peer doesn't announce one.
        let public = service_view(
            good_name,
            "8.8.8.8".parse().expect("ip"),
            1716,
            &[
                ("protocol", "8"),
                ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
            ],
        );
        assert!(resolved_to_peer(&public).is_none());
    }

    /// vk #1101: a zero SRV port is not a reason to drop the peer. We never
    /// dial it (module docs); it stays on the struct so the resolve log line
    /// shows what the peer announced — that is how a kdeconnectd announcing
    /// 0 becomes visible in a journal.
    #[test]
    fn test_resolved_to_peer_keeps_a_zero_srv_port_for_the_log() {
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        let ip: std::net::IpAddr = "192.168.1.50".parse().expect("ip");
        let resolved = service_view(
            &good_name,
            ip,
            0,
            &[
                ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
                ("protocol", "8"),
            ],
        );

        let peer = resolved_to_peer(&resolved).expect("SRV port 0 must still convert");
        assert_eq!(peer.srv_port, 0);
        assert_eq!(peer.address, ip);
    }

    /// vk #1101 accepted regression *(cypher, S3)*: a resolve whose only
    /// private address is IPv6 is skipped. Our TCP listener binds IPv4
    /// only (`listener.rs:71`), so a peer that received our unicast on
    /// v6 would dial an address nothing listens on; and `ServiceView::from`
    /// drops the scope id, so a link-local target cannot even be sent to.
    /// Red on main: `resolved_to_identity` accepts any private address.
    #[test]
    fn test_resolved_to_peer_skips_an_ipv6_only_service() {
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        for v6 in ["fd12:3456::10", "fe80::1"] {
            let resolved = service_view(
                &good_name,
                v6.parse().expect("ip"),
                1716,
                &[
                    ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
                    ("protocol", "8"),
                ],
            );
            assert!(
                resolved_to_peer(&resolved).is_none(),
                "an IPv6-only resolve ({v6}) must be skipped, not unicast to"
            );
        }
    }

    /// A dual-stack peer resolves to its private IPv4 address whatever
    /// order the addresses arrive in *(cypher, kimi-k3 missing test 4)*.
    #[test]
    fn test_resolved_to_peer_prefers_private_ipv4_over_ipv6() {
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        let v4: std::net::IpAddr = "192.168.1.50".parse().expect("ip");
        let v6: std::net::IpAddr = "fd12:3456::10".parse().expect("ip");
        let txt: std::collections::HashMap<String, String> = [
            ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
            ("protocol", "8"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for addresses in [vec![v6, v4], vec![v4, v6]] {
            let view = ServiceView {
                fullname: &good_name,
                port: 1716,
                addresses,
                txt: txt.clone(),
            };
            let peer = resolved_to_peer(&view).expect("a dual-stack peer converts");
            assert_eq!(peer.address, v4);
        }
    }

    /// The skip diagnosis must name the IPv6 regression wherever an IPv6
    /// address is present and no usable IPv4 is — including an
    /// IPv4-mapped IPv6 (mdns-sd's AAAA decoder does not reject mapped
    /// rdata, so an announcer can plant one; Rust classifies it V6, and
    /// the selection treats it as unusable, which is safe: the unicast
    /// primitive binds per-family, and the ratified IPv4-only rule
    /// (vk #1101) says do not send there).
    #[test]
    fn test_resolve_skip_reason_labels_the_diagnosis() {
        let v4: std::net::IpAddr = "192.168.1.50".parse().expect("ip");
        let public_v4: std::net::IpAddr = "8.8.8.8".parse().expect("ip");
        let ula: std::net::IpAddr = "fd12:3456::10".parse().expect("ip");
        let mapped: std::net::IpAddr = "::ffff:192.168.1.5".parse().expect("ip");

        assert_eq!(resolve_skip_reason(&[ula]), "ipv6_only");
        assert_eq!(resolve_skip_reason(&[mapped]), "ipv6_only");
        assert_eq!(resolve_skip_reason(&[ula, public_v4]), "ipv6_only");
        assert_eq!(resolve_skip_reason(&[public_v4]), "unusable");
        assert_eq!(resolve_skip_reason(&[]), "unusable");
        // A usable IPv4 present: never labeled ipv6_only (this input
        // converts rather than skips; the tripwire below pins that the
        // two sites keep agreeing about it).
        assert_eq!(resolve_skip_reason(&[ula, v4]), "unusable");
    }

    /// Drift tripwire (vk #1101 review): the selection and the skip
    /// diagnosis share one predicate (`usable_peer_address`). If they
    /// ever disagree, a peer the selection ACCEPTS could be diagnosed
    /// `ipv6_only` in the journal, or a rejected one could lose the
    /// label. For every address set: accepted ⇔ not diagnosed ipv6_only.
    #[test]
    fn test_resolve_skip_reason_never_contradicts_resolved_to_peer() {
        let v4: std::net::IpAddr = "192.168.1.50".parse().expect("ip");
        let public_v4: std::net::IpAddr = "8.8.8.8".parse().expect("ip");
        let ula: std::net::IpAddr = "fd12:3456::10".parse().expect("ip");
        let mapped: std::net::IpAddr = "::ffff:192.168.1.5".parse().expect("ip");
        let good_name = format!("peer-device-aaaaaaaaaaaaaaaaaaaaaa.{SERVICE_TYPE}");
        let txt: std::collections::HashMap<String, String> = [
            ("id", "peer-device-aaaaaaaaaaaaaaaaaaaaaa"),
            ("protocol", "8"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let sets: Vec<Vec<std::net::IpAddr>> = vec![
            vec![v4],
            vec![public_v4],
            vec![ula],
            vec![mapped],
            vec![v4, ula],
            vec![ula, v4],
            vec![public_v4, ula],
            vec![mapped, v4],
            vec![mapped, public_v4],
        ];
        for addresses in sets {
            let view = ServiceView {
                fullname: &good_name,
                port: 1716,
                addresses: addresses.clone(),
                txt: txt.clone(),
            };
            let accepted = resolved_to_peer(&view).is_some();
            let labeled_ipv6_only = resolve_skip_reason(&addresses) == "ipv6_only";
            // A set the selection accepts must never be diagnosed
            // ipv6_only. (The reverse — rejected + "unusable" — is a
            // legitimate pairing, e.g. a public-IPv4-only spoof.)
            assert!(
                !(accepted && labeled_ipv6_only),
                "selection and diagnosis disagree for {addresses:?}"
            );
        }
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
                    move |peer| {
                        let _ = tx.send(peer);
                    },
                    run_shutdown,
                )
                .await;
        });

        let discovered = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if let Some(peer) = rx.recv().await {
                    if peer.device_id == our_identity().device_id {
                        break peer;
                    }
                }
            }
        })
        .await
        .expect("our own announcement must be resolved within 15s");

        assert_eq!(discovered.device_name, "mDNS Test Device");
        assert_eq!(discovered.device_type, "desktop");
        assert_eq!(discovered.protocol_version, 8);
        assert_eq!(discovered.srv_port, 17161);
        assert!(
            crate::protocol::is_private_address(&discovered.address),
            "the resolved address must be a LAN address, got {}",
            discovered.address
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

    /// 2026-09-04 storm forensics: `storm_verdict` is the pure decision
    /// the sampler in `run` calls once per minute. The aggregate is the
    /// OUTBOUND-PACKET-SENT subset of mdns-sd counters (see `SEND_COUNTERS`
    /// for the citations); a counter that went DOWN (daemon restart,
    /// counter reset) contributes 0 — never a negative, never a panic.
    #[test]
    fn test_storm_verdict_register_resend_over_threshold() {
        let mut prev = mdns_sd::Metrics::new();
        prev.insert("register-resend".to_string(), 100);
        let mut cur = mdns_sd::Metrics::new();
        cur.insert("register-resend".to_string(), 800);

        let report = storm_verdict(&prev, &cur, 600)
            .expect("700 outbound sends per minute must exceed threshold 600");
        assert_eq!(report.sends, 700);
        assert!(
            report
                .deltas
                .contains(&("register-resend".to_string(), 700)),
            "deltas must contain the moved counter, got {:?}",
            report.deltas
        );

        assert!(
            storm_verdict(&prev, &cur, 800).is_none(),
            "700 sends must NOT exceed threshold 800"
        );
    }

    /// A counter that went DOWN (daemon restart, counter reset) must
    /// contribute 0, not a negative, and never produce a storm verdict by
    /// itself — `None` at any positive threshold.
    #[test]
    fn test_storm_verdict_counter_reset_contributes_zero() {
        let mut prev = mdns_sd::Metrics::new();
        prev.insert("register-resend".to_string(), 1000);
        prev.insert("respond".to_string(), 500);
        let mut cur = mdns_sd::Metrics::new();
        // register-resend DROPPED (counter reset / daemon restart).
        cur.insert("register-resend".to_string(), 0);
        // respond ALSO dropped (separate reset).
        cur.insert("respond".to_string(), 50);

        for threshold in [1, 100, 600, 100_000] {
            assert!(
                storm_verdict(&prev, &cur, threshold).is_none(),
                "counter-reset snapshot must not produce a storm at threshold {}",
                threshold
            );
        }
    }

    /// No entry in `StormReport::deltas` may be negative, even when a
    /// non-send counter resets during the sampling window. The brief's
    /// hard rule: a counter that went DOWN contributes 0, never a
    /// negative, never a panic. Mixed scenario: `register-resend` UP
    /// by 700 (real sends), `respond` DOWN by 450 (counter reset).
    /// Verdict must be Some, and the `deltas` field must contain ONLY
    /// `register-resend` (positive) — the reset counter is invisible.
    #[test]
    fn test_storm_verdict_deltas_never_negative() {
        let mut prev = mdns_sd::Metrics::new();
        prev.insert("register-resend".to_string(), 100);
        prev.insert("respond".to_string(), 500);
        let mut cur = mdns_sd::Metrics::new();
        cur.insert("register-resend".to_string(), 800); // +700
        cur.insert("respond".to_string(), 50); // -450 reset

        let report =
            storm_verdict(&prev, &cur, 600).expect("700 outbound sends must trigger the storm");
        assert_eq!(report.sends, 700);
        for (name, delta) in &report.deltas {
            assert!(
                *delta >= 0,
                "no delta may be negative, but {} = {}",
                name,
                delta
            );
        }
        assert!(
            !report.deltas.iter().any(|(n, _)| n == "respond"),
            "a reset counter must NOT appear in deltas (got {:?})",
            report.deltas
        );
        assert!(
            report
                .deltas
                .contains(&("register-resend".to_string(), 700)),
            "deltas must contain the moved register-resend, got {:?}",
            report.deltas
        );
    }

    /// Cache churn (`cached-ptr` bouncing) is NOT a send storm — it's a
    /// state counter, not an outbound-packet counter, and must never
    /// trigger the verdict regardless of how big the delta is.
    #[test]
    fn test_storm_verdict_cache_churn_is_not_a_storm() {
        let mut prev = mdns_sd::Metrics::new();
        prev.insert("cached-ptr".to_string(), 0);
        let mut cur = mdns_sd::Metrics::new();
        cur.insert("cached-ptr".to_string(), 10_000);

        for threshold in [1, 100, 600, 10_000_000] {
            assert!(
                storm_verdict(&prev, &cur, threshold).is_none(),
                "cached-ptr churn alone must not be a storm at threshold {}",
                threshold
            );
        }
    }

    /// The sampler arm in `run` must NOT kill the browse loop. Drive `run`
    /// with `start_paused = true`, advance 61 s so the 60 s metrics
    /// interval fires at least once, then cancel the shutdown token and
    /// verify the task returns cleanly.
    ///
    /// Uses a unique device id so this test's daemon doesn't contend
    /// with the other `our_identity()`-using tests in parallel runs.
    #[tokio::test(start_paused = true)]
    async fn test_run_sampler_arm_does_not_kill_loop() {
        let mut identity = our_identity();
        identity.device_id = format!("sampler-arm-test-{}", uuid::Uuid::new_v4());
        let service = Arc::new(MdnsDiscoveryService::new(&identity).expect("announce"));

        let (tx, _rx) = mpsc::unbounded_channel::<MdnsPeer>();
        let shutdown = CancellationToken::new();
        let run_service = service.clone();
        let run_shutdown = shutdown.clone();
        let browser = tokio::spawn(async move {
            run_service
                .run(
                    move |peer| {
                        let _ = tx.send(peer);
                    },
                    run_shutdown,
                )
                .await;
        });

        // Advance past one full metrics interval. The first sample is
        // silent (no previous snapshot); the call to `get_metrics` plus
        // the recv-with-timeout happens entirely inside the sampler arm
        // and must not end the loop.
        tokio::time::advance(std::time::Duration::from_secs(61)).await;

        assert!(
            !browser.is_finished(),
            "the browse loop must still be running after one metrics tick"
        );

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), browser)
            .await
            .expect("browsing must stop on shutdown")
            .expect("join");
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
                    move |peer| {
                        let _ = tx.send(peer);
                    },
                    run_shutdown,
                )
                .await;
        });

        // Drain the initial announcement first so it can't be mistaken
        // for the post-reannounce one.
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if let Some(peer) = rx.recv().await {
                    if peer.device_id == our_identity().device_id {
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

        let discovered = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if let Some(peer) = rx.recv().await {
                    if peer.device_id == our_identity().device_id
                        && peer.device_name == "mDNS Test Device (renamed)"
                    {
                        break peer;
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

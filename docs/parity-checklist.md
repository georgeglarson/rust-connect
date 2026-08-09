# Behavioral parity checklist — rust-connect vs the reference implementations

Audit date: 2026-08-04. Revised 2026-08-04: gaps 1/2/4/5 fixed 2026-08-04
(gap-1/2/4/5 rows updated to CONFORMANT). Revised 2026-08-09 (Task 2.1,
vk #997): Robustness gap 2 (payload accept timeout) fixed, row updated to
CONFORMANT*. Sources:

- **kde** = kdeconnect-kde, local clone `~/repos/kdeconnect-kde` (paths relative to it).
- **android** = kdeconnect-android. `docs/reference/LanLinkProvider.java` and
  `docs/reference/PairingHandler.kt` are the pinned vendored copies (marked **V**);
  files not vendored were audited from GitHub master (marked **M**, with `#line`
  approximate). Behavioral answers agreed in both where they overlapped.
- **rust** = this repo.

Status: **CONFORMANT** (matches at least one reference, and mismatches none) /
**CONFORMANT\*** (matches one reference; the other differs or the constant
differs — see note) / **DIVERGENT** (differs from the references' behavior) /
**UNIMPLEMENTED** (reference behavior absent) / **N/A** (concept doesn't apply).
The Gaps section at the end lists only DIVERGENT / UNIMPLEMENTED rows, ranked.

---

## Discovery

| Behavior | kde | android | rust status | rust ref |
|---|---|---|---|---|
| Broadcast cadence | No periodic timer; on start, network change, rename, custom-device change (`core/backends/lan/lanlinkprovider.cpp:149,192`, `core/daemon.cpp:282,296`) | On start + network change, min gap 200 ms (V `LanLinkProvider.java:567,573-577`) | **DIVERGENT** — periodic broadcast forever, 60 s default (deliberate pre-mDNS; revisit after mDNS live-validation, see `service_manager.rs` TODO) | `src/config/settings.rs:13`, `src/protocol/discovery.rs:235` |
| Immediate re-broadcast on network change | Yes (`lanlinkprovider.cpp:180-194`) | Yes (V `LanLinkProvider.java:572-584`) | **UNIMPLEMENTED** — no network-change hook | — |
| Broadcast destination | 255.255.255.255 per-interface (source-bound) + custom devices (`lanlinkprovider.cpp:207-248`) | 255.255.255.255 (trusted nets only) + custom devices (V `LanLinkProvider.java:474-481`) | **CONFORMANT\*** — 255.255.255.255 only, single socket; no per-interface binding, no custom-device list (manual connect via API instead) | `src/protocol/discovery.rs:97` |
| Ports: UDP 1716, TCP first-free 1716-1764 | `lanlinkprovider.h:67-69`, `lanlinkprovider.cpp:139-147` | V `LanLinkProvider.java:64-65,454-470` | **CONFORMANT** | `src/protocol/types.rs:15-23`, `src/protocol/listener.rs` `bind_port` |
| Oversized identity fallback (re-send with emptied capabilities) | On `DatagramTooLargeError` (`lanlinkprovider.cpp:259-269`) | Absent | **UNIMPLEMENTED** (kde-only behavior) | — |
| UDP receive buffer | (Qt datagram) | 512 KiB (V `LanLinkProvider.java:69`) | **DIVERGENT** — 64 KiB; a >64 KiB identity is truncated and dropped | `src/protocol/discovery.rs:136` |
| Receiver dials sender's TCP port on UDP identity (any device, not paired-only) | `lanlinkprovider.cpp:331-339` | V `LanLinkProvider.java:236-252` | **CONFORMANT** | `src/services/service_manager.rs:149` |
| Received identity tcpPort outside 1716-1764 dropped | `lanlinkprovider.cpp:316-320` | V `LanLinkProvider.java:236-243` | **CONFORMANT** | `src/protocol/discovery.rs:177-182` |
| Reverse-connection fallback (dial fails → send UDP identity back) | `lanlinkprovider.cpp:343-354,395-399` | Absent | **UNIMPLEMENTED** (kde-only) | — |
| mDNS service type `_kdeconnect._udp(.local)` | `core/backends/lan/mdnshdiscovery.cpp:15`, `avahidiscovery.cpp:16` | `MdnsDiscovery.kt` `SERVICE_TYPE` | **CONFORMANT** | `src/protocol/mdns_discovery.rs:40` |
| mDNS instance = deviceId, port = TCP port, TXT id/name/type/protocol | `mdnshdiscovery.cpp:18-24`, `avahidiscovery.cpp:136-149` | `MdnsDiscovery.kt` `createNsdServiceInfo` | **CONFORMANT** | `src/protocol/mdns_discovery.rs` `MdnsDiscoveryService::new` |
| mDNS resolve behavior | Send UDP identity to resolved address; v8 dial-direct is their TODO (`mdnshdiscovery.cpp:31-36`) | Same (M `MdnsDiscovery.kt` `onServiceResolved` + TODO) | **CONFORMANT** — implements the v8 dial-direct path both refs defer to a TODO | `src/services/service_manager.rs:158` |
| Self-ignore (own deviceId) on UDP + TCP + mDNS | `lanlinkprovider.cpp:304-307`, `mdnshdiscovery.cpp:27-30` | V `LanLinkProvider.java:111-116`, `MdnsDiscovery.kt` `onServiceFound` | **CONFORMANT** | `src/protocol/discovery.rs:185`, `src/protocol/connection/inbound.rs:103`, `src/services/service_manager.rs` (self-guard) |
| Private-IP guard, incoming TCP | Absent | V `LanLinkProvider.java:138-141` | **CONFORMANT** (android) | `src/protocol/connection/inbound.rs:44` |
| Private-IP guard, UDP | Loopback-sender skip only (`lanlinkprovider.cpp:284`) | V `LanLinkProvider.java:215-218` | **CONFORMANT** | `src/protocol/discovery.rs:146-151` |
| Same-device/IP redial rate limit | 500 ms (`lanlinkprovider.cpp:49,309-314`) | 1000 ms, per-IP AND per-device (V `LanLinkProvider.java:71,183-207`) | **CONFORMANT\*** — 1000 ms, per-IP only | `src/protocol/connection/mod.rs:59`, `outbound.rs:101-112` |
| targetDeviceId / targetProtocolVersion mismatch → reject | `lanlinkprovider.cpp:536-545` | V `LanLinkProvider.java:169-178` | **CONFORMANT** | `src/protocol/connection/inbound.rs:111-126` |
| Protocol-downgrade refusal (paired device announcing lower version) | `lanlinkprovider.cpp:322-327` | V `LanLinkProvider.java:280-283,351-354` | **CONFORMANT** | `src/services/connection_orchestrator.rs:231-247` |
| Trusted-network gating (announce/receive only on trusted SSIDs) | Absent | V `LanLinkProvider.java:123-127,477-481,562-564`, M `TrustedNetworkHelper.kt#59-60` | **N/A** — no per-SSID trust concept on desktop; kde doesn't gate either | — |
| Unpaired/inbound connection cap | 42 unpaired (`lanlinkprovider.cpp:47,666-671`) | Absent | **CONFORMANT\*** — 64 inbound handler slots, same DoS purpose | `src/protocol/listener.rs:23` |

## Link layer

| Behavior | kde | android | rust status | rust ref |
|---|---|---|---|---|
| TLS role = reverse of TCP role (acceptor is TLS client, dialer is TLS server) | `lanlinkprovider.cpp:391,573` | V `LanLinkProvider.java:292-294` | **CONFORMANT** | `src/protocol/connection/inbound.rs:137`, `outbound.rs` (`tls_accept`) |
| Cert CN == deviceId enforced on peer cert | `lanlinkprovider.cpp:637-645` | Not enforced (TOFU only; V `SslHelper.kt:77-80,152-155`) | **CONFORMANT** (kde; stricter than android) | `src/protocol/connection/inbound.rs:145`, `outbound.rs:244` |
| v8 encrypted identity re-exchange; mid-handshake deviceId/protocolVersion change → abort | `lanlinkprovider.cpp:434-445` | V `LanLinkProvider.java:301-327` | **CONFORMANT** | `src/protocol/listener.rs:324,366`, `outbound.rs` (`expected_identity`) |
| Duplicate link: same cert → new socket adopted, old closed; different cert → reject new | `lanlinkprovider.cpp:655-660`, `landevicelink.cpp:29-41` | V `LanLinkProvider.java:367-373`, M `LanLink.java#68-107` | **CONFORMANT** (post 60b126f fix) | `src/protocol/connection/inbound.rs:205-220` |
| TCP keepalive 30 s idle / 10 s interval / 3 probes, both directions | `lanlinkprovider.cpp:618-634` | `setKeepAlive(true)` both directions, no params (V `LanLinkProvider.java:244,437`) | **CONFORMANT** | `src/protocol/keepalive.rs:19-21`, applied `inbound.rs:39`, `outbound.rs:39,125` |
| Framing: newline-terminated JSON | `core/networkpacket.cpp:59` | M `NetworkPacket.kt#225` | **CONFORMANT** | `src/protocol/packet.rs:41-48` |
| Identity line size cap | 8192 (`lanlinkprovider.cpp:45`) | 512 KiB (V `LanLinkProvider.java:68`) | **CONFORMANT** (android) — 512 KiB | `src/protocol/connection/mod.rs:55` |
| Steady-state packet size cap + behavior on oversize | 32 MiB, discard buffer, **continue** (`landevicelink.cpp:19,98-101`) | 32 MiB, skip line, **continue** (M `LanLink.java#46,85-88`) | **CONFORMANT** — 32 MiB cap, oversize line consumed and skipped, link kept alive (fixed 2026-08-04) | `src/protocol/connection/mod.rs:59,404-433` (`read_steady_line`) |
| Empty lines skipped | Yes (unserialize-false skip, `networkpacket.cpp:65-73`) | Yes (M `LanLink.java#89-91`) | **CONFORMANT** — blank lines ignored (fixed 2026-08-04) | `src/protocol/connection/mod.rs:437-447` |
| Identity required as first TCP packet | Yes; 1 s abort timer (`lanlinkprovider.cpp:492-500`) | Yes; no pre-TLS timeout, 10 s `SO_TIMEOUT` after upgrade (V `SslHelper.kt:176`) | **CONFORMANT\*** — required, 10 s pre-TLS read timeout | `src/protocol/connection/inbound.rs` (identity read + `ConnectionTimeout`) |
| Steady-state read timeout tolerated (keepalive keeps link) | (Qt async reads) | 10 s timeout caught, loop continues (M `LanLink.java#86-88`) | **CONFORMANT** — 30 s recv timeout → loop continues | `src/protocol/connection_loop.rs:270-276` |
| Unpaired device sends non-pair packet | Reply `{pair:false}` (`core/device.cpp:391-394`, which calls `PairingHandler::unpair()` → `pairinghandler.cpp:153-158`) | Auto-unpair + notify plugins (M `Device.kt#424-436`) | **CONFORMANT** — replied `{pair:false}` once per unpaired stretch on the link (fixed 2026-08-04) | `src/protocol/connection_loop.rs:49-74,253-270` |

## Pairing

| Behavior | kde | android | rust status | rust ref |
|---|---|---|---|---|
| Timeouts: requester 30 s, accepter 25 s | 30 s for both (`core/backends/pairinghandler.h:20`) | 30 s / 25 s (V `PairingHandler.kt:87-92,150-155`) | **CONFORMANT** (android) | `src/protocol/pairing/mod.rs:29,34` |
| pair=true while Requested → pairing completes | `pairinghandler.cpp:34-36,169-174` | V `PairingHandler.kt:52,202-211` | **CONFORMANT** | `src/protocol/connection_loop.rs:96-109` |
| pair=true while NotPaired → RequestedByPeer; v8 requires timestamp, ±1800 s window | `pairinghandler.cpp:51-63`, `:16` | V `PairingHandler.kt:60-95,228` | **CONFORMANT** | `src/protocol/connection_loop.rs:126-154`, `src/protocol/pairing/mod.rs:240-321` |
| pair=true while RequestedByPeer → duplicate ignored | `pairinghandler.cpp:37-39` | V `PairingHandler.kt:53-58` | **CONFORMANT** | `src/protocol/pairing/mod.rs:281-290` |
| pair=true while Paired → unpair both locally, treat as fresh request | `pairinghandler.cpp:40-49` | V `PairingHandler.kt:60-68` | **CONFORMANT** | `src/protocol/pairing/mod.rs:257-264` |
| pair=false matrix: NotPaired ignore; Requested/RequestedByPeer fail+clear; Paired unpair | `pairinghandler.cpp:74-85` | V `PairingHandler.kt:97-113` | **CONFORMANT\*** — also disconnects the link on Requested/RequestedByPeer/Paired (deliberate; kde does not) | `src/protocol/connection_loop.rs:193-233` |
| Requester-side timeout expiry sends `{pair:false}` | Yes (`pairinghandler.cpp:161-167`) | Timer → NotPaired + pairingFailed, no packet observed | **CONFORMANT** (android) — expiry drops state, no packet | `src/protocol/pairing/mod.rs:323-349` |
| SAS: SHA-256 over sorted (larger-first) public-key DERs + decimal timestamp, first 8 hex uppercase | `pairinghandler.cpp:176-195` | V `PairingHandler.kt:239-255` | **CONFORMANT** | `src/protocol/pairing/mod.rs:509-568` |
| Local unpair notifies reachable peer with `{pair:false}` | `pairinghandler.cpp:153-159` | (unpair path) | **CONFORMANT** | `src/api/handlers/device.rs:249-257` |
| Cert persisted only at pairing confirmation (verify-before-write) | (Qt trust store at pairing) | M `Device.kt#212-216` | **CONFORMANT** | `src/protocol/pairing/mod.rs:358-366` |

## Packet handling

| Behavior | kde | android | rust status | rust ref |
|---|---|---|---|---|
| Unknown packet type: log, don't crash; payload not leaked | `core/device.cpp:379-387` | M `Device.kt#439-447` | **CONFORMANT** — router warns and drops; no payload socket is ever opened for an unclaimed packet (payloads are outbound dials here) | `src/protocol/router.rs` |
| Malformed JSON line | Skip, continue (`networkpacket.cpp:65-73`) | Close socket (M `LanLink.java#92-103`) | **CONFORMANT** (android) — link death | `src/protocol/connection/mod.rs` (`recv_packet_inner`) |
| Missing `type`/`body` fields | Defaults, lenient (`networkpacket.cpp:75-79`) | JSONException → close (M `NetworkPacket.kt#293-304`) | **CONFORMANT** (android) — required fields, failure is link-fatal | `src/protocol/types.rs:216-233` |
| `id` field type tolerance | Never read — total tolerance (`networkpacket.cpp:46`) | Never read (M `NetworkPacket.kt#295-303`) | **CONFORMANT** — lenient string/number deserializer (fixed 2026-08-04) | `src/protocol/types.rs:82-99` |
| Multiple/partial packets per read | `while (canReadLine())` buffering (`landevicelink.cpp:102-132`) | Persistent buffered reader (M `LanLink.java#81-94`) | **CONFORMANT** — bounded line reads; partial line waits for more data | `src/protocol/connection/mod.rs:71-99` |

## Payload transfers

| Behavior | kde | android | rust status | rust ref |
|---|---|---|---|---|
| `payloadTransferInfo` port range 1739-1764 | `compositeuploadjob.h:69-70` | V `LanLinkProvider.java:66` | **CONFORMANT** | `src/protocol/payload_transfer.rs:29-30` |
| Payload sockets are TLS; sender = TLS server, receiver = TLS client, trusted-device context | `landevicelink.cpp:113-129`, `compositeuploadjob.cpp:168-170` | M `LanLink.java#205,254` | **CONFORMANT** | `src/protocol/payload_transfer.rs` (`connect_receiver`, `send_file`) |
| Accept timeout (sender waiting for receiver) | 30 s (`compositeuploadjob.cpp:35-37,231-242`) | 10 s (M `LanLink.java#200`) | **CONFORMANT\*** — 30 s, matching kde (the desktop reference); android's 10 s differs | `src/protocol/payload_transfer.rs:41` |
| Receiver connect timeout | Absent (Qt default) | (platform default) | **CONFORMANT\*** — explicit 30 s | `src/protocol/payload_transfer.rs:31,215` |
| payloadSize vs actual bytes: short read → error + delete; over-read tolerated | `core/filetransferjob.cpp:111-122` | Any mismatch → delete + throw (M `CompositeReceiveFileJob.java#158-163`) | **CONFORMANT\*** — reads exactly N; short = error + delete; over silently truncated at N | `src/protocol/payload_transfer.rs:245,263-270` |
| payloadSize = -1 endless-stream sentinel | Supported (`core/networkpacket.h:85`, `filetransferjob.cpp:109-110`) | Not used by android share (mismatch errors anyway) | **UNIMPLEMENTED** — `payloadSize` is `Option<u64>`; -1 not representable | `src/protocol/types.rs:225` |
| `{port}`-only payloadTransferInfo → address from the live link | (receiver dials sender's link address) | M `LanLink.java#248-261` | **CONFORMANT** | `src/plugins/share.rs` (`resolve_transfer_info`) |

## Lifecycle

| Behavior | kde | android | rust status | rust ref |
|---|---|---|---|---|
| Plugin init on connect for paired devices | Plugin `connected()` hook (`core/device.cpp:160,184`) | Plugins reloaded, no init packets sent (M `Device.kt#315-350,656`) | **CONFORMANT** (kde-style) — init packets on connect-if-paired and on pair completion | `src/protocol/listener.rs:267-277`, `src/protocol/connection_loop.rs:109,183` |
| Capabilities advertised in every identity | `core/deviceinfo.h:123-133` | M `DeviceInfo.kt#64-65` | **CONFORMANT** | `src/protocol/types.rs` (`Identity`) |
| Capability update applied only when both lists non-empty | `core/device.cpp:319-328` | `updateDeviceInfo` on change (M `Device.kt#383-405`) | **DIVERGENT** — upsert overwrites caps from any identity, including empty ones (mDNS resolve path is guarded separately) | `src/device/registry.rs:64-68`, `src/services/service_manager.rs` (mDNS guard) |
| Send-side capability gating (refuse types the peer didn't advertise) | `core/device.cpp:358-363` | Absent | **UNIMPLEMENTED** (kde-only; peer would ignore the packet anyway) | — |
| Reachable = has live link | `core/device.cpp:110-113,291-294,348-351` | M `Device.kt#312-313,362-368` | **CONFORMANT** | `src/device/lifecycle` |
| Unreachable + unpaired devices purged | `core/daemon.cpp:268-270` | (registry model) | **CONFORMANT\*** — registry persists them (history-first model) | `src/device/registry.rs` |

---

## Gaps (DIVERGENT / UNIMPLEMENTED only, ranked)

> Fixed since the initial audit (2026-08-04): the audit's gaps 1
> (unpaired peer told `{pair:false}`), 2 (32 MiB steady cap,
> skip-and-continue), 4 (blank lines skipped), 5 (lenient `id`) — all four
> rows above now read CONFORMANT. The list below is renumbered to only the
> still-open gaps (the audit's 3 and 6-13).

### User-visible breakage

1. **Broadcast-forever cadence.** Both references broadcast only on start /
   network change (kde `lanlinkprovider.cpp:149,192`; android V
   `LanLinkProvider.java:567,572-584`); rust-connect broadcasts every 60 s
   forever (`src/config/settings.rs:13`). Deliberate pre-mDNS; the follow-up
   is already noted in `service_manager.rs` pending live mDNS validation.

### Robustness

3. **Capability overwrite on empty-cap identity.** kde applies capability
   updates only when both lists are non-empty (`core/device.cpp:319-328`);
   rust upsert overwrites unconditionally (`src/device/registry.rs:64-68`).
   Only reachable with a hand-crafted identity today (real peers always send
   caps; the mDNS path is guarded separately).
4. **UDP receive buffer 64 KiB** vs android's 512 KiB
   (`src/protocol/discovery.rs:136`). An identity with a very large
   capability list would be truncated and dropped.

### Cosmetic / interop edge

5. No network-change re-broadcast trigger (both refs have one).
6. No reverse-connection fallback when a dial fails (kde-only,
   `lanlinkprovider.cpp:343-354`).
7. No oversized-identity emptied-caps fallback (kde-only,
   `lanlinkprovider.cpp:259-269`).
8. `payloadSize = -1` endless-stream sentinel unsupported (kde-only,
   `core/networkpacket.h:85`); rust uses `Option<u64>`.
9. No send-side capability gating (kde-only, `core/device.cpp:360-363`).

---

## Coverage

All sections complete: Discovery, Link layer, Pairing, Packet handling,
Payload transfers, Lifecycle. Plugin-level parity (per-plugin packet shapes)
is intentionally out of scope — that surface is enumerable and already
covered by prior audits; this checklist exists for the *behavioral* layer
the packet spec doesn't mention.

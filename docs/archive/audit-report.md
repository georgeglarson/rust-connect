# Comprehensive Audit Report — rust-connect

**Date**: 2026-03-31
**Scope**: Full codebase — connection lifecycle, protocol, plugins, API, security, concurrency, error handling
**Method**: 4 parallel deep audits (connection lifecycle, protocol correctness, plugin system, API/security)

---

## CRITICAL (11)

| # | Issue | Location | Impact |
|---|-------|----------|--------|
| 1 | **No read/write timeout on `recv_packet`/`send_packet`** — connections hang forever on half-open or stalled peers. `CancellationToken` is not checked during blocking reads. | `connection.rs:491,530` | DoS — single misbehaving peer permanently leaks a tokio task |
| 2 | **No timeout on plaintext identity read** — incoming TCP connections that send partial data and stall hang forever | `connection.rs:247,314` | DoS — unbounded task spawning, resource exhaustion |
| 3 | **No timeout on TLS handshake in `native_tls_connect`** — non-responding peer hangs handshake forever | `connection.rs:431` | DoS — task leak during TLS negotiation |
| 4 | **No timeout on `TcpStream::connect()`** — outgoing connections to unreachable hosts hang up to OS TCP timeout (2+ min on Linux) | `connection.rs:85,165` | DoS — delayed shutdown, wasted resources |
| 5 | **Auto-pair bypass** — `connection_loop.rs:40-48` auto-creates a pending request and immediately accepts it when none exists. Any device that connects and sends `pair:true` pairs automatically with zero user confirmation. | `connection_loop.rs:40-48` | Unauthorized device pairing |
| 6 | **`danger_accept_invalid_certs(true)`** — TLS provides zero identity verification. Entirely relies on post-handshake TOFU fingerprint comparison. | `connection.rs:417,425` | MITM attacks possible at TLS layer |
| 7 | **`connect()` method ignores cert verification failure** — logs warning but continues, unlike `connect_to_device` and `accept_incoming` which correctly return `Err` | `connection.rs:94-97` | MITM via ignored certificate mismatch |
| 8 | **API key logged in plaintext** at INFO level on daemon startup | `daemon.rs:45-52` | Credential exposure in persistent logs |
| 9 | **Unrestricted CORS** — `allow_origin(Any)`, `allow_methods(Any)`, `allow_headers(Any)` | `router.rs:16-21` | CSRF from any origin |
| 10 | **Empty `api_keys` disables auth entirely** — default if `AppSettings::load_from_file()` used without keys, or if `with_api_keys(vec![])` called | `auth.rs:9-11, settings.rs:48` | Complete unauthorized API access |
| 11 | **Unbounded `read_until`** — no max packet size. Malicious peer sends multi-GB data without newline, exhausting memory. | `connection.rs:247,314,530` | Memory exhaustion DoS, OOM kill |

## HIGH (14)

| # | Issue | Location | Impact |
|---|-------|----------|--------|
| 12 | **Plugin `handle_packet` panics kill packet loop** — no `catch_unwind`. `notify_disconnected` never called, `disconnect` never called. Connection silently lost. | `router.rs:69` → `connection_loop.rs:66` | Single malicious packet kills device connection |
| 13 | **`lifecycle.transition(Disconnected)` never called** — devices remain `Connected` forever in registry. No `StateChanged` event emitted. SSE clients never see disconnects. | `connection_loop.rs:75-84` | Stale device state in API and event stream |
| 14 | **Battery/SMS data never cleaned on disconnect** — `on_disconnected` is a no-op. HashMaps grow unbounded per unique device. | `battery.rs:25`, `sms.rs:30` | Memory leak proportional to device history |
| 15 | **Timing-unsafe API key comparison** — `==` on `&String` enables side-channel byte-by-byte brute force | `auth.rs:19` | Facilitates API key extraction |
| 16 | **No rate limiting** on any endpoint | `router.rs` | Brute-force amplification, DoS |
| 17 | **Path traversal via device_id** — no sanitization at protocol layer. Used directly in cert/fingerprint file paths. Malicious peer with `../../` in device_id writes outside cert_dir. | `crypto.rs:375-495` | Arbitrary file read/write on host |
| 18 | **Non-atomic `save_to_disk`** — crash mid-write corrupts file. On next boot, `.ok()` silently swallows load error, empty state written back on next shutdown, permanent data loss. | `registry.rs:95-112`, `daemon.rs:55-56` | Silent permanent data loss on crash |
| 19 | **No packet size limit** on deserialization — arbitrary large JSON parsed entirely into memory | `packet.rs:73-85` | Memory DoS |
| 20 | **No connection rate limiting** — unbounded task spawning on incoming TCP connections | `listener.rs:36-44` | Resource exhaustion DoS |
| 21 | **`native_tls_connect` reads device_id from filesystem** on every call instead of using in-memory `self.device_id` field | `connection.rs:397-405` | Unnecessary I/O per connection, failure if file unavailable |
| 22 | **Reconnect sleep not cancellation-aware** — `tokio::time::sleep(delay).await` not wrapped in `select!` with shutdown, delays shutdown up to 30s | `daemon.rs:334` | Slow shutdown |
| 23 | **`connect()` and `accept_as_client()` don't check for duplicate connections** — can silently clobber existing connection, unlike `connect_to_device` and `accept_incoming` which check `contains_key` | `connection.rs:84-116,446-476` | Connection replacement race |
| 24 | **`identity_exchange` can disconnect without notifying plugins** — calls `cm.disconnect()` but returns `()`, caller never calls `notify_disconnected` | `listener.rs:91-136` | Plugins hold stale connected state |
| 25 | **`to_packet()` can panic** on serialization failure — `unwrap()` in production code | `types.rs:72` | Task crash during connection setup |

## MEDIUM (20)

| # | Issue | Location |
|---|-------|----------|
| 26 | No validation of `Identity` fields (empty device_id, path chars, huge device_name length, invalid protocol_version) | `types.rs:25-40` |
| 27 | No broadcast jitter — all devices with same interval synchronize over time | `discovery.rs:211-231` |
| 28 | `save_to_disk` holds read lock during blocking filesystem I/O | `pairing.rs:250-276` |
| 29 | Private key written before `chmod 0600` — brief window with default (possibly world-readable) permissions | `crypto.rs:191-211` |
| 30 | SSE stream doesn't follow SSE wire protocol (no `data:` prefix, no double-newline delimiters) | `websocket.rs:67-79` |
| 31 | No cross-stream event ordering between DeviceEvent and PluginEvent (battery event may arrive before connected state change) | `websocket.rs:65` |
| 32 | `send_packet` updates `last_activity` and `packets_sent` metrics before flush confirmed | `connection.rs:488-515` |
| 33 | Flush error silently discarded in `send_packet` — caller gets `Ok(())` even when data may not be delivered | `connection.rs:493` |
| 34 | `ServerEvent` `#[serde(untagged)]` enum — ambiguous deserialization if both variants could match | `websocket.rs:20` |
| 35 | Reconnect enters `run_packet_loop` even when shutdown is already cancelled | `daemon.rs:340-360` |
| 36 | Clipboard fire-and-forget `tokio::spawn` loses ordering under rapid contention | `clipboard.rs:64-67` |
| 37 | SMS threads stored globally by thread ID, not per-device (thread ID collision across devices) | `sms.rs:64` |
| 38 | Share plugin is a no-op — logs the packet but never actually writes files to disk | `share.rs:49-68` |
| 39 | No device_id validation at protocol layer (only API layer validates via `validate_device_id`) | `listener.rs:62` |
| 40 | `load_from_disk` merges unvalidated data — can re-add previously unpaired devices | `pairing.rs:315-316` |
| 41 | No validation on `broadcast_interval_secs` (0 = flood) or `pairing_timeout_mins` (0 = instant expiry) | `settings.rs:26-27` |
| 42 | No validation on `api_port` (port 0 = unpredictable, privileged ports fail without root) | `settings.rs:47` |
| 43 | `get_device` returns full internal `Device` struct — future internal fields would be exposed to API clients | `handlers/mod.rs:58,64` |
| 44 | Registry TOCTOU race between `plugins` and `capability_index` in `register`/`get_by_capability` | `registry.rs:29-51` |
| 45 | Read lock on `plugins` held during all plugin `on_connected`/`on_disconnected` callbacks — latent deadlock if callback tries to write-lock registry | `registry.rs:96-110` |

## LOW (15+)

| # | Issue | Location |
|---|-------|----------|
| 46 | Mixed `std::sync::RwLock` (device_id, device_name) and `tokio::sync::RwLock` (connections) — `.unwrap()` can panic on poisoned lock | `connection.rs:65-67` |
| 47 | `JoinHandle` dropped on spawned tasks — panics silently swallowed, no active task tracking | `listener.rs:41`, `daemon.rs:214,247` |
| 48 | Hardcoded 500ms sleep in identity exchange — fragile race-condition workaround | `listener.rs:123` |
| 49 | Asymmetric TLS roles (outgoing=server, incoming=client) — confusing, hard to verify correctness | `connection.rs:160,308` |
| 50 | `set_device_identity` can panic on poisoned lock | `connection.rs:79-82` |
| 51 | Double `notify_disconnected` possible for same device if multiple code paths disconnect concurrently | `connection_loop.rs:82-83` |
| 52 | `PairState::Rejected` variant is dead code — never assigned | `pairing.rs:24` |
| 53 | `handle_pair_response` is never called from production code — connection_loop calls accept/reject directly | `pairing.rs:148-154` |
| 54 | Negative packet IDs possible (sign bit set from first UUID byte) | `types.rs:91` |
| 55 | 10-year certificate validity (NIST recommends max 3 years for end-entity) | `crypto.rs:104-105` |
| 56 | `target_protocol_version` typed as `Option<String>` instead of `Option<u32>` | `types.rs:38-39` |
| 57 | Certificate and key sizes logged at generation | `crypto.rs:150-152,275-279` |
| 58 | Peer certificate fingerprint logged at INFO level | `crypto.rs:407-409` |
| 59 | Discovery own-broadcast filtering relies on error message string containing `"ignored_own"` | `discovery.rs:278` |
| 60 | `DeviceType::from_str` shadows std `FromStr` trait | `device/types.rs:84` |
| 61 | No SSE heartbeat/ping — proxies/NAT may close idle connections | `websocket.rs:67-79` |
| 62 | Missing security headers (X-Content-Type-Options, X-Frame-Options, etc.) | `router.rs` |
| 63 | `Error::not_found` and `Error::io` helpers expose filesystem paths in API error messages | `errors.rs:245-260` |
| 64 | `pairing_timeout_mins` cast to `i64` without overflow check | `app.rs:44` |
| 65 | Duplicate broadcaster code — `EventBroadcaster` and `PluginEventBroadcaster` are near-identical, no generic abstraction | `device/events.rs`, `plugins/events.rs` |
| 66 | API key accepted as query parameter in SSE endpoint — appears in browser history, logs, proxy logs | `websocket.rs:37-40` |
| 67 | `connect()` and `accept_as_client()` don't validate peer certificates like `connect_to_device` and `accept_incoming` do | `connection.rs:84-116,446-476` |

---

## Recommended Fix Priority

### Phase 1: Prevent hangs and crashes (CRITICAL 1-4, 11, 12)
Add timeouts to every network I/O operation. Add max packet size limit. Add panic protection to packet routing.

### Phase 2: Fix data integrity (HIGH 13, 14, 18)
Call `lifecycle.transition(Disconnected)`. Clean up plugin data on disconnect. Make `save_to_disk` atomic.

### Phase 3: Fix security (CRITICAL 5-10, HIGH 15-17)
Fix auto-pair, add constant-time key comparison, sanitize device_id, tighten CORS, remove key from logs.

### Phase 4: Hardening (remaining HIGH/MEDIUM)
Rate limiting, notification cancel, SSE protocol, reconnect cancellation, duplicate connection checks, dead code cleanup.

### Phase 5: Polish (LOW)
Security headers, broadcast jitter, generic broadcaster, code cleanup.

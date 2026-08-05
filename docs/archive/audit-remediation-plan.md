# Plan: Rust-Connect Audit Remediation + Feature Completion

**Generated**: 2026-04-01  
**Estimated Complexity**: High  
**Current state**: 424 tests, 0 clippy warnings, 8/8 investigation items complete

## Overview

Single comprehensive plan combining **critical security fixes** from the 5-dimension audit with **remaining high-value features**. Ordered by risk: security/reliability first, then features, then structural improvements. Each sprint produces a committable, testable increment.

## Prerequisites

- All 424 existing tests must continue to pass
- No new clippy warnings
- Each sprint commits independently
- Real device (Android phone, stock KDE Connect app 1.35.5) available for manual testing on Sprints 3-4

---

## Sprint 1: Critical Security Fixes

**Goal**: Eliminate 6 critical findings that could cause data loss, security breaches, or daemon crashes  
**Demo/Validation**: `cargo test` passes, `cargo clippy` clean, daemon survives edge cases

### Task 1.1: Fix path traversal in share plugin
- **Location**: `src/plugins/share.rs:62-63`
- **Description**: Sanitize filename before joining with download_dir. Strip `..` components, reject absolute paths, reject paths with directory separators.
- **Acceptance Criteria**:
  - `filename = "../../../etc/passwd"` → rejected with error
  - `filename = "normal_file.txt"` → accepted
  - `filename = "subdir/file.txt"` → accepted (creates subdirectory)
- **Validation**: New test `test_receive_file_path_traversal_rejected`, `test_receive_file_absolute_path_rejected`

### Task 1.2: Add file size limit on received files
- **Location**: `src/plugins/share.rs`, `src/protocol/payload_transfer.rs`
- **Description**: Add `MAX_FILE_SIZE` constant (100MB). Check `payloadSize` before receiving. Abort transfer if exceeds limit.
- **Acceptance Criteria**:
  - Files > 100MB rejected before transfer starts
  - `payloadSize` field used for pre-check
  - Configurable via `AppSettings.max_file_size_mb`
- **Validation**: New test `test_receive_file_exceeds_size_limit_rejected`

### Task 1.3: Fix runcommand blocking tokio + add timeout
- **Location**: `src/plugins/runcommand.rs:79-145`
- **Description**: Replace `std::process::Command::output()` with `tokio::task::spawn_blocking`. Add 30-second timeout via `tokio::time::timeout`.
- **Acceptance Criteria**:
  - Command execution runs on blocking thread pool (doesn't stall tokio workers)
  - Commands exceeding 30s are killed and reported as failed
  - Exit code, stdout, stderr captured correctly
- **Validation**: New test `test_command_timeout_kills_process`, existing tests still pass

### Task 1.4: Fix global rate limiter → per-client
- **Location**: `src/api/middleware.rs:18-47`
- **Description**: Replace global `OnceLock<Mutex<RateLimiterState>>` with `DashMap<IpAddr, RateLimiterState>` or `Mutex<HashMap<IpAddr, RateLimiterState>>`. Each client IP gets its own 100 req/min bucket.
- **Acceptance Criteria**:
  - Client A hitting rate limit does not affect Client B
  - Stale entries cleaned up (LRU or periodic sweep)
  - Default: 100 requests/minute per IP
- **Validation**: New test `test_per_client_rate_limiting`

### Task 1.5: Fix crypto.rs expect() panics → graceful errors
- **Location**: `src/protocol/crypto.rs:425,440,555,599`
- **Description**: Replace `validate_device_id(device_id).expect(...)` with proper `Result` propagation. Path methods return `Result<PathBuf>`.
- **Acceptance Criteria**:
  - Invalid device_id returns error, not panic
  - All callers handle the Result properly
  - No `expect()` or `unwrap()` on validation paths
- **Validation**: New test `test_invalid_device_id_returns_error_not_panic`

### Task 1.6: Fix plugin panic not logged
- **Location**: `src/protocol/router.rs:96-103`
- **Description**: Add `error!` log before returning error when `catch_unwind` catches a plugin panic.
- **Acceptance Criteria**:
  - Plugin panic is logged with device_id and plugin name
  - Error is still returned to caller
  - Other plugins continue functioning
- **Validation**: New test `test_plugin_panic_is_logged`

---

## Sprint 2: Reliability & Privacy

**Goal**: Fix 10 medium-severity reliability and privacy issues  
**Demo/Validation**: `cargo test` passes, no sensitive data in logs, graceful error handling

### Task 2.1: Remove sensitive data from logs
- **Location**: `src/plugins/sms.rs:96`, `src/plugins/telephony.rs:75`, `src/plugins/notification.rs:112`, `src/daemon.rs:99`
- **Description**:
  - SMS: Remove `address` from log, log only `thread_id`
  - Telephony: Mask phone number (show last 4 digits only)
  - Notification: Remove `title` and body from log
  - Daemon: Remove `key_prefix` from API key log
- **Acceptance Criteria**: No phone numbers, notification content, or API key fragments in logs
- **Validation**: Manual: run daemon, trigger events, verify log output

### Task 2.2: Fix logging consistency — missing event fields, wrong levels
- **Location**: Multiple files (see audit findings 2.1-2.4, 4.1-4.2, 5.1-5.7)
- **Description**:
  - Add `event` field to all log statements missing it (~15 locations)
  - Change keepalive failure from `info!` to `warn!`
  - Change device disconnection from `info!` to `warn!` (unexpected) / `info!` (graceful)
  - Change certificate fingerprint mismatch from `warn!` to `error!`
  - Change mousepad input logging from `info!` to `debug!`
  - Change mousepad injection logs to include `device_id`
  - Change TLS handshake complete from `info!` to `debug!`
- **Acceptance Criteria**: All logs have `event` field, correct severity levels, consistent field names
- **Validation**: Manual: run daemon at `RUST_LOG=debug`, verify log structure

### Task 2.3: Add missing event logging
- **Location**: `src/device/registry.rs:42-49,75-80`, `src/protocol/connection.rs:893-903`, `src/protocol/router.rs:55-58`
- **Description**:
  - Log device add/remove operations
  - Log cancel_loop/remove_cancel_token operations
  - Log handler unregistration
- **Acceptance Criteria**: All state-changing operations produce log entries
- **Validation**: Manual: verify logs on device add/remove

### Task 2.4: Standardize validate_device_id — single source of truth
- **Location**: `src/api/handlers/mod.rs:16-37`, `src/protocol/crypto.rs:13-33`
- **Description**: Merge into single `validate_device_id` in `src/utils/errors.rs` or `src/protocol/crypto.rs`. Use stricter validation rules everywhere.
- **Acceptance Criteria**: One function, all callers use it, consistent rules
- **Validation**: Existing tests pass, no duplicate definitions

### Task 2.5: Add idle timeout to TCP listener
- **Location**: `src/protocol/listener.rs:102-116`
- **Description**: Set `SO_RCVTIMEO` or use `tokio::time::timeout` on the accept loop. Close connections that complete TLS but send no data within 30 seconds.
- **Acceptance Criteria**: Idle connections closed after 30s, active connections unaffected
- **Validation**: Manual: connect without sending data, verify timeout

### Task 2.6: Add jitter to reconnect backoff
- **Location**: `src/daemon.rs:437-559` (via `compute_backoff_delay`)
- **Description**: Add random jitter (±25%) to backoff delay to prevent thundering herd.
- **Acceptance Criteria**: Backoff delay varies by ±25% from calculated value
- **Validation**: New test `test_backoff_delay_has_jitter`

### Task 2.7: Fix unbounded rate limit tracking map
- **Location**: `src/protocol/connection.rs:238-239`
- **Description**: Clean up stale entries more aggressively (every N attempts, not just when > 100).
- **Acceptance Criteria**: Map never grows beyond reasonable size
- **Validation**: New test `test_connection_attempt_map_does_not_leak`

---

## Sprint 3: Device Lifecycle Endpoints

**Goal**: Add 5 device management endpoints for full lifecycle control  
**Demo/Validation**: `cargo test` passes, manual: curl endpoints to connect/disconnect devices

### Task 3.1: DELETE /api/v1/devices/:device_id
- **Location**: `src/api/handlers/mod.rs`, `src/api/router.rs`
- **Description**: Remove device from registry. If connected, disconnect first. Unpair. Notify plugins.
- **Acceptance Criteria**:
  - Device removed from registry
  - Active connection terminated gracefully
  - Pairing state cleared
  - Returns `{ "removed": true }`
  - 404 if device not found
- **Validation**: New test `test_delete_device`, `test_delete_device_not_found`

### Task 3.2: POST /api/v1/devices/:device_id/connect
- **Location**: `src/api/handlers/mod.rs`, `src/api/router.rs`
- **Description**: Initiate TCP+TLS connection to device. Body: `{ "address": "ip:port" }`. Spawn connection loop.
- **Acceptance Criteria**:
  - Connection established and packet loop started
  - Returns `{ "connected": true, "generation": N }`
  - 409 if already connected
  - 404 if device not in registry
- **Validation**: New test `test_connect_device`, `test_connect_already_connected`

### Task 3.3: POST /api/v1/devices/:device_id/disconnect
- **Location**: `src/api/handlers/mod.rs`, `src/api/router.rs`
- **Description**: Terminate active connection. Cancel packet loop. Transition to Disconnected state. Notify plugins.
- **Acceptance Criteria**:
  - Connection terminated gracefully
  - Returns `{ "disconnected": true }`
  - 404 if device not found
  - 409 if not connected
- **Validation**: New test `test_disconnect_device`, `test_disconnect_not_connected`

### Task 3.4: GET /api/v1/devices/:device_id/state
- **Location**: `src/api/handlers/mod.rs`, `src/api/router.rs`
- **Description**: Return current lifecycle state and state_since timestamp.
- **Acceptance Criteria**:
  - Returns `{ "state": "connected", "state_since": "2024-..." }`
  - 404 if device not found
- **Validation**: New test `test_get_device_state`

### Task 3.5: GET /api/v1/devices/connected
- **Location**: `src/api/handlers/mod.rs`, `src/api/router.rs`
- **Description**: List all currently connected devices with address and generation.
- **Acceptance Criteria**:
  - Returns `{ "connected_devices": [{ "device_id", "address", "generation" }] }`
  - Empty array if none connected
- **Validation**: New test `test_list_connected_devices`

---

## Sprint 4: Outgoing File Share + More Packet Types

**Goal**: Complete file sharing (desktop→phone) and add 3 more KDE Connect packet types  
**Demo/Validation**: Manual: send file from desktop to phone, trigger findmyphone/lock/battery

### Task 4.1: Wire PayloadTransfer for outgoing file send
- **Location**: `src/plugins/share.rs`, `src/protocol/payload_transfer.rs`
- **Description**: Add `send_file(file_path, device_id)` method to SharePlugin. Opens payload transfer listener, advertises port in share request packet, waits for device to connect, sends file bytes.
- **Acceptance Criteria**:
  - File sent from desktop appears on phone
  - Progress tracked (bytes sent / total)
  - Error handling for transfer failures
- **Validation**: Manual test with real device

### Task 4.2: Add POST /api/v1/devices/:device_id/share/send endpoint
- **Location**: `src/api/handlers/mod.rs`, `src/api/router.rs`
- **Description**: Multipart form upload. Receives file, initiates outgoing share to device.
- **Acceptance Criteria**:
  - File uploaded via POST, sent to device
  - Returns `{ "sent": true, "filename": "..." }`
  - 404 if device not connected
- **Validation**: Manual: `curl -F "file=@test.txt" http://localhost:9090/api/v1/devices/:id/share/send`

### Task 4.3: Add Presenter plugin
- **Location**: New file `src/plugins/presenter.rs`
- **Description**: Handle `kdeconnect.presenter` packets. Simple packet with action (next/previous). Log events, broadcast via PluginEventBroadcaster.
- **Acceptance Criteria**:
  - Parses presenter packets
  - Broadcasts `PluginEvent::PresenterAction`
  - Outgoing capability: `kdeconnect.presenter.request`
- **Validation**: New tests for packet parsing and capabilities

### Task 4.4: Add SystemVolume plugin
- **Location**: New file `src/plugins/systemvolume.rs`
- **Description**: Handle `kdeconnect.systemvolume` packets. Track volume level and mute state per device.
- **Acceptance Criteria**:
  - Parses volume packets (maxVolume, volume, muted)
  - Stores per-device volume state
  - `get_volume(device_id)` method for API access
- **Validation**: New tests for packet parsing and state storage

### Task 4.5: Handle incoming battery.request
- **Location**: `src/plugins/battery.rs`
- **Description**: Add `kdeconnect.battery.request` to incoming capabilities. When received, read local battery status from `/sys/class/power_supply/` (Linux) and respond with `kdeconnect.battery` packet.
- **Acceptance Criteria**:
  - Incoming capability registered
  - Responds with local battery data when requested
  - Graceful fallback if no battery (desktop)
- **Validation**: New test `test_handle_battery_request`

### Task 4.6: Add notification history
- **Location**: `src/plugins/notification.rs`, `src/api/handlers/mod.rs`
- **Description**: Store recent notifications (last 100) in a bounded Vec. Add `GET /api/v1/notifications?device_id=xxx&limit=N` endpoint.
- **Acceptance Criteria**:
  - Notifications stored with device_id, app_name, title, timestamp
  - Bounded to 100 entries (oldest dropped)
  - API endpoint returns filtered history
- **Validation**: New test `test_notification_history`, `test_get_notifications_endpoint`

### Task 4.7: Register new plugins in loader
- **Location**: `src/plugins/loader.rs`, `src/plugins/mod.rs`
- **Description**: Register PresenterPlugin, SystemVolumePlugin. Add SystemVolumePlugin to AppState.
- **Acceptance Criteria**: All new plugins registered and accessible
- **Validation**: `cargo test`

---

## Sprint 5: DRY/SRP/Structural Improvements

**Goal**: Reduce code duplication, improve structure, add API pagination and benchmarks  
**Demo/Validation**: `cargo test` passes, reduced line count, `cargo bench` runs

### Task 5.1: Extract Packet::body_as helper
- **Location**: `src/protocol/types.rs`
- **Description**: Add `impl Packet { pub fn body_as<T: DeserializeOwned>(&self, context: &str) -> Result<T> }`. Replace all 6 instances of `serde_json::from_value(...).map_err(...)`.
- **Acceptance Criteria**: All plugins use `packet.body_as("battery")?` pattern
- **Validation**: Existing tests pass, no behavior change

### Task 5.2: Create generic EventBroadcaster<T>
- **Location**: `src/device/events.rs`, `src/plugins/events.rs`
- **Description**: Merge `EventBroadcaster` and `PluginEventBroadcaster` into single generic `EventBroadcaster<T: Clone + Debug>`. Update all callers.
- **Acceptance Criteria**: One generic struct, both device and plugin events use it
- **Validation**: Existing tests pass

### Task 5.3: Extract auth+validation into axum extractor
- **Location**: `src/api/handlers/mod.rs`
- **Description**: Create `AuthenticatedDevice` extractor that validates API key + device_id. Replace 13 instances of repeated auth preamble.
- **Acceptance Criteria**: Handler functions start with `auth: AuthenticatedDevice` instead of 3-line preamble
- **Validation**: Existing tests pass

### Task 5.4: Extract duplicate connection lifecycle from daemon.rs
- **Location**: `src/daemon.rs:330-559`
- **Description**: Extract `handle_new_connection(state, connected_id, remote_identity, generation) -> LoopResult` method. Both `try_outgoing_connection` and `reconnect_with_backoff` call it.
- **Acceptance Criteria**: ~25 lines of duplicated logic eliminated
- **Validation**: Existing tests pass, behavior unchanged

### Task 5.5: Add API pagination to list_devices
- **Location**: `src/api/handlers/mod.rs`
- **Description**: Add `?page=N&limit=M` query params to `GET /api/v1/devices`. Default: page=1, limit=50. Return `{ "devices": [...], "total": N, "page": M, "limit": L }`.
- **Acceptance Criteria**: Pagination works, backward compatible (no params = all devices)
- **Validation**: New test `test_list_devices_paginated`

### Task 5.6: Add [[bench]] section with criterion
- **Location**: `Cargo.toml`, new `benches/` directory
- **Description**: Add `criterion` dev-dependency. Create `benches/packet_serialization.rs` benchmarking Packet serialize/deserialize, `benches/connection.rs` benchmarking connection setup.
- **Acceptance Criteria**: `cargo bench` runs, produces meaningful results
- **Validation**: `cargo bench` completes successfully

### Task 5.7: Add OpenAPI documentation to all handlers
- **Location**: `src/api/handlers/mod.rs`
- **Description**: Add `#[utoipa::path(...)]` attributes to all 9 undocumented handlers (battery, sms, clipboard, mpris, telephony, share/files, health).
- **Acceptance Criteria**: Swagger UI shows all endpoints with request/response schemas
- **Validation**: Manual: open `/docs`, verify all endpoints documented

### Task 5.8: Remove unused dependencies
- **Location**: `Cargo.toml`
- **Description**: Remove `tokio-tungstenite` and `jsonschema` (unused). Add platform guards for `zbus` and `notify-rust` (`#[cfg(target_os = "linux")]`).
- **Acceptance Criteria**: `cargo build` succeeds, fewer dependencies
- **Validation**: `cargo tree` shows removed deps

---

## Testing Strategy

- Each sprint: `cargo test` must pass all existing + new tests
- Each sprint: `cargo clippy` 0 warnings
- Each sprint: `cargo fmt --check` clean
- Sprint 1: Focus on security edge case tests
- Sprint 3: Integration tests for device lifecycle
- Sprint 4: Manual testing with real device for file share and new packet types
- Sprint 5: Benchmark validation

## Potential Risks & Gotchas

1. **Path traversal fix**: Must handle Windows paths (`..\\`) as well as Unix (`../`). Use `pathdiff` or `std::path::Path::components()` for robust normalization.
2. **Rate limiter per-client**: `DashMap` adds a dependency. Alternative: `Mutex<HashMap>` with periodic cleanup. Trade-off: performance vs dependency count.
3. **Payload transfer TLS**: Currently plaintext. Sprint 1 doesn't fix this (high complexity). Document as "local network only" limitation. Consider adding in a future sprint.
4. **Outgoing file share**: Requires the phone to connect back to the desktop on a dynamic port. Firewall/NAT may block this. Need to advertise the correct local IP.
5. **Notification history**: Bounded Vec with oldest-dropped is simple but loses data. Consider ring buffer (`ringbuf` crate) for better performance.
6. **API pagination backward compatibility**: Default behavior (no params) must return all devices to not break existing clients.
7. **Sprint 5 refactors**: Large-scale refactors risk breaking existing tests. Each task must be independently testable. Consider feature-flagging if needed.
8. **Presenter/SystemVolume plugins**: These are receive-only (phone→desktop). The phone sends events, we log them. No outgoing action needed for MVP.

## Rollback Plan

- Each sprint is a separate commit — revert if issues found
- No database migrations or schema changes
- All changes are code-only, reversible via git

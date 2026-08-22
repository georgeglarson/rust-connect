# Task #1042 M4-wiring lane findings

Branch: `feat-shareinputdevices-m4-wire`. Commits logical, in this order:
1. `mod.rs` — `activate_portal_session` wiring (fd handoff + pump thread + disconnect watcher)
2. `portal.rs` — `take_ei_fd` accessor, `populate_ei_receiver` slot, comment updates
3. `shareinputdevices_portal_lifecycle.rs` — `connect_to_eis_socketpair` option on the fake
4. `shareinputdevices_m4_wiring.rs` (new) — two integration tests

## What changed

### `mod.rs` — `activate_portal_session`
- Renamed session handling: keep `mut PortalSession` until after `take_ei_fd`,
  wrap in `Arc` only at the very end. `take_ei_fd` needs `&mut self`; once
  the receiver exists we also need a shared `&PortalSession` for the slot —
  so the production code follows the same shape the M4 tests do.
- Built `EiReceiver::new(fd, handshake_name)` → `Arc<EiReceiver>`.
- `populate_ei_receiver` cloned the `Arc` into the slot before the drive
  future was moved into its thread.
- Pump thread: `std::thread::Builder::new().name("shareinputdevices-ei-pump")`
  + `tokio::runtime::Builder::new_current_thread().enable_all().build()` →
  `block_on(async move { receiver.start().await; drive.await })`. The drive
  future is `!Send` (per reis), so it cannot live on the multi-thread
  runtime where the rest of the plugin lives. Wire rx + disconnect rx
  channeled back to the main task via oneshot channels.
- Disconnect watcher: `tokio::spawn(async move { disconnect_rx.changed().await;
  backend_available.store(false) })`. Mirrors cpp
  `inputcapturesession.cpp:372-374` — disconnect is observed, session is
  NOT closed (the destructor closes it; the Disabled signal is the session
  teardown trigger).
- Unified consumer (unchanged shape): `tokio::select! biased; { activated_rx,
  wire_rx }` → build packet → `send_packet(device_id, packet)`.

### `portal.rs` — fd ownership + late-binding slot
- `ei_fd: Option<OwnedFd>` (was `_ei_fd: OwnedFd`); `take_ei_fd(&mut self) ->
  OwnedFd` moves the fd out, panics on second call. The M4 brief mandated
  this rather than a getter; taking by value makes ownership transfer to
  the receiver unambiguous (the original `OwnedFd` is dropped on `take()`,
  closing any fd the receiver doesn't inherit — this would be the silent
  bug if we returned a borrowed fd and the receiver tried to clone it
  independently).
- `populate_ei_receiver(&self, receiver: Arc<EiReceiver>)` writes to the slot.
  Called once after the receiver is constructed. The signal handler reads
  the slot on every `Activated`; if the slot is still `None` (the receiver
  hasn't been wired yet — the brief says M4 owns this), the drain is a
  no-op and the gate's `should_queue()` keeps queueing EI events for
  later replay.
- `ei_receiver_slot: Arc<std::sync::Mutex<Option<Arc<EiReceiver>>>>`. The
  `std::sync::Mutex` is held only long enough to clone the `Arc` — the
  signal handler's `.await` `r.handle_activated(...)` runs without the
  guard (the `MutexGuard` is `!Send`, holding it across `.await` would
  block the multithread runtime).
- `spawn_signal_handler` Activated arm now does step-1 (decode + send
  ActivatedEvent to consumer) BEFORE step-2 (`.await` `r.handle_activated`
  to drain the EI gate). The biased select in the consumer guarantees
  the shareinputdevices.request packet is processed BEFORE the first
  mousepad.request — matching the cpp's `started(deltax, deltay) → for
  (event : queuedEiEvents) handleEiEvent(event)` order
  (`inputcapturesession.cpp:296-300`).

### `tests/shareinputdevices_portal_lifecycle.rs` — fake portal extension
- New `FakePortalState.connect_to_eis_socketpair: Option<OwnedFd>`.
  `connect_to_eis` `take()`s it; if `Some`, hands it back via
  `zbus::zvariant::Fd::from(owned)`. Otherwise falls back to `/dev/null`
  (the M2 path never reads the fd).
- **Bug found and fixed in the same change.** My first cut of the socketpair
  plumbing used `std::mem::take(&mut guard.calls)` to peel off the
  recorded ConnectToEIS entry for an unrelated side-check — `take` empties
  the Vec, so the `state.calls` was wiped after ConnectToEIS. The M2
  v1-sequence test then asserted on a half-populated list (only
  GetZones/SetPointerBarriers/Enable), failing the strict ordering check.
  Fixed by dropping the `take` — the lock-guard pattern still serves its
  purpose without the side-effect on `calls`.

### `tests/shareinputdevices_m4_wiring.rs` (new, ~750 lines)
- Mirrors the M2 fake portal harness; adds the fake EIS peer on a
  dedicated `std::thread` (reis's `EisHandshaker` + `EisRequestConverter`).
- `setup_m4_harness` does:
  1. `UnixStream::pair()` for the EIS-side socketpair.
  2. Drives the full v1 sequence against the fake portal.
  3. `session.take_ei_fd()` → `EiReceiver::new(...)`.
  4. Spawns the pump thread (same shape as production).
  5. Spawns the test consumer (`biased;` select, recording channel).
  6. Spawns the disconnect watcher.
- **Test 1: `m4_activated_signal_routes_to_consumer_via_session`** —
  emit a single `Activated(o, a{sv})` signal on the fake portal conn;
  assert exactly one `kdeconnect.shareinputdevices.request` packet
  arrives on the recording channel with the spec body shape
  (`exitEdge`, `deltax`, `deltay`).
- **Test 2: `m4_ei_peer_disconnect_flips_backend_available`** — start
  with `backend_available=true`, signal the fake peer to drop its
  read loop, poll the flag and assert it flips to false within 3s.
- **Encoding trap.** The test's `emit_activated_signal` initially used
  `Value::Structure(StructureBuilder::new().add_field(F64).add_field(F64).build())`
  for `cursor_position`. **This produces a `(vv)` wire encoding — each
  field is a Variant, not an F64.** The production decoder does
  `<(f64, f64)>::try_from(v)` which rejects `(vv)`, so the test hung
  on `2s` outbound timeout because no `shareinputdevices.request` was
  emitted. Fix: put the tuple in the HashMap directly
  (`opts.insert("cursor_position", (x, y).into())`) — zvariant serializes
  a `(f64, f64)` Rust tuple as the spec's `(dd)` STRUCT. The same trap
  likely lives in production `Options::insert_doubles`
  (`portal.rs:262-273`) — the encoding it produces is `(vv)` too. The
  production Release path has not been observed broken because the M2
  tests don't decode cursor_position on Release; the real portal's Qt
  decoder appears to accept either, but it would be worth tightening
  `insert_doubles` to use the tuple form too. **NOT in this M4 lane** —
  the Release path is upstream of M4 and is out of scope for this
  brief. Flagging here for a follow-up.

## How it was verified

- `CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m4-ei cargo test --no-fail-fast` — all test targets green:
  - 1040 unit tests
  - 33 in `tests/mpris_*` (unrelated; sanity check)
  - 20 in `tests/shareinputdevices_ei_socketpair.rs` (M3 receiver/pump)
  - 6 in `tests/shareinputdevices_m4_wiring.rs` (NEW: 2 m4 wiring tests; same file also has 4 fixtures/helper tests).
  - 5 in `tests/shareinputdevices_portal_lifecycle.rs` (M2 v1 sequence + probes)
  - plus the rest of the suite unchanged
- `CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m4-ei cargo clippy --all-targets -- -D warnings` — clean.
- `CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m4-ei cargo fmt --check` — clean.
- Full suite WITHOUT any TMPDIR override (per the standing rule — clipboard_x11).

## Critique — blunt

**The fd semantics in the test are not really pinned.** Test 1's "fd is
the same one the portal handed back" is asserted indirectly — by the
EIS handshake completing. If production's `take_ei_fd` returned a fd
that was NOT the socketpair end the fake portal sent, the receiver's
HELLO bytes would never reach the fake peer's read loop, the
handshake oneshot would time out, and `setup_m4_harness` would panic
on `EIS handshake timed out — fd wiring is broken`. So the wiring IS
verified end-to-end, but the assertion is implicit — there's no explicit
`assert_eq!(taken_fd.as_raw_fd(), client_fd.as_raw_fd())` because
SCM_RIGHTS transfers the fd via the kernel; the receiver gets a fresh
fd number that doesn't match the sender's. I considered asserting on
the raw fd number but concluded the integration verification (handshake
must complete) is stronger than the local-identity check. Documented in
the test source rather than asserted.

**The disconnect test is timing-loose.** 3s deadline for the backend
flag to flip after a peer shutdown, polling every 50ms. In practice
this fires within ~20ms (the peer's poll sleeps 5ms, the converter
drops, the receiver's pump sees EOF, the disconnect arm fires). The
3s budget is a safety margin, not a load-bearing latency. A future
hardening could drop to 200ms.

**The encoding bug discovered during testing is latent in M2/M3.** As
noted above, `Options::insert_doubles` (`portal.rs:262-273`) and the
test's emit helper both produce `(vv)` instead of `(dd)` for
cursor_position. The M2 v1-sequence test never decodes cursor_position
on the way in (it asserts call ORDER only), so this hasn't surfaced
in M2's gate. The M4 wiring test DOES decode it on the way in, which
is what surfaced the bug. Left `insert_doubles` untouched because it's
upstream of M4 and the brief explicitly scopes M4 to wiring only.
Flagging here so a follow-up lane fixes it before the first real-portal
end-to-end run, where Qt may or may not be lenient about `(vv)` vs
`(dd)`.

**The pump thread's current_thread runtime is a real runtime boundary.**
Anything that holds a tokio mutex across a long await on the main
runtime could deadlock against the pump, since they share the gate
mutex (via `EiReceiver::gate`). This is the correct shape — `start()` is
designed to be called from a current_thread runtime precisely because
the drive future is `!Send` — but it is a load-bearing design point.
A future change that puts the pump on the multi-thread runtime would
need to use a `LocalSet` and `spawn_local` instead.

**The test consumer mirrors production byte-for-byte but uses an
unbounded recording channel.** If the production consumer ever blocks
on `send_packet` (because the device's send window is full), the test
won't catch it — the recording `UnboundedSender` never blocks.
This is intentional (the brief mandates "without the cryptographic /
connection-state weight of a real ConnectionManager"), but it means
this lane cannot catch a back-pressure regression in the unified
consumer. M4a's `tests/interop` lane is the right place to pin that.

## Out-of-scope (per brief)
- keysym → Qt::Key table (deferred; the M4 receiver leaves
  `plan_key`'s `special_key` body empty and only fills it when the M4a
  lane lands)
- `tests/interop` harness (M4a)
- live phone leg (M4b)
- docs promotion
- vk closure
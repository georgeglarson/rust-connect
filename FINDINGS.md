# Task #1042 — Fix lane B (peer-gated activation)

Lane M4 panel round 1. Splits the shareinputdevices boot path from
the activation path: the boot path probes + stashes the portal
session; the activation path runs lazily, driven by the first
capable peer connect and torn down on the last one leaving.

## What changed

**Behavior change in `ShareInputDevicesPlugin`.**

Pre-fix: `enable_session_backend()` probed the portal, then drove
the full v1 session sequence on the spot. A portal-less desktop
with no connected peer was inert (no consumer to drain the wire
stream); a portal-less desktop WITH a connected peer trapped the
cursor — barrier up, no events going anywhere.

Post-fix: `enable_session_backend()` probes + stashes the portal
session-bus connection only. The capability gate (a spawned task
subscribed to the device-event broadcaster) drives
`activate_portal_session` when a peer connects that advertises
`kdeconnect.shareinputdevices.request` or
`kdeconnect.mousepad.request`. The same gate drives
`deactivate_portal_session` when the consumer set empties (last
capable peer disconnects), flipping `backend_available=false`
and clearing the portal session slot. Capability-filtered fan-out
sends packets only to peers that advertise the consumer caps.

**Files touched.**

- `src/plugins/shareinputdevices/mod.rs` — `enable_session_backend`
  becomes probe-and-stash. New `spawn_capability_gate` task
  subscribes to the broadcaster and runs `do_evaluate_after_event`
  on every `StateChanged` event. `do_activate` / `do_deactivate`
  are the new entry points (split into free functions so the
  spawned task can hold only `Arc`-wrapped state). New
  `activation_in_flight: Arc<AtomicBool>` cross-armed guard
  with an RAII `ActivationGuard` that resets the flag on every
  exit path (early-return, error, completion) — closes a window
  where a re-entry would leave the flag stuck at TRUE and freeze
  all future activations.
- `src/protocol/connection/mod.rs` — added test-only
  `fake_connected: Arc<Mutex<HashSet<DeviceId>>>` shadow on
  `ConnectionManager` (cfg-gated on `test` /
  `test-helpers`). `is_connected` and `capable_consumer_ids`
  consult the shadow under `cfg(test)` so integration tests can
  declare peers "connected" without standing up a real TLS
  handshake. Test-only mutators
  `mark_fake_connected_for_test` /
  `unmark_fake_connected_for_test`. `record_peer_capabilities`
  widened to `pub async fn`.
- `src/plugins/loader.rs` — `load_default_plugins` now takes the
  device-event broadcaster and wires it into the
  shareinputdevices plugin via `with_event_broadcaster`. This is
  the production wiring the gate needs.
- `src/app.rs` — passes the broadcaster through to
  `load_default_plugins`. One-line call site change.
- `tests/shareinputdevices_m4_peer_gated_activation.rs` — NEW.
  Three tests for the lane: a capable peer connect drives the
  full v1 sequence + capability honesty; disconnect closes the
  session and reconnect re-activates cleanly; the fan-out filter
  targets only capable peers.
- `tests/shareinputdevices_m4_boot_attach.rs` — refactored. The
  two existing tests' trigger changed shape: the boot path no
  longer activates. Test 1 (`capable_peer_connect_drives_full_v1_sequence`)
  now drives activation via the gate (record capable peer +
  broadcast Connected); preserves the v1 call order and
  capability-honesty pin. Test 2 (`activation_times_out_on_silent_eis_peer`)
  now pins that the activation pump's timeout still bounds the
  wait when activation fires against a silent EIS peer — the
  brief's hang-on-silent-peer footgun now lives inside
  `do_activate` instead of `enable_session_backend`.

**Known limitation, documented not solved.**

The capability advertisement pushed at activation has no
retraction API. `ConnectionManager::record_peer_capabilities`
has `add` but no `remove`. Peers connected before deactivation
keep seeing the stale `kdeconnect.shareinputdevices.request`
capability until their next capability sync. Documented in
`mod.rs` and the brief; future lane can add
`remove_capabilities` to the CM.

## How it was verified

**Final gates — all green.**

- `cargo fmt --check` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test --no-fail-fast` (full suite, no TMPDIR override) —
  **1040 unit tests + 0 failures** across the whole crate, plus
  35 integration tests across the shareinputdevices / boot /
  wiring / peer-gated / portal-lifecycle test files, plus 5
  doc-tests. Total time ~30s for the unit-test binary alone.

**Red-before-green evidence.**

- `tests/shareinputdevices_m4_peer_gated_activation.rs` — three
  tests. Each went red before its production code landed (the
  `eis_keep_alive` flag + multi-socketpair queue were added
  specifically to make the disconnect/reconnect test observably
  deterministic; without them, the EI pump's disconnect watcher
  fires between `wait_for_calls` and the `backend_available`
  poll and the test asserts on a transient false). All three
  green now.
- `tests/shareinputdevices_m4_boot_attach.rs` — both tests
  refactored to drive activation through the gate. Both pass.

**The `activation_in_flight` bug the test caught.**

The original re-entry guard did the swap FIRST, then created
the RAII guard. The early-bail path (return when previous was
already TRUE) ran with the flag stuck at TRUE and the guard
never created — so the flag stayed TRUE forever and no future
`do_activate` could ever succeed again. The
`last_capable_peer_disconnect_closes_and_reconnect_reactivates`
test caught this on the second activation:
`backend_available` failed to flip back to TRUE even though the
v1 sequence had clearly run (second `CreateSession` on the
ledger). Fix: create the guard BEFORE the swap. The guard's
`Drop` runs on every exit, the swap's early-return now
correctly leaves the flag at FALSE.

**The fake-portal socketpair queue.**

Each activation needs a fresh socketpair end for `EiReceiver::new`
— the receiver takes ownership of the fd, the kernel sees the
peer thread's read loop end and shuts down the stream. One
socketpair can only carry one EIS handshake. The fake portal
took the fd on the first `ConnectToEIS` call; the disconnect/
reconnect test's second activation was reading from `/dev/null`
and the handshake hung, then `EiReceiver::new` failed and
`do_activate` bailed before reaching `backend_available.store(true)`.
Fix: changed `connect_to_eis_socketpair: Option<OwnedFd>` to
`connect_to_eis_socketpairs: VecDeque<OwnedFd>`. The harness
now accepts a `socketpair_count: usize`; the disconnect/
reconnect test installs two.

**The cross-armed guard's contract.**

Both the eager re-eval task (runs once at gate spawn) and the
subscription loop (runs on every event) can decide to activate
in the same peer-connect window. Without the flag, the two
arms raced into a double `CreateSession` on the same bus
connection — the portal refuses. With the flag, the second arm
bails at the swap. Verified by the `capable_peer_connect_drives_v1_sequence`
test's tight poll on the `Enable` call landing exactly once.

## Critique (blunt)

**The boot/activation split is correct but the boot path's
"return inert" semantics are now structurally different from the
activation path's "return inert" semantics.** If `enable_session_backend`
hits a probe failure or no-session-bus, the plugin stays
permanently inert — no future capable peer triggers anything.
That's the same shape as before the fix, but the rationale
changed: pre-fix, the boot path WAS the activation, and a
failure meant the desktop could never share input. Post-fix,
the boot path is probe-and-stash; a probe failure still means
the desktop can never share input, even with a connected phone
sitting right there. The right shape would be: retry the probe
on every gate event so a portal that comes up AFTER the daemon
(a likely-after-reboot sequence on a Wayland session that
starts before xdg-desktop-portal) gets a chance. Not in scope
for this lane; record as a follow-up.

**The fan-out filter doesn't filter at fan-out time.** The wire
consumer re-snapshots `capable_consumer_ids` on every packet,
which is correct but wasteful — peers' capability sets don't
change between packets. A cached snapshot invalidated on
`StateChanged` would be cheaper and stop a slow `capable_consumer_ids`
from blocking the consumer. Not in scope; the brief's
"capability-filtered fan-out" wording pins the filter, not the
cache. Worth a future pass.

**The activation pump's `BOOT_PATH_PUMP_DELIVERY_TIMEOUT` is
named wrong now.** It's no longer "boot path" — it lives inside
`do_activate` and bounds the activation pump's oneshot awaits.
The constant name survives the brief but reads as a hangover.
Rename in a follow-up; the brief's pin is the value (5s) and
the rationale (bounds a silent-EIS-peer's handshake hang), not
the symbol.

**The fan-out test asserts on `capable_consumer_ids` directly,
not on the wire consumer.** That's a deliberate choice — the
wire consumer is hard to observe without a real EI pump. But
the snapshot test pins the filter's contract without pinning
that the wire consumer actually CALLS the filter on every
packet. The M4 wiring tests cover the consumer end-to-end, but
the snapshot test is the one that catches a regression that
removes the filter call. Together they're load-bearing.

**The capability-honesty contract on `deactivate` is broken.** A
peer that connected while `backend_available=true` (and so saw
the `kdeconnect.shareinputdevices.request` capability advertised)
keeps seeing that capability until its next capability sync,
even after deactivation. The brief item 5 says "record, don't
solve"; that's what we did. The capability advertisement will
go stale on reconnect cycles and on the first deactivation.
A real phone-side consumer that retries on capability loss
will pull a phantom offer. Documented, not fixed.

**The eager re-eval task races the subscription on boot.** Both
can decide to activate for a peer that connected before the
gate subscribed. The `activation_in_flight` flag serializes
them, but it's a guard, not a fix. The clean shape would be:
don't run the eager re-eval at all — `record_peer_capabilities`
runs before the `StateChanged(Connected)` broadcasts, so the
subscription's first event already covers the boot-time-capable
case. The eager task was a defensive add; on reflection it's
redundant. Could be removed in a follow-up without behavior
change. Left in for this lane — the brief listed it as item 1
of the activation contract and removing it is a separate
decision.
# M4-wire boot-time activation hookup — Task #1042

## What changed

- `src/plugins/shareinputdevices/mod.rs::enable_session_backend`: after the
  probe passes and the connection is stashed, the function now calls
  `self.activate_portal_session().await` instead of logging
  `shareinputdevices_probe_passed_inert` and returning. The probe-failure
  path is unchanged (logs the reason, leaves `backend_available = false`,
  returns inert). Doc comments updated: `enable_session_backend`'s flow
  description now describes the hand-off to activation; the stale
  "M3's entry point … producer stays INERT until the M3 EI transport
  attaches" language on `activate_portal_session` is replaced with a
  description of what the function does (run the v1 sequence, wire the
  EI receiver, spawn the consumer) and its inert-on-failure contract
  (every internal failure path logs a `warn!` and returns; no panic, no
  daemon-boot failure).
- `tests/shareinputdevices_m4_boot_attach.rs` (new): a red-before-green
  integration test that drives `enable_session_backend` against a
  private D-Bus + `FakePortal` and asserts the v1 sequence
  (CreateSession → ConnectToEIS → GetZones → SetPointerBarriers →
  Enable) lands on the fake's call ledger without any manual
  `activate_portal_session` call. The test also pins
  `portal_backend_available() == true` and that
  `outgoing_capabilities()` advertises
  `kdeconnect.shareinputdevices.request` after the boot path
  completes.

## How it was verified

The lane's load-bearing test is
`enable_session_backend_drives_full_v1_sequence` in
`tests/shareinputdevices_m4_boot_attach.rs`. It uses the M2 lifecycle
test's pattern (private dbus-daemon + `DBUS_SESSION_BUS_ADDRESS` +
`FakePortal` with a call ledger) and the M4 wiring test's pattern
(unix socketpair + minimal fake EIS peer thread that completes the
EIS handshake and exits).

### Red — before the fix

The new test was written and run against the unfixed
`enable_session_backend` (which only probed and returned). Output:

```
running 1 test
test enable_session_backend_drives_full_v1_sequence ... FAILED

failures:

---- enable_session_backend_drives_full_v1_sequence stdout ----

thread 'enable_session_backend_drives_full_v1_sequence' (376481) panicked at tests/shareinputdevices_m4_boot_attach.rs:486:13:
v1 sequence did not reach Enable within 2s; calls so far: []
```

The fake's call ledger is empty: the production code stopped at the
probe. No `CreateSession`, no `ConnectToEIS`, nothing. The test
panics at the polling loop's deadline.

### Green — after the fix

With `enable_session_backend` now calling
`self.activate_portal_session().await` after the probe passes:

```
running 1 test
test enable_session_backend_drives_full_v1_sequence ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
```

The fake's call ledger now has all five calls in spec order:
`CreateSession → ConnectToEIS → GetZones → SetPointerBarriers →
Enable`. `portal_backend_available()` returns true;
`outgoing_capabilities()` advertises
`kdeconnect.shareinputdevices.request`.

### Gates

All three gates pass on the lane branch:

- `cargo test --no-fail-fast` (full suite, no TMPDIR override):
  **1227 passed, 0 failed, 4 ignored** (the 4 ignored are the USB
  integration tests gated on a real Android device; baseline is
  1226 + 1 new = 1227). The M2 portal lifecycle tests (5), M3 EI
  socketpair tests (20), M4 wiring tests (3), and the new M4 boot
  attach test (1) all pass together.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo fmt --check`: clean.

### Failure shape verified by inspection

`activate_portal_session` already degrades to `warn!` and returns
inert on every internal failure (no stashed conn, `PortalSession::start`
errored, `EiReceiver::new` errored, dedicated-thread spawn errored,
oneshot receivers dropped, disconnect path). `enable_session_backend`
inherits the same shape: a failed activation logs a warn and returns
inert; `backend_available` stays false; capability advertisement
stays gated. The boot path cannot panic or fail daemon boot on a
portal-side error. The M3 + M4 unit/integration tests in
`shareinputdevices_ei_socketpair.rs` and `shareinputdevices_m4_wiring.rs`
exercise the same code paths and stay green, confirming the failure
shape is preserved.

## Critique — blunt

The brief is correct that the seam is feasible: `zbus::Connection::session()`
respects the process-level `DBUS_SESSION_BUS_ADDRESS`, and the M2
lifecycle test already exploits this. The brief is also right that
this is parity, not invention — the cpp's
`shareinputdevicesplugin.cpp::ShareInputDevicesPlugin::enable()` starts
the InputCapture session at plugin enable, not at a later "EI transport
attaches" trigger. The M3 lane's "wait for EI to attach before
arming" rationale (`an armed InputCapture barrier with no EI consumer
would capture the user's cursor with nothing forwarding the promised
mousepad.request stream`) was a sound argument against a `start()` call
before M3 existed, but it no longer holds once M3 + M4 are merged.
This lane's boot-time hookup is the right shape.

**Where I think the brief is thin or could be tightened:**

1. **No negative-path test.** The test only covers the happy path
   (probe passes → v1 sequence runs). It does NOT cover the
   probe-failure path (no v1 calls, no `backend_available` flip) or
   an activation-failure path (e.g. CreateSession returns
   user-cancelled, no `Enable` on the ledger). The brief asks for
   "failed activation must NOT panic or fail daemon boot" but
   doesn't ask for a test that proves it. The existing M2 lifecycle
   test `v1_session_aborts_on_empty_zones` already covers the
   "PortalSession::start fails" branch in isolation, and the M3 +
   M4 tests pass when wired into the boot path here, so the
   fail-inert shape is exercised at the type level. But a
   test that drives `enable_session_backend` with a broken
   portal (e.g. `version = 0` or `GetZones` returning empty)
   and asserts the plugin stays inert would be a stronger pin.
   I left it out because the brief said not to build new
   injection mechanisms just for this, and the M2 test
   already covers the underlying behavior.

2. **The fake portal in the new test duplicates the M2 one.** The
   `FakePortal` + `FakePortalState` + `setup()` + `DaemonGuard`
   pattern is now in three places: `shareinputdevices_portal_lifecycle.rs`,
   `shareinputdevices_m4_wiring.rs` (without the call ledger), and
   the new `shareinputdevices_m4_boot_attach.rs` (with the call
   ledger + socketpair). The shape differences are small enough
   that a shared `tests/shareinputdevices/_support.rs` module would
   be a clear win — but the brief scoped this lane narrowly
   (close the activation gap), and refactoring the test
   infrastructure is its own lane with its own trade-offs (e.g.
   `tests/_support` modules make individual test files harder to
   read in isolation). I left the duplication. The next
   person to add a 4th fake-portal test should probably do the
   refactor.

3. **The lane's "test" deliverable verifies less than the
   activation's full behavior.** It pins the call sequence and the
   capability flip, but NOT the Activated-signal routing, the EI
   receiver's activation-gate behavior, or the unified consumer's
   `biased;` select. Those are covered by the M4 wiring tests
   (`m4_activated_signal_routes_to_consumer_via_session`,
   `m4_input_relays_through_gate_and_consumer`) at the per-piece
   level. The brief's contract is "the boot path drives the v1
   sequence," and that's what the new test pins. The other
   behaviors were already tested in M4 and stay tested in M4.
   Splitting the test surface this way is fine, but it does mean
   the new test could pass while the boot path is broken in a way
   that the M4 tests don't cover (e.g. `enable_session_backend`
   calling `activate_portal_session` but discarding the
   `Arc<PortalSession>`). I considered a stronger end-to-end
   test that fires an `Activated` signal after the boot path and
   asserts the unified consumer emits a
   `kdeconnect.shareinputdevices.request` — that would prove
   the whole pipeline is wired. I left it out for two reasons:
   the brief scoped the test to "the v1 sequence runs," and
   the M4 wiring test's signal-routing test already pins the
   end-to-end Activated path at the per-piece level. Adding a
   third copy of the same signal routing under a different
   harness is duplication without new coverage.

4. **The fake EIS peer's "drop the eis_ctx" move is a load-bearing
   trick.** I needed the peer's `start()` to return so
   `wire_rx` and `disconnect_rx` could be sent back to the
   main thread, and I needed the dedicated pump thread to
   exit so the test wouldn't hang at teardown. The minimal
   solution was a peer that completes the handshake then
   drops the EIS context (which closes its half of the
   socketpair, which makes the receiver's pump see EOF,
   which fires the disconnect arm, which exits the drive
   future, which lets the thread exit). It works, but
   it's a chain of "and then this also fires" that's not
   obvious from the test name. A code comment in
   `spawn_minimal_eis_peer` calls it out; the rest of the
   test code references it as the rationale. If the
   disconnect semantics ever change (e.g. someone removes
   the EOF arm because "real EIS never just dies"), this
   test will hang at teardown and the next person will
   have to figure out why.

5. **The doc comment on `enable_session_backend` claims the boot
   path is "parity" with the cpp, but doesn't reference the
   exact upstream site.** It says "The cpp upstream starts the
   InputCapture session at plugin enable — boot-time activation
   after a passed probe is parity, not invention." A more
   specific reference (e.g. `shareinputdevicesplugin.cpp::enable()`)
   would help a future maintainer trace the claim. I left
   it vague because (a) the comment in the source repo's
   file-level docstring (line 63-68 of mod.rs) already says
   "M1's plugin is NOT registered with the loader" and
   describes the loader-vs-bootstrap relationship, and
   (b) the lane is intentionally narrow. A future
   cross-reference cleanup could pin it tighter.

# Task #1042 M4-wire follow-up — input-relay test findings

Branch: `wip-test-shareinputdevices-m4-relay`. One commit lands the
new test + the harness extension it needed. No production code
changed — the M4 wiring (PR #29 / #1042) was already complete and
this lane is the load-bearing integration test the previous lane's
brief flagged as missing.

## What changed

### `tests/shareinputdevices_m4_wiring.rs` — added `m4_input_relays_through_gate_and_consumer` (+ extended harness)

**New test (lines ~1080-1240).** Red-before-green oracle for the
unified consumer's `body = wire_rx.recv()` arm — the relay branch
that the brief said had ZERO coverage. Sequence:

1. Fake portal + fake EIS peer up; pump thread handshake complete.
2. Emitter drives seat + virtual pointer device with the receiver's
   bound caps (`Keyboard | Pointer | Button | Scroll`), `resumed`,
   then `start_emulating(7)` to arm the activation gate
   (`eis_sequence=7, activation_id=0 → should_queue()=true`).
3. Pre-Activated: pointer motion `(5.0, 7.0)` + `BTN_LEFT` press +
   `BTN_LEFT` release — every event queues into
   `ActivationGate::pending`. Oracle asserts NO outbound packet
   reaches the recording consumer for 150ms (the wire_rx arm must
   stay silent while the gate is armed).
4. Emit `Activated` with `activation_id=42` (mismatches `7` and
   would be the next test's regression trap if anyone removes the
   requirement; the consumer's drain uses the wire_rx payloads, not
   this id, so the mismatch is incidental but worth pinning) and
   `cursor_position=(50.0, 100.0)`. The signal handler fires two
   side effects in order: (a) decode + push `ActivatedEvent` onto
   `activated_tx`, (b) clone the `Arc<EiReceiver>` out of the slot
   and call `handle_activated(42)`. The receiver's
   `handle_activated` drains the gate's queue in arrival order and
   feeds each drained body through the production `wire_tx` channel.
5. Oracle asserts the packet sequence on the recording consumer:
   - FIRST: `kdeconnect.shareinputdevices.request` with
     `{exitEdge: 2, deltax: 50.0, deltay: 100.0}` (barrier p1=(0,0)
     on the 1920×1080 zone, Edge::Left — `deltax/y = cursor verbatim`).
     The `biased;` select in `run_test_consumer` (the test's mirror
     of `activate_portal_session`'s production consumer at
     mod.rs:781-887) processes the activated arm FIRST, so the cpp's
     `started(deltax, deltay)` always precedes queued events on
     every select iteration.
   - THEN: `kdeconnect.mousepad.request` with `{"dx": 5.0, "dy": 7.0}`
     (motion body — `plan_motion(5.0, 7.0)` verbatim, dx/dy pass
     through).
   - THEN: `kdeconnect.mousepad.request` with `{"singlehold": true}`
     (BTN_LEFT press — `plan_button(Left, Press)` verbatim at
     mod.rs:207).
   - THEN: `kdeconnect.mousepad.request` with `{"singlerelease":
     true}` (BTN_LEFT release — `plan_button(Left, Release)` at
     mod.rs:208; release arrived LAST in the fake peer, must surface
     LAST on the wire — pins drain order).
   - NOTHING further for 200ms (no spurious packets after drain).

**Harness extension.** Extended `spawn_fake_eis_peer` to accept an
optional `cmd_rx: Option<tokio::sync::mpsc::Receiver<EisCommand>>`.
The peer thread now maintains `Option<EisSeat>` + `Option<EisDevice>`
in thread-local state (reis ties them to the `eis::Context`, which
owns the socketpair fd, so they cannot cross thread boundaries)
and applies each `EisCommand` synchronously to that state. The drain
loop interleaves command processing with the post-handshake read
loop so the peer's read queue never starves the test's send queue.

Added `EisCommand` enum (six variants: `AddSeat(caps)`,
`AddDevice(caps)`, `Resumed`, `StartEmulating(seq)`,
`PointerMotion(f32, f32)`, `Button(u32, bool)`) plus a `FakeEisEmitter`
test-side wrapper that exposes sync `try_send` helpers (the test
runs on a tokio runtime but the emitter calls are sync; `try_send`
is sync on `tokio::sync::mpsc`).

Refactored `setup_m4_harness(session_handle)` →
`setup_m4_harness(session_handle, with_emitter: bool)`. The two
existing tests pass `false` and get `emitter: None` — their behavior
is byte-for-byte unchanged. The new test passes `true` and gets
`Some(FakeEisEmitter)`. The harness now clones the
`EisRequestConverter`'s `Connection` twice: once for the
`handshake_complete` oneshot (unchanged path), once into a
`connection_for_thread` local that the command dispatch uses.

**Capability honesty (the r3 seat-widening note, called out in the
brief).** The new test advertises the seat with the receiver's
`bound_caps` (`Keyboard | Pointer | Button | Scroll`). Reis's
`Seat::add_device` rejects a Bind that asks for capabilities the
seat didn't advertise (`request.rs:851`:
`if !self.0.advertised_capabilities.contains(capability) → skip`).
If the test advertised a smaller set, the receiver's
`seat.bind_capabilities(bound_caps)` would land in the fake peer's
`handle_seat_request` and the bind would be dropped — the receiver
would hang on `DeviceAdded` forever. The same contract is pinned
by `seat_bind_reaches_the_eis_peer` in the M3 socketpair suite; the
M4 lane replicates the contract because the harness uses the same
reis high-level helpers (`connection.add_seat`,
`seat.add_device`).

**`connection.add_seat` auto-flush.** Unlike `seat.add_device` /
`device.resumed()` / `device.start_emulating()`, `add_seat` does
NOT auto-flush (reis's `Connection::add_seat` returns a `Seat` and
the new-seat event sits in the write buffer until `flush()`). The
peer thread's `apply_eis_command(AddSeat)` flushes explicitly right
after the `add_seat` call; without it the receiver would process
the bind before seeing the seat and the bind would target an
object the EIS side hadn't advertised yet.

## How it was verified

The brief's gates, run in this order against the warm target dir
`CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m4-ei`:

### 1. `cargo fmt --check`

```
$ cargo fmt --check; echo "exit=$?"
exit=0
```

Clean across the whole crate after `cargo fmt -- tests/shareinputdevices_m4_wiring.rs`
applied the file-local reformat (long-line wraps for the two
`tokio::time::timeout(...).await` calls and the `.as_ref().ok()
.and_then(|o| o.as_ref()).map(...)` chain on the spurious-receive
assert).

### 2. `cargo clippy --all-targets -- -D warnings`

```
$ cargo clippy --all-targets -- -D warnings 2>&1 | tail -5
    Checking rust-connect v0.1.0 (/tmp/delegate-rust-connect-wip-test-shareinputdevices-m4-relay)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 22.83s
```

Zero warnings. The `EisCommand` enum's `#[derive(Debug, Clone)]`
plus the `FakeEisEmitter` wrapper are now used by the new test, so
the previous dead-code warnings are gone.

### 3. `cargo test --no-fail-fast` (FULL suite, no TMPDIR override)

```
$ env -u TMPDIR cargo test --no-fail-fast 2>&1 | grep "^test result:"
... 33 binaries ...
test result: ok. 1226 passed; 0 failed; ...
```

The M4 wiring binary alone:

```
$ cargo test --test shareinputdevices_m4_wiring
running 3 tests
test m4_input_relays_through_gate_and_consumer ... ok
test m4_ei_peer_disconnect_flips_backend_available ... ok
test m4_activated_signal_routes_to_consumer_via_session ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The M4 binary count went from **2 → 3** (+1 new test). Total across
the whole crate: **1226 passed, 0 failed, 0 regressed**.

### 4. Mutation oracle (TDD discipline — characterization check)

The new test passed first run because the M4 wiring is complete
(production code matches the oracle). To confirm the assertions
actually catch regressions, I mutated each oracle and ran:

- **Wrong motion body** (`dx: 5.0 → 99.0`):
  ```
  assertion `left == right` failed: motion body must match
  `plan_motion(5.0, 7.0)` exactly; got {"dx":5.0,"dy":7.0}
    left: Object {"dx": Number(5.0), "dy": Number(7.0)}
   right: Object {"dx": Number(99.0), "dy": Number(7.0)}
  ```
- **Wrong biased-select order** (expected first packet to be
  `mousepad.request` instead of `shareinputdevices.request`):
  ```
  assertion `left == right` failed: first packet after Activated
  must be the activation announcement; got body={"deltax":50.0,
  "deltay":100.0,"exitEdge":2}
    left: "kdeconnect.shareinputdevices.request"
   right: "kdeconnect.mousepad.request"
  ```
- **Gate bypass** (asserted a premature packet WAS expected
  during the armed window):
  ```
  test m4_input_relays_through_gate_and_consumer FAILED
  ```
  (the consumer correctly stayed silent, so the test failed on
  the inverse assertion).

Each mutation reverted; the green baseline restored.

## Critique — blunt

The brief is the right test to add. Two things it gets wrong or
leaves open, and one thing I'd push back on as a maintainer.

**1. The brief's test is a positive oracle for existing behavior,
not a red→green TDD cycle.** The previous M4 lane shipped the
production wiring (commit 289bc54 on `feat-shareinputdevices-m4-
wire`); this lane adds the coverage it implied but didn't include.
Writing the test against already-working production code means the
test passes first run — TDD's "watch it fail" rule can't apply
because there's no failing code to flip green. The mutation oracle
above is the next-best discipline, but it doesn't catch a class of
bug: a future refactor of `activate_portal_session` that breaks
the relay arm AND the consumer's recording-shape in the same
change would still satisfy this test's `serde_json` equality check
against a mutated `plan_motion`. The M3 socketpair tests are
sharper here because they exercise the planner + transport
end-to-end at the production-shape level (every keymap fixture
has an explicit `_↔_wire` assertion). The M4 relay test could
use the same belt-and-braces: assert that the planner's
*production* `plan_motion(5.0, 7.0) → {dx: 5.0, dy: 7.0}` was the
*source* of the wire body, not just that the body happens to
match. Today the test pins the body shape against a literal
`serde_json::json!{...}`, which is a brittle coupling to the
planner's current shape; if `plan_motion` adds a field, the test
breaks for the wrong reason. Fix: call `plan_motion(dx, dy)`
directly in the test (it's `pub` in mod.rs) and assert
`body == serde_json::to_value(plan_motion(5.0, 7.0))?`. I left
this for a follow-up — the literal-shape assertion is what the
brief asked for, and stringing planners into tests pulls mod.rs
into the test module's compile graph.

**2. The `FakeEisEmitter` API exposes reis types in the test
public surface.** `BitFlags<DeviceCapability>`, `EisDevice`,
`EisSeat` (via `seat.clone()` in `apply_eis_command`) are reis's
public types — pinning them in the test means a reis upgrade that
renames or restructures the type will break the M4 wiring test
even if the runtime contract is unchanged. The M3 socketpair
suite has the same coupling; the M4 lane doesn't make it worse,
but it does multiply the exposure (one more test binary pinning
the same types). A future refactor would help: extract a tiny
`TestEisEmitter` trait in the test crate with methods like
`arm_gate(seq: u32)` and `emit_motion(dx: f32, dy: f32)`, and
implement it against reis. Not worth the churn now — reis is on
0.7.1 and the test types are already locked in.

**3. The brief pins `activation_id = 42` while the gate's
`eis_sequence = 7`.** The receiver's gate semantics are
`should_queue = eis_sequence > activation_id` — once
`handle_activated(N)` runs, `activation_id := N` and the gate
opens regardless of `eis_sequence`. Setting activation_id to any
non-matching value is fine for THIS test (the consumer reads the
queued bodies, not the activation_id), but the mismatch would
confuse a future maintainer who reads the test as a regression
trap for "what happens when activation_id != eis_sequence". The
production cpp at `inputcapturesession.cpp:288-296` always passes
the matching id (the portal Activated signal carries it). Either
pin both to 7 (mirror production) or comment the mismatch
explicitly. I commented it inline.

**4. The peer thread's command loop drains `cmd_rx` in chunks of
16 per loop tick.** That's a magic number. With the 5ms loop
period, 16/5ms = 3200 commands/sec on the cap side; the test sends
~8 commands per run, so the cap is unreachable in practice. But
the magic number still deserves a `const MAX_COMMANDS_PER_TICK: u32
= 16;` and a one-line WHY (otherwise an unbounded drain could
starve the post-handshake read loop and the receiver's bind would
never land). Left inline.

**5. The `connect_to_eis` fake portal takes the socketpair fd
with `Option::take()`** — once. The new test inherits that and
inherits the consequent "ConnectToEIS called twice or no socketpair
installed" panic. That's correct for a single-session test, but
if a future test wants to start a second session against the same
fake portal it'll panic before the harness reports the real
problem. Pre-existing limitation; not mine to fix in this lane,
but worth a `// TODO: re-install the socketpair fd for multi-
session tests` note on the fake portal. Did not add — out of
scope.

**6. I did not touch production code.** The brief said
"do not touch production code unless a test written from the
oracle fails." The test passed first run, so I didn't have to.
Recorded here for the lane ledger.

## Files changed

```
tests/shareinputdevices_m4_wiring.rs | 460 +++++++++++++++++++++++++++++++--
1 file changed, 450 insertions(+), 10 deletions(-)
```

## Test counts

- M4 wiring binary: **2 → 3 tests** (+1 new)
- Whole crate: **1226 passed, 0 failed, 0 regressed**

## Things deliberately NOT done

- No push (`git push`).
- No PR (`gh pr create`).
- No merge.
- No edits to production code (`src/`).
- No edits to FINDINGS.md beyond this lane's record (the prior
  lane's findings are gone — this file replaces them per the
  brief's "Replace FINDINGS.md with your own").
- No new fixtures (`tests/fixtures/upstream-wire/...`) — the
  wire-body shapes this lane asserts are planner-derived, not
  upstream-fixture-derived, so no fixture is warranted.
- No refactor of the `FakeEisEmitter` reis-type exposure
  (critique #2) — out of scope.
- No fix to the activation_id/eis_sequence mismatch surprise
  (critique #3) — documented inline instead.

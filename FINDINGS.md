# M3 panel round 2 — fix lane

Internal review artifact. Not for public distribution. PR bodies and any
audience-facing prose must stand alone; they must not reference this file,
this worktree, delegate, cypher-gate, or any other fleet-internal machinery.

## What changed

Two verified defects and a small hygiene set, all on branch
`fix-shareinputdevices-m3-panel-r2` (worktree only; no push, no PR,
no merge).

### Fix 1 — the seat bind is now flushed (P1)

`src/plugins/shareinputdevices/ei.rs`:

- `EiReceiver::start` clones the `ei::Context` (cheap — `Backend` is
  `Arc<BackendInner>` per reis `wire/backend.rs:67`) and threads the
  clone through `pump` to `dispatch`.
- `dispatch`'s `SeatAdded` arm now calls `seat.seat.bind_capabilities(*bound_caps)`
  AND `context.flush()` — the reis side buffers the bind (`event.rs:901-907`)
  and the only post-handshake flushes in the crate are the Ping responder
  (`event.rs:226-233`, server-initiated only) and explicit
  `Context::flush` / `Connection::flush` calls (`ei.rs:120-127`,
  `event.rs:88-94`, `request.rs:122`, `handshake.rs:112/192`).
  Without the explicit flush the bind sits in the write buffer
  forever and a real EIS (mutter / KWin) never sees it. libei's
  contract is that an EIS creates devices only in response to a
  received seat bind, so the receiver would silently hang with no
  `DeviceAdded`, no input, no error.

`tests/shareinputdevices_ei_socketpair.rs`:

- The fake EIS keeps reading after the handshake instead of stopping.
  `setup()` now publishes the first `EisRequest::Bind`'s `capabilities`
  on a oneshot (`bind_rx`) so tests can observe the bind.
- New test `seat_bind_reaches_the_eis_peer_before_devices` asserts
  the bind arrives within 2s of `add_seat` — and specifically arrives
  BEFORE the test calls `add_device`, mirroring the production
  ordering. Pre-fix this times out (bind sits in the buffer);
  post-fix it resolves with the bound capability set.
- All 13 existing tests that advertised a subset of
  `Keyboard | Pointer | Button | Scroll` on their seats now advertise
  the full set. reis's `EisRequestConverter::handle_seat_request`
  rejects a Bind that asks for capabilities the seat didn't advertise
  with `RequestError::InvalidCapabilities` (`request.rs:418-431`),
  so a test that advertised only `Pointer | Button | Scroll` would
  panic in the fake's drain loop the moment the receiver's bind
  flushed. The fix is test-scaffolding only — per-test behavior is
  unchanged (each test still asserts the same wire body for the
  single event type it exercises).

### Fix 2 — state-free keysym→text lookup (P2)

`src/plugins/shareinputdevices/ei.rs`:

- `keysym_to_text` was rewritten to take `keysym: xkb::Keysym` and call
  `xkb::keysym_to_utf8(keysym)` (the free function on `Keysym`,
  equivalent to libxkbcommon's `xkb_keysym_to_utf8`). Pre-fix it
  called `state.key_get_utf8(keycode)` — the state-based lookup that
  applies xkbcommon's Control and capitalization transformations.
  With Control active and unconsumed, `xkb_state_key_get_utf8` returns
  `"\x03"` for `c`; the existing `< 0x20` filter then erased it,
  putting `{key: "", ctrl: true}` on the wire — every Ctrl shortcut
  silently lost.
- The call site in `dispatch`'s `KeyboardKey` arm passes `keysym` (not
  `state, xkb_keycode`). The keysym already reflects level selection
  via `key_get_one_sym` (same split the cpp uses at
  `inputcapturesession.cpp:433-436`).
- The `< 0x20` filter stays — that mirrors Qt's
  `QKeyEvent::text()` filter for Escape / Backspace / Tab, which
  `xkb_keysym_to_utf8` does NOT apply. We get the cpp wire shape
  for control keys (empty text) AND for letter keys (preserved text).

`tests/shareinputdevices_ei_socketpair.rs`:

- `TEST_KEYMAP` adds `<AC03> = 54;` and `key <AC03> { [ c ] };` so a
  letter key (evdev `KEY_C` = 46 → xkb 54 → `XK_c`) is addressable
  in the Ctrl-shortcut test. The earlier keymap only had a control-char
  path (Escape, Backspace) and a letter key whose state-based lookup
  collapses under Control.
- New test `ctrl_shortcut_keeps_the_letter_text_on_the_wire` depresses
  Control (mask bit 2) and presses `KEY_C`, asserting
  `key == Some("c")` AND `ctrl == Some(true)`.

### Hygiene

`src/plugins/shareinputdevices/ei.rs`:

- **Module doc** gains an M4 integration note: the drive future is
  `!Send` (reis's `EiConvertEventStream` and xkbcommon's `xkb::State`
  are both `!Send`), the daemon's `main.rs:10` is `#[tokio::main]`
  (multithreaded by default), so M4's wiring must run the pump on a
  dedicated thread with a current-thread runtime (or explicit
  `LocalSet`) and forward the resulting `WireBody` mpsc across thread
  boundaries.
- **`start()` doc** says `spawn_local` / `current_thread` / inline
  `await` only; explicitly forbids `tokio::spawn` on a multithreaded
  runtime (the `Send` requirement is the only signal).
- **KeyboardKey arm** comment now acknowledges the xkbcommon
  not-to-be-mixed warning between `update_key` and `update_mask`.
  Both warnings apply — faithful cpp port AND xkbcommon's caveat.
  Same tension in both ports; not a divergence to fix here.
- **`build_xkb_state`** switched from `try_clone + seek + take + read_to_end`
  to `try_clone + File::from + FileExt::read_exact_at(0)`. The old
  comment was wrong: an fd that crossed SCM_RIGHTS shares its open
  file description with the sender, including the seek offset the
  compositor left behind, and `seek(0)` on a `dup(2)`-cloned fd moves
  the sender's offset too (corrupting any later read it does). The
  old `seek` was protecting against an imaginary bug. `read_exact_at`
  is the positionless variant (`pread(2)` under the hood): it reads
  `buf.len()` bytes at offset 0 and leaves the file's seek position
  untouched.
- **Struct-level `#[allow(clippy::arc_with_non_send_sync)]`** and its
  near-duplicate comment block deleted. The lint fires at the
  `Arc::new` sites inside `new()`; those sites carry their own
  `#[allow]`s already.
- **Dead `EiReceiver::gate()` test seam** (the second
  `impl EiReceiver` block) deleted. `#[cfg(test)]` items are invisible
  to the integration crate; nothing in the suite or elsewhere called
  it. The activation-gate accessors used in the unit tests live on
  `ActivationGate` itself (`activation_id`, `eis_sequence`,
  `pending_len`).
- **In-file line-number cross-references** replaced with
  function/arm names (e.g., `ei.rs:486-490` →
  "the Button arm of dispatch's `plan_button.is_null()` drop";
  `ei.rs:523-525` → "dispatch's `EiEvent::ScrollStop | EiEvent::ScrollCancel`
  arm"; `ei.rs:392` → "pump's EOF exit"; `mod.rs:233-256` →
  "`plan_scroll`"; `mod.rs:267-274` → "`plan_scroll_discrete`";
  `mod.rs:956-965` → "`plan_scroll_discrete`'s x-not-negated
  invariant"). Cpp refs (`inputcapturesession.cpp:NNN`) left intact —
  those are external and don't drift on changes in this repo.

## How it was verified

### Baseline (HEAD 7dafc27)

```
$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei cargo test --no-fail-fast
...
test result: ok. 1217 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out
```

15 tests in the EI socketpair suite (`tests/shareinputdevices_ei_socketpair.rs`).

### Fix 2 RED — harness reworked, fix not yet applied

After the harness rework in commit `984e18d` (reading fake EIS, `bind_rx`
oneshot in `setup`, `TEST_KEYMAP` extended, `ctrl_shortcut_keeps_the_letter_text_on_the_wire`
test added, `keysym_to_text` still calling `state.key_get_utf8(keycode)`):

```
$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei \
    cargo test --test shareinputdevices_ei_socketpair --no-fail-fast
running 16 tests
...
test ctrl_shortcut_keeps_the_letter_text_on_the_wire ... FAILED
---- ctrl_shortcut_keeps_the_letter_text_on_the_wire stdout ----
thread 'ctrl_shortcut_keeps_the_letter_text_on_the_wire' (1103870) panicked at
tests/shareinputdevices_ei_socketpair.rs:1345:9:
assertion `left == right` failed: Ctrl+C must keep the letter text on the wire
(state-free keysym lookup); got
{"alt":false,"ctrl":true,"key":"","shift":false,"specialKey":0,"super":false}
  left: Some("")
 right: Some("c")
test result: FAILED. 15 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out;
              finished in 0.42s
```

The 15 pre-existing tests stayed green through the harness rework, satisfying
the brief's "the existing 15 tests must stay green unchanged in behavior".

### Fix 2 GREEN — `keysym_to_text` rewritten

```
$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei \
    cargo test --test shareinputdevices_ei_socketpair --no-fail-fast
running 16 tests
...
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;
              finished in 0.43s
```

### Fix 1 RED — bind not yet flushed

After Fix 2's commit, before Fix 1, with the new
`seat_bind_reaches_the_eis_peer_before_devices` test added:

```
$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei \
    cargo test --test shareinputdevices_ei_socketpair seat_bind_reaches_the_eis_peer_before_devices
running 1 test
test seat_bind_reaches_the_eis_peer_before_devices ... FAILED

---- seat_bind_reaches_the_eis_peer_before_devices stdout ----
thread 'seat_bind_reaches_the_eis_peer_before_devices' (1110340) panicked at
tests/shareinputdevices_ei_socketpair.rs:1422:14:
seat bind never reached the EIS peer — reis buffers bind_capabilities and the
receiver does not flush; the EIS would never create devices in production: Elapsed(())
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 16 filtered out;
              finished in 2.01s
```

The bind times out at the 2-second deadline. The pre-fix behavior is
exactly the production failure mode: bind sits in the write buffer, EIS
sees nothing, no devices ever arrive.

### Fix 1 GREEN — context.clone() + flush after bind_capabilities

```
$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei \
    cargo test --test shareinputdevices_ei_socketpair --no-fail-fast
running 17 tests
test seat_bind_reaches_the_eis_peer_before_devices ... ok
test disconnect_via_eof_signals_and_completes ... ok
test disconnect_event_alone_completes_pump_without_socket_close ... ok
test disconnect_via_explicit_event_signals_and_completes ... ok
test button_press_release_round_trip ... ok
test events_passthrough_when_not_armed ... ok
test scroll_delta_round_trip ... ok
test scroll_discrete_round_trip ... ok
test pointer_motion_round_trip ... ok
test keyboard_keymap_loads_and_emits_text ... ok
test ctrl_shortcut_keeps_the_letter_text_on_the_wire ... ok
test latched_shift_modifier_surfaces_as_shift_on_wire ... ok
test control_char_keys_emit_empty_text ... ok
test activation_gate_queues_until_activated ... ok
test gate_queues_scroll_until_activated ... ok
test scroll_stop_and_cancel_are_noops ... ok
test unmapped_keycode_does_not_panic_pump_survives ... ok
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;
              finished in 0.41s
```

### Gates (after hygiene)

```
$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei cargo fmt --check
(no output — clean)

$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei cargo clippy --all-targets -- -D warnings
    Checking rust-connect v0.1.0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 22.36s

$ CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei cargo test --no-fail-fast
...
passed=1219 failed=0 ignored=4 measured=0
```

1219 = 1217 baseline + 2 new tests (`ctrl_shortcut_keeps_the_letter_text_on_the_wire`,
`seat_bind_reaches_the_eis_peer_before_devices`). 4 ignored are the existing
usb_integration tests that require a real Android device on adb.

## Critique — blunt

The brief is largely correct in its diagnosis and Fix 1 is a real P1
defect — verified end-to-end through the fake EIS. A few things the
brief got wrong or under-specified, and a few things my fix doesn't
catch that a hostile reader should know about.

**Brief over-confidence on the reis auto-flush claim.** The brief said
"If reis 0.7.1 DOES auto-flush somewhere the integrator missed, record
the evidence in FINDINGS and downgrade to the missing-test fix only."
I grepped every `flush` site in reis 0.7.1 — the only post-handshake
flushes are the Ping responder (server-initiated only, never fires
against a receiver context) and explicit `Context::flush` /
`Connection::flush` calls. Fix 1 stands as a genuine P1; the
missing-test downgrade path does not apply.

**The seat-capability widening in tests is a small honesty tax.** Pre-fix,
the existing tests advertised `Pointer | Button | Scroll` (or just
`Keyboard`) on their seats because the bind was buffered and the fake
never saw it. Post-fix the bind flushes, reis's
`handle_seat_request` rejects a Bind that asks for capabilities the
seat didn't advertise, and the fake's `expect` on `handle_request`
panics. So I widened 13 seats to the full set. That means we have
no test that exercises "what happens if the EIS only advertises
Pointer?" — that branch of reis's validation is now uncovered.
A test that wanted to cover it would need to bypass the bind path
(e.g., never let the receiver see `SeatAdded`), which is artificial.
Acceptable trade; flagging it.

**`control_char_keys_emit_empty_text` does not prove Fix 2.** That
test was added in the prior round for the `< 0x20` filter, which
Fix 2 doesn't touch. It does still pass post-fix (the keysym for
Escape is `XK_Escape`, and `xkb_keysym_to_utf8(XK_Escape)` returns
`"\x1b"` which the filter strips to `""`), but the test would also
have passed pre-fix. The load-bearing test is
`ctrl_shortcut_keeps_the_letter_text_on_the_wire`, which fails
pre-fix and passes post-fix. Document this if anyone wonders why
two tests look similar.

**The eis-side `connection.flush()` in the new test is a leaky
abstraction.** The new test explicitly flushes the EIS context after
`add_seat` so the seat event reaches the receiver. The existing tests
don't need to flush there because their `pointer_motion(...)` helper
flushes later. A future test author who writes
"add a seat, then add a device, then send an event" without flushing
will hang in the eis-side read loop waiting for the seat event to
deliver. The harness could paper over this by auto-flushing in
`add_seat`/`add_device` helpers, but that's a wrapper-layer fix
beyond the brief's scope. I added a `// Flush the eis-side so the
`seat` event reaches the receiver.` comment so the next reader sees
why.

**The state-free lookup changes behavior outside Latin-1.** Qt's
`lookupStringNoKeysymTransformations` strips chars `< 0x20` after
the keysym→text lookup; our `chars().filter(|&c| (c as u32) >= 0x20)`
does the same per-char. Both are char-based (Rust `char` is a 32-bit
Unicode scalar value, but the filter casts to `u32` and compares),
so the behavior matches across the BMP. Outside the BMP,
`xkb_keysym_to_utf8` returns UTF-8 multi-byte sequences whose chars
land on supplementary planes; the filter still passes them through
if their codepoint is `>= 0x20`. Qt's filter is char-based on its
`QString` (UTF-16 code units) and would behave the same way for
the same codepoints. So parity holds for non-ASCII keysyms (Greek,
Cyrillic, CJK, etc.). Not exercised in the suite — the test keymap
is ASCII-only — but the parity argument is sound.

**The build_xkb_state `try_clone` is still load-bearing, but for a
different reason than its old comment claimed.** The dup'd fd shares
the open file description with the sender (including the seek offset).
We don't care about the offset because `read_exact_at` uses `pread(2)`
which is positionless. The `try_clone` exists only because reis hands
us `&OwnedFd` and `File::from(OwnedFd)` consumes. If reis ever switches
to handing out an `OwnedFd` by value (or adds a positionless reader on
the keymap wrapper), we can drop the `try_clone` and the `File` round-
trip entirely.

**The `!Send` reactor note in the module doc is a guess.** I assert
M4 needs a dedicated thread with a current-thread runtime because the
daemon's `main.rs:10` is `#[tokio::main]` (multithreaded by default).
I didn't verify the daemon's actual runtime configuration — there could
be a `flavor = "current_thread"` override elsewhere, or M4 could
decide to spawn a fresh runtime. The note is correct in shape (it
identifies the constraint and the choice the wiring must make) but
should be re-checked when M4 starts.

**I did not try to break the fix under load.** The new test asserts
the bind arrives within 2s. A real production scenario with the portal
sending a flood of seat/device events between the handshake and the
bind could in principle still see issues (backpressure, partial
flushes). The existing tests don't exercise that load shape. The fix
is correct in form (one explicit flush, one explicit place) but the
"no race with concurrent events" claim is empirical, not proven.

**What this lane doesn't fix.** M4 PortalSession wire-up (out of
scope per the brief). The keysym→Qt::Key table for `specialKey`
(out of scope, documented M4 deferral). The settle-sleeps in the
tests (out of scope, integrator ruling — Fix 1's reading-fake
rework is the foundation a future gating pass would use, not a
gating pass itself).

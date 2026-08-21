# FINDINGS — Task #1042 M3 panel round 1 (fix lane)

## What changed

Seven fixes applied to the shareinputdevices M3 EI transport
(`src/plugins/shareinputdevices/ei.rs`) plus its
`tests/shareinputdevices_ei_socketpair.rs` fixture, plus the
`.github/workflows/release.yml` apt-install line, plus a hygiene pass
on a dangling path reference in `Cargo.toml`. Tests added: 4 new
socketpair tests. Existing 11 tests preserved and green.

### A. Activation-gate check-then-queue — single lock scope

Five arms of `dispatch` (PointerMotion, Button, ScrollDelta,
ScrollDiscrete, KeyboardKey) used to acquire the gate lock TWICE per
event — once for `should_queue()` and once for `queue()`/send.
Between the two acquires, M4's wiring can deliver a D-Bus
`Activated` signal on a multithread runtime, draining the queue
mid-decision. The result is a stranded event sitting in `pending`
until the next activation — for buttons that's a held BTN_LEFT the
phone never sees released.

Each arm now holds one `let mut g = gate.lock().await;` covering both
the check and the queue/send. The guard is dropped before sending
on the pass-through path (the brief is explicit). The planner `body`
is computed only on the pass-through branch — today it was built
before the gate decision, discarded on the queue path, then rebuilt
at drain. Null-body mirror added in `handle_activated`'s replay so
the queue-then-drain path matches the live-path drop of BTN_RIGHT
release.

A race test is NOT included: single-`LocalSet` tests cannot
reproduce it (the race requires a multithread runtime where the
D-Bus `Activated` signal task interleaves with the pump). The fix
is structural — the two acquires become one.

### B. Break the pump on `EiEvent::Disconnected`

The Disconnected arm fired `disconnect_tx.send(true)` but kept the
`while let Some(event) = stream.next().await` loop open. reis's
`Connection::disconnected` (reis request.rs:106) calls
`shutdown_read` on the EIS side, NOT `shutdown_write` — the
Receiver's read end is unaffected, so the pump blocked forever on
the next `stream.next().await` after dispatching the event. The cpp
gets away with this only because the portal closes the socket too;
we don't rely on that.

The Disconnected arm is now handled inside `pump` directly (not via
`dispatch`) so the terminal protocol event ends the pump on its
own: `let _ = disconnect_tx.send(true); break;`. `dispatch`'s
signature loses the `disconnect_tx` parameter (it was only used for
the Disconnected arm).

The existing
`disconnect_via_explicit_event_signals_and_completes` test
continues to close the socket for hygiene, but its caveat comment
that framed the socket close as a "workaround" has been replaced —
the socket close is no longer load-bearing.

New test `disconnect_event_alone_completes_pump_without_socket_close`
sends ONLY the explicit Disconnected event (drops `eis_done_tx`
without sending, keeps `connection` alive) and asserts both
`disconnect_rx.changed()` resolves AND `drive` completes within 2s.
Red before fix (pump hung on `stream.next().await`); green after.

### C. Unmapped keycodes must not panic the pump

`debug_assert!(keysym.raw() != 0)` in the KeyboardKey arm panicked
debug builds on any keycode the delivered keymap mapped to
`XKB_KEY_NoSymbol`. Real keymaps have unmapped/vendor codes. The
spawned pump task died silently; `disconnect_tx` was dropped
without `send(true)`, so `disconnect_rx.changed()` resolved to
`Err(closed)` and the receiver was dead without a signal.

The assert is dropped. The path now treats NoSymbol like the
no-keymap path: warn + drop, no wire body. A deliberate divergence
from the cpp is documented inline: the cpp emits
`key(Key_unknown, mods, "")` for unmapped keys, but with
`specialKey` hardcoded 0 here a faithful `Key_unknown` is not
representable, so the transport drops instead of emitting a
contentless packet. M4's keysym→Qt::Key table restores cpp parity
by mapping NoSymbol to the literal `Key_unknown` code.

New test `unmapped_keycode_does_not_panic_pump_survives` sends an
unmapped keycode (50, no binding), asserts no body arrives, then
sends a mapped key (KEY_H → XK_h) and asserts the body still
arrives. Red before fix (panic at ei.rs:574 + pump dies);
green after.

### D. Modifiers: use the effective mask

`Modifiers::from_xkb_state` queried `STATE_MODS_DEPRESSED` only.
The cpp path (inputcapturesession.cpp:422-434) calls
`xkb_state_update_mask(depressed, latched, locked, ...)` and reads
modifiers from the resulting state via `QXkbCommon::modifiers` —
which is `STATE_MODS_EFFECTIVE` (depressed | latched | locked).
Latched (sticky-keys) and locked modifiers were dropped while
`key_get_utf8` (effective-state) already emitted the shifted text —
producing wire packets whose text and modifiers disagree.

One-token fix: `STATE_MODS_DEPRESSED` → `STATE_MODS_EFFECTIVE` in
all four mod-name checks. The doc comment now references cpp
:422-426 instead of the overclaimed "tests only the DEPRESSED
mask".

New test `latched_shift_modifier_surfaces_as_shift_on_wire` sends
a `KeyboardModifiers` event with latched=1 (XKB modifier bit 0 =
Shift), depressed=0, locked=0, group=0, then a KEY_H press, and
asserts `shift: true` on the wire body. Red before fix
(`shift: false`); green after.

### E. Filter control characters out of key text

The transport's text path claimed to mirror
`QXkbCommon::lookupStringNoKeysymTransformations` but omitted its
filter. `xkb_state_key_get_utf8` yields `"\x1b"`/`"\x08"`/`"\t"`
for Escape/Backspace/Tab — Qt strips those to empty before
emitting, so the cpp never sees them on the wire.

`keysym_to_text` now filters chars where `(c as u32) < 0x20`. A
string that becomes empty stays empty (verified by the test below).

New test `control_char_keys_emit_empty_text` presses KEY_ESCAPE
(evdev 1 → xkb 9 → XK_Escape → "\x1b") and asserts `key: ""` on
the wire body. Red before fix (`key: "\u{1b}"`); green after.

### F. release.yml needs libxkbcommon-dev

`.github/workflows/release.yml` installed only
`libssl-dev`/`libdbus-1-dev`/`pkg-config`; the `xkbcommon` crate
links unconditionally at build time, so release builds would fail
with `cannot find -lxkbcommon`. Added `libxkbcommon-dev` to the
apt line with the same why-comment style as `ci.yml`.

### G. Branch hygiene

- Removed dangling `plans/task-1042-m3-brief` reference from the
  `ei.rs` module header (the substance is preserved in the
  surrounding text — reis/xkbcommon choice, reactor shape, scope
  per M1).
- Removed dangling `plans/task-1042-m2-report.md` reference from
  the `Cargo.toml` dependency comment (the surrounding prose is
  preserved).
- Deleted dead `make_local_set` test helper (`#[allow(dead_code)]`,
  never called).
- Removed stale `#[allow(dead_code)]` on `bound_caps` (the field
  IS read at ei.rs:355/392/426 — the live path uses it for
  `seat.seat.bind_capabilities`).
- Replaced hand-rolled `memfd_create` syscall (number 319 is
  x86_64-only; aarch64 is 279) with `libc::memfd_create` (which
  dispatches per architecture).
- Wrapped `conn_rx.await` in `setup()` with the same
  `timeout(Duration::from_secs(5), ...)` idiom the test fixture
  already uses for `receiver.start()`.
- Fixed the `setup()` doc comment that referenced a nonexistent
  `each_test_local_set` macro — replaced with a description of the
  per-test `LocalSet::new()` + `local.block_on(&rt, ...)` pattern.

The same dangling `plans/` pattern exists in `mod.rs` from M1; left
alone, out of this lane's scope per the brief.

## How it was verified

`CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei` (warm
cache from the M3 lane; one cargo build at a time, no in-tree
target). Full suite ran WITHOUT any TMPDIR override.

### Red-before-green traces (per-fix)

**B.** `disconnect_event_alone_completes_pump_without_socket_close`
before fix:

```
running 1 test
test disconnect_event_alone_completes_pump_without_socket_close ... FAILED

failures:

---- disconnect_event_alone_completes_pump_without_socket_close stdout ----

thread 'disconnect_event_alone_completes_pump_without_socket_close' (1012250) panicked at tests/shareinputdevices_ei_socketpair.rs:928:14:
pump did not exit on Disconnected event alone: Elapsed(())
```

After fix: `ok`.

**C.** `unmapped_keycode_does_not_panic_pump_survives` before fix
(re-enabling the `debug_assert`):

```
thread 'unmapped_keycode_does_not_panic_pump_survives' (1018049) panicked at src/plugins/shareinputdevices/ei.rs:574:17:
no keysym for keycode 58
note: run with `RUST_BACKTRACE=1` to display a backtrace

thread 'unmapped_keycode_does_not_panic_pump_survives' (1018049) panicked at tests/shareinputdevices_ei_socketpair.rs:1035:14:
mapped key after unmapped timed out — pump must survive: Elapsed(())
```

After fix: `ok`.

**D.** `latched_shift_modifier_surfaces_as_shift_on_wire` before fix
(re-enabling `STATE_MODS_DEPRESSED`):

```
thread 'latched_shift_modifier_surfaces_as_shift_on_wire' (1020366) panicked at tests/shareinputdevices_ei_socketpair.rs:1139:9:
assertion `left == right` failed: LATCHED Shift must surface as shift:true on the wire; got {"alt":false,"ctrl":false,"key":"h","shift":false,"specialKey":0,"super":false}
  left: Some(false)
 right: Some(true)
```

After fix: `ok`.

**E.** `control_char_keys_emit_empty_text` before fix:

```
thread 'control_char_keys_emit_empty_text' (1022802) panicked at tests/shareinputdevices_ei_socketpair.rs:1211:9:
assertion `left == right` failed: Escape must produce empty text on the wire (filter <0x20); got {"alt":false,"ctrl":false,"key":"","shift":false,"specialKey":0,"super":false}
  left: Some("\u{1b}")
 right: Some("")
```

After fix: `ok`.

**A.** No new test — the fix is structural, the race requires a
multithread runtime, single-LocalSet tests cannot reproduce it.
The existing `activation_gate_queues_until_activated` and
`gate_queues_scroll_until_activated` stay green across the change
(verified in the full suite run below).

### Full suite (no TMPDIR override)

```
$ cargo test --no-fail-fast
... 30+ test binaries ...
test result: ok. 1039 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
... (and many more) ...
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
... (shareinputdevices_ei_socketpair)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
... (doc-tests)
```

Shareinputdevices socketpair (15 tests, was 11):

```
running 15 tests
test disconnect_via_explicit_event_signals_and_completes ... ok
test disconnect_event_alone_completes_pump_without_socket_close ... ok
test disconnect_via_eof_signals_and_completes ... ok
test events_passthrough_when_not_armed ... ok
test button_press_release_round_trip ... ok
test scroll_delta_round_trip ... ok
test pointer_motion_round_trip ... ok
test scroll_discrete_round_trip ... ok
test latched_shift_modifier_surfaces_as_shift_on_wire ... ok
test control_char_keys_emit_empty_text ... ok
test keyboard_keymap_loads_and_emits_text ... ok
test activation_gate_queues_until_activated ... ok
test gate_queues_scroll_until_activated ... ok
test scroll_stop_and_cancel_are_noops ... ok
test unmapped_keycode_does_not_panic_pump_survives ... ok

test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### Clippy

```
$ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.32s
```

### Format

```
$ cargo fmt --check
fmt exit: 0
```

## Critique — blunt

### 1. The Brief's structural fix for A is correct but the documentation story is now murkier than the code

The new `handle_activated` replay has a Null-body guard that mirrors
the live-path drop. The guard is currently unreachable for two
reasons: (a) the live path drops BTN_RIGHT release on the
pass-through branch (still); (b) on the queue branch, the brief's
"compute body only on the pass-through" rule means we don't
pre-screen for Null — we just queue raw. So the replay guard
handles a case that doesn't exist in the current code path. If a
future refactor moves the Null-screen out of the live path too,
the guard becomes load-bearing. The brief is correct to ask for it
defensively; future readers will be puzzled by an apparently
unreachable guard.

### 2. The Disconnected break introduces a behavior divergence from the cpp that's NOT documented in the change

Pre-fix, the cpp's `break` (inputcapturesession.cpp:373) implicitly
relies on the portal closing the socket. The Receiver inherits
that assumption and hangs forever on a buggy portal that sends
Disconnected without closing. Post-fix, the Receiver is more
defensive — the pump exits on the protocol event alone. That's an
improvement, but it means a divergence from the cpp that no test
in the existing fixture was designed to surface: the cpp would
hang; the Receiver exits cleanly. If a future maintainer "fixes"
the divergence by matching the cpp's behavior — i.e., reverting
the break — they'd reintroduce the hang. The pump's inline
comment ("reis does NOT EOF the stream on Disconnected") documents
this, but a single-line comment in a hot path is fragile. A
prominent module-level note ("BEHAVIOR DIVERGENCE FROM CPP: …")
would survive refactors better.

### 3. The unmapped-keycode divergence is documented inline but the production-case consequences are unanalyzed

The cpp emits `key(Key_unknown, mods, "")` for unmapped keys. The
Receiver drops the event entirely. In production this means: any
emulating portal sending an unmapped keycode silently swallows the
event. The Android consumer treats `specialKey: 0` as "use text" —
an unmapped key that the cpp routes to `Key_unknown` produces a
packet the consumer interprets as an empty-text key. The Receiver
produces no packet at all. The Android app's behavior in the
"empty packet" vs "no packet" case may differ (logging, focus
side-effects), and we haven't validated. M4's keysym→Qt::Key table
fixes this — but the brief says M4 by milestone definition, and
M3's divergence is a known regression vector.

### 4. The STATE_MODS_EFFECTIVE change might break an untested assumption in M1's planner

`plan_key` takes four independent bools (shift/ctrl/alt/super) and
emits them verbatim. Pre-fix, depressed modifiers only — which
meant a sticky-keys user pressing 'a' got "A" (from
`key_get_utf8`) with shift=false on the wire (from
`STATE_MODS_DEPRESSED`). The Android consumer's KeyListenerView
would have likely used the text field as authoritative and ignored
the modifier mismatch. Post-fix, both agree. But M1's tests pin
`shift: false` for the default-modifier path (`keyboard_keymap_loads_and_emits_text`
asserts `obj.get("shift").and_then(|v| v.as_bool()) == Some(false)`).
That test still passes because the default keymap has no Shift
binding and no Latched Shift is sent — but if M1's tests ever
grow a "Shift held" path, they'd need to assert `shift: true` now.
That's an M1 concern, not M3, but the M1 wire-shape contracts are
the pin.

### 5. The control-char filter's empty-string collapse is correct but the test only covers Escape

`key_get_utf8` returns control chars for at least Escape (`\x1b`),
Backspace (`\x08`), and Tab (`\t`). The test pins only Escape.
Backspace and Tab are not tested. If the filter were
incomplete (e.g., only filtered chars 0x01-0x08, missing 0x09-0x1f),
Backspace and Tab would catch it but Escape would not. The test
is cheap to extend (add a keymap binding, two more assertions),
and the brief said "extend the test keymap with a mapping that
produces a control char (or test the helper directly if it is
unit-reachable)". I added one mapping, not three. The filter
itself is straightforward (`(c as u32) >= 0x20`) and unlikely to
regress, but the test's coverage is narrower than the brief
suggested.

### 6. The hand-rolled `memfd_create` was a latent x86_64-only bug masked by CI's `ubuntu-latest`

The brief is right to flag this as a hygiene fix. The CI and
release pipelines run on x86_64 only (verified by the
`x86_64-unknown-linux-gnu` path in release.yml:33), so the bug
never surfaced in CI. A maintainer running `cargo test` on an
Apple Silicon box (darwin/aarch64 or linux/aarch64) would see the
syscall return `ENOSYS`. The fix to `libc::memfd_create` is
mandatory for the cross-arch story; the lack of a CI matrix that
catches it is a separate hygiene gap.

### 7. Things I deliberately did NOT do

- **Did NOT write a race test for fix A** — per the brief, the
  race requires a multithread runtime and single-LocalSet tests
  cannot reproduce it. The fix is structural.
- **Did NOT add the `set_pointer_barriers` + `Enable` wiring for
  the activation signal** — explicitly out of scope per the
  brief's silenced findings (M4 by milestone definition).
- **Did NOT add the keysym→Qt::Key table** — explicitly out of
  scope (M4 deferral). The current `specialKey: 0` for unmapped
  keys remains.
- **Did NOT add libxkbcommon-dev to the msrv job** — false
  positive per the brief: the job is `cargo check --locked`
  only, xkbcommon has no `build.rs`, nothing links at check time.
- **Did NOT tighten the 150-200ms settle sleeps in the tests** —
  the brief silenced that finding; touching nine test timings for
  zero blocker value risks flakes.
- **Did NOT add a backscroll test** — out of scope for this lane
  (the scroll_discrete/smooth paths are already pinned by the
  M3-follow-up-lane tests).
- **Did NOT delete the existing FINDINGS.md** — per the brief,
  the integrator disposes the artifact; this FINDINGS.md replaces
  it.

## Files changed

- `src/plugins/shareinputdevices/ei.rs` — fixes A, B, C, D, E,
  and the module-header hygiene from G.
- `tests/shareinputdevices_ei_socketpair.rs` — four new tests
  (B, C, D, E), updated caveat comment on the existing
  disconnect test, replaced hand-rolled `memfd_create`, removed
  dead `make_local_set`, fixed `setup()` doc comment, added
  `timeout` around `conn_rx.await`.
- `.github/workflows/release.yml` — fix F (libxkbcommon-dev).
- `Cargo.toml` — fix G (removed dangling `plans/` citation from
  the shareinputdevices dep comment).
- `FINDINGS.md` — this file.

## Test count before / after

| File | Before | After | Delta |
|---|---|---|---|
| `tests/shareinputdevices_ei_socketpair.rs` | 11 | 15 | +4 |
| Full suite | 1039 lib + others | 1039 lib + others | 0 |
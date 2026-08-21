# FINDINGS — Task #1042 M3 follow-up lane (scroll + disconnect tests)

## What changed

Added six new tests to `tests/shareinputdevices_ei_socketpair.rs` covering
the gaps M3 left in the EI transport's socketpair coverage. The five
existing tests (`pointer_motion_round_trip`, `button_press_release_round_trip`,
`activation_gate_queues_until_activated`, `events_passthrough_when_not_armed`,
`keyboard_keymap_loads_and_emits_text`) are untouched and still green.

New tests, all in the existing `LocalSet` + socketpair pattern:

| Test | What it pins |
|---|---|
| `scroll_delta_round_trip` | EI `ScrollDelta(dx, dy)` → emitted body equals `plan_scroll(dx, dy, 0, 0)` |
| `scroll_discrete_round_trip` | EI `ScrollDiscrete(dx, dy)` → emitted body equals `plan_scroll_discrete(dx, dy)` (y-negation is the planner's pinned job; this pins transport routing) |
| `scroll_stop_and_cancel_are_noops` | After a real `ScrollDelta`, sending `ScrollStop` and `ScrollCancel` produces no additional `WireBody` (mirrors upstream cpp `:418-421`) |
| `gate_queues_scroll_until_activated` | Gate armed (sequence=11, no `handle_activated` yet) → `ScrollDelta` + `ScrollDiscrete` queued, NOT emitted; `handle_activated(11)` drains both in arrival order |
| `disconnect_via_explicit_event_signals_and_completes` | EIS-side `connection.disconnected(...)` → `disconnect_rx` flips to `true` AND drive future completes (with socket close after the event, see Critique §1) |
| `disconnect_via_eof_signals_and_completes` | No explicit `Disconnected` event, just socket close → `disconnect_rx` flips to `true` AND drive future completes (the EOF path, ei.rs:392) |

Three small helper functions (`scroll_delta`, `scroll_discrete`,
`scroll_stop`, `scroll_cancel`) were added beside the existing
`pointer_motion` / `button_event` / `key_event` helpers — same
`device.frame(0); connection.flush()` close.

Test file total: **11 tests** (was 5).

## How it was verified

`CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei` (warm cache
from the M3 lane; one cargo build at a time, no in-tree target).

**Scoped re-run of the EI socketpair test file:**

```
$ cargo test --test shareinputdevices_ei_socketpair --no-fail-fast
running 11 tests
test disconnect_via_eof_signals_and_completes ... ok
test disconnect_via_explicit_event_signals_and_completes ... ok
test events_passthrough_when_not_armed ... ok
test pointer_motion_round_trip ... ok
test scroll_delta_round_trip ... ok
test scroll_discrete_round_trip ... ok
test button_press_release_round_trip ... ok
test keyboard_keymap_loads_and_emits_text ... ok
test activation_gate_queues_until_activated ... ok
test gate_queues_scroll_until_activated ... ok
test scroll_stop_and_cancel_are_noops ... ok

test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

**Full suite, no TMPDIR override:**

```
$ cargo test --no-fail-fast
... 30+ test binaries ...
test result: ok. 1039 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
... (and many more) ...
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
... (shareinputdevices_ei_socketpair)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
... (doc-tests)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

**Clippy:**

```
$ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 22.55s
```

**Format:**

```
$ cargo fmt --check
$ echo "fmt exit: $?"
fmt exit: 0
```

## Critique — blunt

### 1. reis's `Connection::disconnected` does NOT cause the Receiver's stream to EOF

This is the most consequential finding of the lane. The reis source at
`request.rs:106` calls `self.0.context.0.shutdown_read()` (i.e.
`socket.shutdown(std::net::Shutdown::Read)`) on the EIS side and comments
it as "Shutdown read end of socket, so all future reads will return EOF".

That comment is wrong about the direction. `Shutdown::Read` shuts down
the read half of **the local end of the socket** — meaning the EIS can
no longer read, which is a no-op because the EIS never reads in this
configuration. The Receiver's read end (which is the EIS's write end) is
**unaffected** by `Shutdown::Read` on the EIS side. The Receiver does
not see EOF.

This caused `disconnect_via_explicit_event_signals_and_completes` to
fail on first run: the receiver's pump dispatched `EiEvent::Disconnected`
and fired `disconnect_tx.send(true)` (so `disconnect_rx` flipped), but
then the pump's `while let Some(event) = stream.next().await` blocked
indefinitely on the still-open socket and `drive` never completed.

The upstream cpp at `inputcapturesession.cpp:372-374` has the same
implicit assumption:

```cpp
case EI_EVENT_DISCONNECT:
    qCWarning(KDECONNECT_PLUGIN_SHAREINPUTDEVICES) << "Disconnected from ei";
    break;
```

The cpp logs and breaks — relying on the portal to also close the
socket. That happens in production (the portal goes away), but the
cpp makes no defensive check. ei.rs:410-417 ports that faithfully.

**Two ways to address this:**

- **In the test (what I did):** after `connection.disconnected(...)`,
  also signal `eis_done_tx` and drop the `Connection` so the socket
  actually closes. This simulates production where the portal goes
  away after the Disconnected event. The test still verifies both
  oracle assertions (disconnect_rx flips on the event, drive completes
  on the subsequent EOF).

- **In the implementation (what I did NOT do, per the brief):**
  ei.rs:410-417 could `break` out of the pump's `while let Some` loop
  on `EiEvent::Disconnected` so the pump exits without depending on
  socket EOF. This is the safer behavior — it survives a buggy or
  crashed portal that sends Disconnected but doesn't close the socket.
  The cpp doesn't do this and the brief said "tests only, no new
  functionality, no refactors", so I left it. **M4 should consider
  this — the receiver hangs forever if the EIS sends Disconnected and
  then forgets to close the socket.**

### 2. The scroll planner's y-negation asymmetry is pinned in `plan_scroll_discrete` only, but `plan_scroll` does both paths

In `plan_scroll` (mod.rs:233-256), the smooth dx/dy pass through
verbatim AND the discrete path is OR'd in via the `anglePer120Step`
scaling. The y-negation in the discrete path is folded into the dy
calculation. This is the upstream cpp's behavior — the producer
always builds one packet with both smooth and discrete fields
populated as available.

My `scroll_delta_round_trip` exercises the smooth-only path
(`plan_scroll(dx, dy, 0, 0)`); my `scroll_discrete_round_trip`
exercises the discrete-only path via the separate
`plan_scroll_discrete` helper. I deliberately did NOT test
`plan_scroll(dx, dy, dx, dy)` — that would be testing planner
behavior, which is already pinned in `mod.rs:912-965`. The brief
explicitly said "Do not re-derive or re-test planner semantics —
pin that the TRANSPORT routes to them." The M3 lane's contract is
the transport, and these tests pin it.

### 3. The test names don't follow the file's existing pattern exactly

The existing five tests in this file use the `round_trip` / `until_activated` /
`passthrough_when_not_armed` / `loads_and_emits_text` suffixes. I kept
that style for the scroll tests but had to diverge for the disconnect
tests (`signals_and_completes`) because the assertion has two parts
(`disconnect_rx` flips AND drive completes). The name is a mouthful but
honest — splitting into two single-assertion tests would force each
one to re-do the socketpair setup, which is the expensive part of these
tests (~50ms each). A single test with both assertions is cheaper and
the assertions are clearly commented.

### 4. The `scroll_stop`/`scroll_cancel` no-op test depends on a 200ms wait

To prove no spurious `WireBody` arrives after `ScrollStop`/
`ScrollCancel`, the test waits 200ms and asserts the channel is empty.
That's a soft deadline, not an oracle. A regression that emits a body
on those events would show up — the channel would receive something
within ~150ms (the typical pump latency in the rest of the tests). If
someone broke this in a way that emitted a body much later, the test
would miss it. This is acceptable for a regression pin but a tighter
oracle (e.g., asserting the queue is empty for a longer window) would
catch more. I did not tighten it — the trade-off vs. test wall-clock
isn't worth it at this stage.

### 5. The brief said tests would mostly go green on first run — that was true for 5/6

`disconnect_via_explicit_event_signals_and_completes` failed on first
run and required an implementation-aware workaround (closing the socket
after the event). The other five went green on first compile/run as
predicted. The red-before-green discipline was upheld for the one that
needed it: the failure surfaced a real divergence (reis's
`shutdown_read` is the wrong direction), the divergence is documented
above, and the test workaround matches production behavior rather than
masking the bug.

### 6. Things I deliberately did NOT do

- **Did NOT modify ei.rs** to make the pump exit on `EiEvent::Disconnected`
  alone (see §1 above). The brief said tests only, and M4 should weigh
  the change.
- **Did NOT add scroll_capability to the keyboard-only device tests**
  (`keyboard_keymap_loads_and_emits_text`) — they don't need scroll,
  and the test was untouched per the brief's "five existing tests must
  stay untouched and green" rule.
- **Did NOT test the combined `plan_scroll(dx, dy, dx, dy)` path**
  (smooth + discrete in one packet) — that's planner behavior, pinned
  in `mod.rs`. The brief said don't re-test planner semantics.
- **Did NOT test reis-side frame coalescing** (multiple events per
  frame, or events split across frames). The reis test layer has its
  own coverage for that; this file tests our transport, not reis.
- **Did NOT test `KeyboardModifiers` directly** — the existing
  `keyboard_keymap_loads_and_emits_text` covers the keycode→text path
  with default modifiers, and the modifier-mask plumbing is a single
  line in `dispatch`. Adding a dedicated test would be padding.
- **Did NOT add an M4 follow-up `special_key` mapping test** —
  explicitly out of scope per the brief.

## Files changed

- `tests/shareinputdevices_ei_socketpair.rs` — six new tests + four
  scroll helpers, no changes to the five existing tests.
- `FINDINGS.md` — this file.

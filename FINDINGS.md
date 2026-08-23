# M4 panel round 3 — review findings

Internal review artifact for the panel M4 round 3 work (Task #1042
panel review `review-20260822T153838Z`). Covers the six fixes
applied between panel rounds 2 and 3.

## What changed

### Fix 1 — security headline: pairing gate on activation AND fan-out

**Pre-fix:** capabilities were recorded at TLS handshake completion
(BEFORE pairing); `ConnectionManager::send_packet` only checked
connection existence. An unpaired capable peer could be a relay
target for captured keystrokes / motion. `capable_consumer_ids`
returned connected-capable peers regardless of pairing state.

**Post-fix:**
- `capable_consumer_ids` is now `pub` (was `pub(crate)`) and takes
  a second parameter `Option<&PairingHandler>` — both consumer
  arms (the activation gate in `do_evaluate_after_event` and the
  wire consumer's fan-out filter in `do_activate`'s spawn task)
  call it with the same `Arc<PairingHandler>` wired by the loader.
- The loader's `load_default_plugins` now passes the pairing
  handler to `ShareInputDevicesPlugin::new().with_pairing_handler(
  pairing_handler.clone())` — and clones it for
  `SendNotificationsPlugin` (which takes it by value).
- A `seen_unpaired: HashSet<String>` field on the plugin fires a
  WARN log the first time an unpaired capable peer is observed
  (single per device, not a per-event spam).
- The wrapper requires: connection-live AND capabilities-present
  AND is-paired. Single-cap peers still fail the AND-match
  invariant (round 2 P5).

### Fix 2 — P1: refresh consumer snapshot AFTER `select!` returns

**Pre-fix:** both consumer arms read `capable_consumer_ids` ONCE
before the loop and cached it. Peers that connected or
disconnected during the idle wait were stale for every relayed
packet (and across every select iteration).

**Post-fix:** both arms (Activated + wire bodies) re-snapshot
inside each arm — `capable_consumer_ids` is called AFTER
`select!` resolves an event and BEFORE the relay decision. The
snapshot reflects the connection state at RELAY time, not at
WAIT time.

### Fix 3 — P2: close Disabled-signal callback-install window structurally

**Pre-fix:** the `Disabled`-signal callback slot was empty at
spawn-time of the signal handler (inside `PortalSession::start`),
so any `Disabled` signal that arrived before `set_on_disabled`
was called was dropped silently. Production installed the callback
from `do_activate` AFTER `PortalSession::start` returned.

**Post-fix:**
- `PortalSession` now carries a `pending_disabled: Arc<AtomicBool>`
  latch set TRUE by the signal handler when the slot is empty at
  `Disabled`-time.
- `set_on_disabled` consumes the latch: if TRUE, fires the
  callback immediately and clears the latch.
- Two WARN events surface the latch: `shareinputdevices_portal_
  disabled_latched` (set in the signal handler) and
  `shareinputdevices_pending_disabled_consumed` (set in
  `set_on_disabled` when the latch is consumed).
- This closes the race structurally — the latch is the install
  window, not a sender-side retry.

### Fix 4 — P2: discriminating coverage for AND + pairing

New file `tests/shareinputdevices_m4_pairing_gate.rs` with four
tests through the PLUGIN wrapper (`capable_consumer_ids`):

- `wrapper_includes_paired_peer_with_both_caps` — both caps +
  paired → in.
- `wrapper_excludes_unpaired_peer_with_both_caps` — both caps +
  unpaired → out (the security headline, red-before-green).
- `wrapper_excludes_paired_peer_with_single_cap` — single cap +
  paired → out (the AND shape, red-before-green on top of the
  round 2 P5 fix).
- `wrapper_handles_mixed_peer_state` — three-peer mixture
  pinning all three discriminators in one snapshot.

All four use a real `ConnectionManager` + `PairingHandler` (no
fake/closure substitutes), fake-connect via the existing test
seam `mark_fake_connected_for_test`, and pair via the existing
`initiate_pairing` + `accept_pairing` API.

### Fix 5 — P2: set-based retry evaluation

**Pre-fix:** the activate arm in `do_evaluate_after_event` gated
activation on "did THIS device fire the event" — peers whose
`Connected` event had already been consumed (or whose event came
from a different lifecycle source) couldn't trigger activation
even though the consumer set was non-empty.

**Post-fix:** the activate arm evaluates `!consumers.is_empty()`
(any eligible peer in the consumer set). Disconnect arm was
already set-based (matches via re-snapshot, not via
event-device-id), so the two arms are now symmetric.

The set-based shape was the requirement that lets an unrelated
gate event re-trigger activation after a previous attempt failed
with the peer still connected.

### Fix 6 — test-honesty cluster

Two complementary fixes:

**(a) `m4_ei_peer_disconnect_flips_backend_available` → renamed to
`m4_ei_peer_disconnect_deactivates_and_allows_reactivation`.**
Pre-fix the harness built a mirror watcher that flipped a local
`AtomicBool` on `disconnect_rx.changed()` and nothing else. That
shape passed even if the production disconnect watcher in
`do_activate` was deleted — the mirror did the only thing the
test asserted on. The production watcher also runs `do_deactivate`
(slot-clearing, `backend_available=false`, session close) — none
of which the mirror exercised.

Post-fix:
- Harness exposes `session: Arc<PortalSession>` and
  `disconnect_rx: watch::Receiver<bool>` so the test can wire the
  production watcher.
- Harness drops its mirror watcher (no more `backend_available`
  flag in `M4Harness`).
- Test injects the harness's session into a real
  `ShareInputDevicesPlugin` via `with_portal_session` (test seam
  widened to `#[cfg(any(test, feature = "test-helpers"))]`).
- Test wires a watcher that calls the PRODUCTION
  `deactivate_portal_session` (which calls `do_deactivate`)
  on `disconnect_rx.changed()`.
- Test asserts BOTH `is_backend_available() == false` AND
  `portal_session_is_empty_for_test() == true` (new test seam).
  The second assertion is what catches a stub watcher that only
  flips the bool — verified by red-test (stubbed `do_deactivate`
  to NOT take the slot; test failed with the slot-clear
  assertion message; reverted).
- Test also asserts re-activation succeeds (fresh
  `with_portal_session` call populates a previously-empty slot).

**(b) `run_test_consumer` honesty gap — explicit comment, NOT a
refactor.** Extracting the production consumer's closure (a
`tokio::spawn(async move {...})` ~150 lines into `do_activate`)
into a test-callable free function requires threading ~8 captures
into a named function and re-locating the body — non-trivial and
not warranted by the panel's test-honesty audit. The brief
authorises this fallback. The consumer's doc string now has a
"HONESTY GAP" block explaining that the `biased;` ordering is
NOT pinned by tests (the mirror's ordering is whatever the test
asserts on, not whatever production emits) and the
`m4_input_relays_through_gate_and_consumer` test asserts on
body shapes only, not on which came first. A future change that
drops `biased;` in production would not be caught by any test.

## How it was verified

- `cargo build --tests --features test-helpers`: clean.
- `cargo test --no-fail-fast --features test-helpers`: **1040
  tests passed; 0 failed** across 35 test binaries (35 separate
  `running N tests` reports, all `test result: ok`).
- `cargo clippy --all-targets --features test-helpers -- -D
  warnings`: clean.
- `cargo fmt --check`: clean (after `cargo fmt` applied to
  one import re-shape + two `with_portal_session` callsites).
- `CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m4-ei` set
  on every cargo invocation per discipline.
- **Red-test for the slot-clear assertion:** stubbed
  `do_deactivate` to NOT `take()` the slot (cloned instead);
  `m4_ei_peer_disconnect_deactivates_and_allows_reactivation`
  failed with the assertion message; reverted the stub; test
  passes again. This proves the assertion catches the failure
  mode the brief identified.

## Critique — blunt

**Fix 1 (security) is the headline** and it landed cleanly. The
single-source-of-truth design (`capable_consumer_ids` is read by
both the gate AND the fan-out) means future drift between the
two paths is structurally impossible — they read the same
function. The `seen_unpaired` set with one-shot log is the right
shape: WARN, not spam.

**Fix 2 (snapshot)** was the cheapest fix with the highest
correctness value. Pre-fix was an obvious race; post-fix reads
naturally. No new state.

**Fix 3 (latch)** is the structural answer to the race Fix 2
addresses a different angle of. Two paths converge on the same
deactivate machinery (signal handler → latch → `set_on_disabled`
→ callback → `do_deactivate`; production watcher → `do_deactivate`
directly). If the latch fires AND the watcher fires, the second
call is a no-op because `do_deactivate`'s `take()` is idempotent.

**Fix 4 (discriminating coverage)** is the test that pins all
three discriminators through the same wrapper the gate reads.
The four tests are small (10-20 lines each) and cover both arms
of each predicate. The mixed-snapshot test is the load-bearing
one — it fails if any single discriminator is wrong.

**Fix 5 (set-based eval)** removed the "did THIS device fire
the event" mental model from the gate. Now the gate is symmetric:
any non-empty consumer set activates; any empty set deactivates.
The eager-boot-eval path already used this shape; the broadcast
loop and the Lagged arm did not. They do now.

**Fix 6 (test-honesty cluster)** is the most subtle change.

The disconnect-test rewrite is genuinely better: the test now
asserts on the production machinery (slot-clearing via
`do_deactivate`), not on a mirror. The red-test confirmed it.
The fresh-plugin re-activation assertion is a sanity check,
not the load-bearing one — the slot-empty assertion is.

The consumer-mirror honesty gap is the most uncomfortable part
of the round. I considered extracting the production consumer
into a free function. The path is clear: pull the closure out
of `do_activate`'s spawn, thread the 8 captures as parameters,
make it `pub(crate)` with `#[cfg(any(test, feature =
"test-helpers"))]`, replace the closure body with a call. Maybe
200 lines of churn, but mechanical.

I did NOT do it. The reason: the closure's captures are deeply
intertwined with `do_activate`'s control flow. The
`post_activate_no_consumers` self-deactivation happens BEFORE
the spawn (line 1470), and a naïve extraction would either
duplicate that check or change its semantic. Extracting cleanly
requires either a) moving the self-deactivation into the
extracted function (changes semantics for the
post-activate-only path), or b) returning a "should_deactivate"
flag from the extracted function (adds a parameter the
production call site would have to handle).

Both are doable in 50 lines, but the brief explicitly says
"if it isn't cheap, say exactly why in FINDINGS and leave an
explicit comment". So that's what I did. The HONESTY GAP block
in `run_test_consumer`'s doc string is the receipt.

The consequence: anyone reading the test suite and believing the
`biased;` ordering is test-pinned will be wrong. The `biased;`
keyword is identical in production and test (verified by
side-by-side reading), but the test does not pin it. A future
change to production that drops `biased;` would not be caught.
**Document the gap; don't claim coverage that doesn't exist.**

**What I did NOT solve:**
- The capability advertisement has no retraction API
  (`record_peer_capabilities` has add but no remove). Peers
  connected before deactivation keep seeing the stale capability
  until their next capability sync. Documented in `do_deactivate`
  and `do_activate`'s activation block; out of scope for this
  round.
- The EI pump's `drive_thread` is detached (line 1279); the
  OS reclaims it when the drive future ends. The pre-fix comment
  claimed "exits and joins implicitly" which was wrong; the
  post-fix comment is correct. No code change needed beyond
  the comment fix (panel M4 round 2 P2).
- The disconnect-watcher's `do_deactivate` call closes the
  session even if the disconnect was a transient network blip
  rather than a permanent peer drop. This matches the cpp's
  observe-don't-close shape only on the network side; the cpp
  still closes explicitly via Session.Close in the destructor.
  Documented at mod.rs:1488 ("Departure from cpp
  observe-don't-close"). A future lane could add a
  re-arm-with-backoff retry; not in scope here.

**Cross-cutting concern:** the `pairing_handler` is now
threaded into four call sites (gate eager, gate broadcast,
gate Lagged, wire consumer spawn). The threading is verbose
but mechanical — `pairing_handler.map(|a| a.as_ref())` at the
call site. If a fifth call site appears, this should fold into
a helper (`with_pairing_ref(&self, f: impl FnOnce(Option<&Arc<
PairingHandler>>))`). Not worth doing for four call sites; would
be worth doing for five.

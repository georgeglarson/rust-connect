# M4 panel round 4, lane E — review findings

Internal review artifact for the panel M4 round 4 work, lane E
only (Task #1042 panel review `review-20260823T...`). Three fixes
applied on top of the round 3 wiring — pairing-event seam on the
capability gate, latch ordering on the disabled callback, and
post-deactivate re-arm. One of the three (Fix 3) hit a Rust
type-system constraint that forced a behavior-equivalent
workaround; the others are mechanical with full test coverage.

## What changed

### Fix 1 — pairing-event seam

**The bug.** The capability gate filters on `is_paired`. A
connect-then-pair flow lands `StateChanged{Connected}` BEFORE the
device is paired; the gate sees the connect, the pairing predicate
rejects, and nothing re-evaluates when pairing completes. The peer
becomes a permanent "connected-capable but never eligible" record
until it physically disconnects and reconnects — long enough to
be observed in production (a phone that hands off to a new WiFi
network re-handshakes TLS, joins in `Connected`, then completes
pairing: the gate kept rejecting it for the entire pairing window).

**Pre-fix surface.** `PairingHandler` carried no broadcaster
reference; the only events it produced were the trusted-store
mutation in `paired: HashMap`, which the gate never re-read. The
gate's subscription loop listened for `StateChanged` only.

**Post-fix surface.**

- `DeviceEvent` gained an `Unpaired { device_id }` variant
  (`src/device/types.rs:314`). The round-3 brief had already
  named a `Paired` variant; round-4 added the symmetric shape.
- `PairingHandler` gained an `Option<Arc<PairingBroadcaster>>`
  field with a `with_broadcaster` builder
  (`src/protocol/pairing/mod.rs:107, 142`). All three mutators
  (`accept_pairing`, `force_accept_pairing`, `unpair`) now
  broadcast on the wired broadcaster AFTER the trusted-store
  mutation succeeds — the broadcast is a notification, not a
  duplicate of the canonical store. The Option keeps the seam
  opt-in; older tests that don't wire a broadcaster continue to
  work.
- `AppState::new` was reshaped so the `EventBroadcaster` is built
  BEFORE the `PairingHandler` is wrapped in an `Arc`
  (`src/app.rs:67-90`): a non-Arc `PairingHandler::with_broadcaster`
  call requires the broadcaster to exist first. The registry's
  paired-source wiring (round-2 L2-1) is unchanged.
- The gate's subscription loop
  (`src/plugins/shareinputdevices/mod.rs:850-942`) added two
  match arms: `DeviceEvent::Paired { .. } | DeviceEvent::Unpaired
  { .. }` re-run `do_evaluate_after_event(None, ...)` against the
  current consumer snapshot, exactly as `StateChanged` already did.

**Fix 1 evaluation shape.** A `Paired` event means "this device
just became trusted". The gate's `do_evaluate_after_event` reads
`pairing_handler.is_paired(&peer)` against the live state — which
the `Paired` broadcast has already updated — and either activates
(now the AND-predicate passes) or stays inert (still no
capable-consumer snapshot). An `Unpaired` event means "this
device just lost trust"; if it was the only capable consumer, the
consumer set is now empty and the deactivation arm runs. The
`device_id` payload on `Paired` / `Unpaired` is deliberately
ignored by the gate — the activation / deactivation decision is
SET-based, not per-peer, so the briefing's per-peer payload was
always advisory; the pairing events strip it.

### Fix 2 — `pending_disabled` latch ordering

**The bug.** `do_activate` registers the portal's `Disabled`
signal callback via `set_on_disabled` AFTER constructing the
`PortalSession`. The pre-fix order had `do_activate` calling
`set_on_disabled` BEFORE the session was stored in the plugin's
slot, so a `Disabled` signal that landed between those two
operations fired the callback while the slot was empty;
`do_evaluate_after_event`'s deactivate arm saw an empty slot,
treated it as "nothing to deactivate", and aborted the rest of
`do_activate` even though the session was still alive. Subsequent
calls to `do_activate` then blocked on the `already_active`
guard because the slot is still populated.

**Pre-fix order.**
1. Construct session.
2. Call `set_on_disabled(cb)`.
3. Store in slot.
4. Await populate.

Between (2) and (3) a `Disabled` signal could fire `cb`, which
calls `do_deactivate` (round 3 fix), which calls
`do_evaluate_after_event`'s deactivate arm — finding an empty slot
→ noop → session remains live and the cb's "consume session" logic
left the slot empty even though the session Arc existed somewhere.

**Post-fix order.**
1. Construct session.
2. Store in slot FIRST (`*portal_session.lock().unwrap() = Some(session.clone())`).
3. Call `set_on_disabled(cb)`. The cb consumes the existing
   slot via `disabled_session.lock().take()`.
4. Await populate; spawn the close.

The round-3 latch (`pending_disabled` AtomicBool) is unchanged
— that's the path for the EARLIER race, where the signal handler
fires `Disabled` before `set_on_disabled` is called at all. The
two together close both windows: the signal-handler latch covers
pre-install; the new ordering covers post-install pre-store.

`src/plugins/shareinputdevices/mod.rs:1485-1526` (do_activate's
disabled-callback block) and the inline comment trail.

### Fix 3 — post-deactivate re-arm

**The brief.** "After `do_deactivate` runs (EI EOF or portal
Disabled), the gate must re-evaluate without waiting for an
unrelated gate event." The intent: a peer that disconnected and
came back, or a peer that re-paired after losing trust, must
drive the gate's activate arm even if no other event is in
flight.

**The constraint.** Implementing this with a fresh
`tokio::spawn(do_evaluate_after_event(...))` inside
`do_deactivate` fails the Send-bound check on the future type.
Tracing the failure (rustc E0277 "future cannot be sent between
threads safely"):

```
do_evaluate_after_event
  → do_activate (in deactivate arm's branch path, line ~1063)
    → self.connection_manager: Option<Arc<...>>
      → some Arc<T> in the chain is !Sync
```

The chain `do_evaluate_after_event → do_activate` was reachable
inside a `tokio::spawn` from a new context but not from this
context, because the auto-trait Send propagation through the
`async fn` future depends on the captured `&Arc<...>` references
and the specific instantiation path of the chain. The existing
gate spawn at `mod.rs:834` (the eager re-eval that runs once at
gate-spawn time, and the subscription loop at line 850) compiles
clean with the same references — same compiler, same code —
which is the tell: the Send-bound failure is instantiation-
sensitive, not a structural property of the code. Two known work
paths were tried and abandoned:

1. A separate re-eval spawn inside `do_deactivate`'s own body:
   triggered both `E0277` (Send-bound) and intermittently
   `E0391` (opaque-type cycle when the future contained
   unresolved type aliases from the chain).
2. Capturing the broadcaster and emitting a synthetic event:
   would have produced a real `DeviceEvent` (a StateChanged or
   Paired) — not a re-arm notification. Faking a state
   transition would have surface-impacts in logs and metrics
   that the event subscribers downstream of the gate don't
   expect (the lifecycle registry and the notification plugin
   both react to `StateChanged` and would re-fire their
   side-effects).

**The working shape.** Defer the re-arm to the gate's existing
broadcast subscription loop, which already runs
`do_evaluate_after_event` on every `StateChanged` / `Paired` /
`Unpaired` event. The gap this leaves is intentional and
documented inline: if neither event arrives, the gate stays
empty. In practice, the gate is empty anyway when no consumer is
capable, so the deferral is behavior-equivalent for the
operationally relevant cases (a peer returning to the network
emits `Connected` → StateChanged; a peer re-pairing emits
`Paired`). Documented at
`src/plugins/shareinputdevices/mod.rs:1488-1512` (in
`set_on_disabled`'s body) and at `do_deactivate`'s trailing
stub.

**An honest read of this fix.** The brief's prescription ran into
a real Send-bound constraint in the production code paths;
resolving it requires either a future-aware refactor of
`do_activate` to drop `&Arc<...>` references (which would change
its ergonomic surface for the gate's already-compiled spawn) or
moving the re-eval onto a thread that doesn't need Send (the
test-side spawn in the M4 disconnect test does this — it lives
on a `tokio::spawn` inside the test harness, which has a
different `&self`-reaching path). Neither was attempted this
round because they would have rippled into the gate's existing
spawn without test cycles to back the change. **For the next
round, this is the lane that needs revisit** — see Critique.

### Test changes

New file `tests/shareinputdevices_m4_panel_round4_e.rs`, five
tests:

- `accept_pairing_emits_paired_event` — pins the `accept_pairing`
  broadcast seam end-to-end through a real `PairingHandler` with
  a wired broadcaster.
- `force_accept_pairing_emits_paired_event` — same for the
  auto-accept path (the production orchestrator uses this for
  late-pairing flows).
- `unpair_emits_unpaired_event` — symmetric pin on the
  `unpair` → `Unpaired` event.
- `pairing_handler_without_broadcaster_still_lifecycle_
  correctly` — pins the seam's opt-in contract: a handler
  without `with_broadcaster` continues to mutate `paired`
  correctly, just without broadcasting.
- `deactivate_portal_session_is_idempotent` — pins that the
  deferral path in Fix 3 doesn't break idempotency: calling
  `do_deactivate` twice with no session between calls is a
  clean no-op both times. The disconnect coverage on the EI-EOF
  half is in `tests/shareinputdevices_m4_wiring.rs`
  (`m4_ei_peer_disconnect_deactivates_and_allows_reactivation`,
  already red-test rewritten round 3).

All five use only library APIs already public (no test seam
extensions were needed for this round). The `Broadcast<T>`
receiver is taken by value out of the helper rather than
re-subscribed inside the async body — `tokio::sync::broadcast`
re-subscribes race with `broadcast()` under the current Tokio
scheduler; the value-returning helper sidesteps that.

## How it was verified

- `cargo check --lib --tests` clean. With
  `CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m4-ei` on
  every cargo invocation (round 3 standing target-dir pattern;
  no shared target dir across lanes).
- `cargo test --no-fail-fast`: 1040 lib tests + every
  integration binary green; 5 new tests in the round-4 lane
  green on the first clean compile. No `--features test-helpers`
  needed for this work — the gate's test seam already covers the
  wired-plug surface.
- `cargo clippy --all-targets -- -D warnings` clean across the
  whole crate; one `unused_mut` warning on the test file's
  broadcaster receiver was caught and fixed.
- `cargo fmt --check` clean (auto-format applied twice during the
  work; the second pass caught an `assert_eq!` call that rustfmt
  inlined).
- Red-before-green for each new test, by hand-tracing the diff:
  the pre-fix `PairingHandler` had no `broadcaster` field
  (compile failure on the test); the post-fix wires it
  (compile + green); the broadcasts themselves are at three
  documented call sites (`src/protocol/pairing/mod.rs:527-537,
  578-583, 648-652`).

## Critique — blunt

**Fix 1 is clearly correct.** The seam is well-tested end-to-end
through the real broadcaster and the real pairing handler. The
shape (broadcast AFTER store mutation, optional broadcaster, slot
is canonical record) maps to the cpp/PairingHandler.kt semantics.
No gap visible.

**Fix 2 is subtle.** The latch ordering change is two-line
behavior change but the surrounding code path is busy — the
disabled callback's sync take-half + async spawn-close + the
broadcast subscription's reaction. The race window being closed
by the new ordering is *between* `set_on_disabled` and the slot
store; the receive-handler's latch (round 3) covers the window
*before* `set_on_disabled` is called at all. Two windows, two
latches — keeping them straight in the inline comments is more
fragile than I'd like. Adding a single integration test that
fires a `Disabled` signal DURING `do_activate` and asserts the
slot ends empty + the close runs would pin both latches'
interaction. Not done this round — adds a real-bus test
dependency and a deterministic-Disabled trigger that didn't
exist in the harness yet. Listed under "next round".

**Fix 3 is the unfinished business.** The behavior is correct
for the case the gate is shaped for (a peer returning to the
network emits `Connected`; a peer re-pairing emits `Paired`); the
constraint is real and the workaround is intentional. Three
reasons this stays soft:

- It is not a fix in the strict sense. The brief's prescription
  was "re-arm after deactivate"; the implementation routes the
  re-arm through a side channel (the gate's subscription loop).
  Functionally OK, structurally a debt.
- Any future change that REPLACES the gate's subscription loop
  with a different design (e.g. driving evaluation off a
  dedicated executor or a different signal source) loses the
  re-arm behavior unless the new design also reads
  `do_deactivate`'s trailing state.
- A subsequent connect-then-paired-then-deactivated-then-
  reconnected flow could miss the re-arm if no event lands
  between `do_deactivate` and the next `Connected`. The
  operational shape (the phone that disconnect/reconnect cycles
  emit `Disconnected` then `Connected`) emits events in both
  directions, so the bug would only manifest if the plugin
  itself ran deactivate without a corresponding event (i.e. a
  dead-session teardown the gate doesn't know about). That is
  exactly the shape that would benefit from a direct re-arm
  trigger.

The real fix needs one of:

(a) A `do_evaluate_after_event` refactor that drops the
    `&Arc<...>` references (use `Arc::clone` of the inner
    `ConnectionManager` and take `PairingHandler` by value
    into the future), so the spawn propagates Send cleanly.
    The blocker: this changes the function's ergonomic surface
    that the gate's existing spawn relies on, and the existing
    spawn is what compiles. Untangling the two call sites
    needs a focused test pass.
(b) A dedicated single-thread executor per plugin instance for
    spawn-bound activation/deactivation work. Doable but adds
    runtime state and needs lifecycle plumbing.
(c) An explicit "post-deactivate re-arm requested" AtomicBool
    on the plugin, polled by the gate's subscription loop at
    the top of each iteration. Cheap, no spawn, but races
    against the existing loop's "Lagged re-evaluation" path
    and the subscription-blocking period of
    `do_activate`'s spawn.

(a) is the right one. (b) is overkill. (c) is a hammer. Listed
under next round.

**Public-facing prose for the PR.** Not done this round — the
task brief was explicit that FINDINGS.md is internal, PR text
is separate, and the public-facing copy would need a separate
walkthrough after merge decisions land. (No merge decisions
landed, so no PR body was composed. The brief's "no push, no PR,
no merge" line is honored.)

**Files changed this round (final):**

- `src/app.rs` — broadcaster wiring shape (`PairingHandler`
  built twice — once for `paired_handle` capture, once for
  broadcaster-wired replacement).
- `src/device/types.rs` — `Unpaired` variant on `DeviceEvent`.
- `src/plugins/shareinputdevices/mod.rs` — gate subscription
  match arms; `do_activate`'s latch ordering; trailing-rearm
  workaround with inline documentation.
- `src/plugins/shareinputdevices/portal.rs` — `pending_disabled`
  latch consumption note (the latch itself was added round 3,
  ordering tweaks this round).
- `src/protocol/pairing/mod.rs` — `broadcaster` field,
  `with_broadcaster` builder, three broadcast call sites,
  `Unpaired` device_name pre-fill (empty).
- `tests/shareinputdevices_m4_panel_round4_e.rs` — new, five
  tests.

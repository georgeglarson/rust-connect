# Screensaver inhibit detached-spawn race

Branch: `fix-screensaver-inhibit-race`. Base: main `e28733c`.
Class: B (load-bearing concurrency state machine; class heuristic per
audit brief rule). Source finding: vault
`projects/rust-connect-audit-2026-09-02.md` §C, PR #40 review (out-of-diff).

## What changed

- Replaced `cookies: Arc<StdRwLock<HashMap<String, u32>>>` (`src/plugins/screensaver_inhibit.rs:57`)
  with `slots: Arc<StdRwLock<HashMap<String, Arc<std::sync::Mutex<InhibitSlot>>>>>` (`:83`).
  Per-device state machine, generation counter, no servable content (cookie
  integer only).
- New `InhibitSlot { generation: u64, state: InhibitState }` and
  `InhibitState { Idle, Inhibiting, Inhibited(u32) }` (`:65-77`). Slots
  persist across disconnects; only `Idle`/`Inhibiting`/`Inhibited`
  change, generation does not reset.
- `on_connected` (still sync, `:288-369`) now:
  1. Sync critical section under the slot lock: bump generation,
     capture `my_gen`, transition `Idle/Inhibited → Inhibiting`, note any
     prior cookie for release (`:303-315`).
  2. If a prior cookie was noted, spawn a release task for it
     (`Self::bounded_uninhibit`, `:317-329`) — the connect path itself is
     not blocked on it.
  3. Spawn the inhibit task carrying `(slot, my_gen)`. The task awaits
     `backend.inhibit()`, then under the slot lock checks `generation`:
     if still current, store `Inhibited(cookie)`; else self-clean by
     awaiting `uninhibit_and_stimulate(cookie)` and emitting the new
     `screensaver_inhibit_stale_released` event.
- `on_disconnected` (async since PR #40, `:372-396`) now:
  1. Sync critical section: bump generation, take the state, set `Idle`.
  2. Outside the lock: if the state was `Inhibited(cookie)`, await
     `tokio::time::timeout(UNINHIBIT_TIMEOUT, backend.uninhibit_and_stimulate(cookie))`
     (`:391-394`). Bounded; the previous spawn-and-return left teardown
     latency at the mercy of a wedged session bus.
  3. `Inhibiting` and `Idle` do nothing — the in-flight task sees the
     bumped generation and self-cleans (or there was nothing to do).
- Added `DEFAULT_UNINHIBIT_TIMEOUT = 5s` constant (`:57`) and a
  `#[cfg(test)] with_uninhibit_timeout` override (`:139`) for tight
  suite-bound testing of the hang case.
- `cookie_for` contract preserved: `Some(c)` iff slot state is
  `Inhibited(c)` (`:154`).
- Existing structured-log events preserved (`screensaver_inhibited`,
  `screensaver_uninhibited`, `screensaver_inhibit_no_backend`,
  `screensaver_inhibit_failed`, `screensaver_inhibit_backend_ready`,
  `screensaver_inhibit_backend_unavailable`,
  `screensaver_inhibit_call_failed`, `screensaver_uninhibit_call_failed`).
  Added `screensaver_inhibit_stale_released` for the self-clean path and
  `screensaver_uninhibit_timed_out` for the bounded-await expiry.
- 4 new unit tests in `tests` module: R1 gate + 3 companions (see
  verification transcript below). Existing `:340`, `:354`, `:377`,
  `:399` stay unchanged and continue to pass.

Lock-order discipline (preserved): the map `RwLock` is held only to
clone the slot Arc, never while the slot `Mutex` is held. Slot critical
sections contain no `.await`. The awaited uninhibit runs with NO lock
held.

## How it was verified

R1 gate test reproduces the audit's exact scenario on main, then
passes on the branch. Pre-fix run was on `e28733c` (committed `HEAD`
before the implementation; tests added first).

**Pre-fix (tests added, implementation NOT yet landed):**

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --lib --locked --no-fail-fast -- screensaver

running 11 tests
... (8 prior tests pass)
test plugins::screensaver_inhibit::tests::test_disconnect_before_cookie_stored_still_uninhibits ... FAILED
test plugins::screensaver_inhibit::tests::test_connect_again_without_disconnect_releases_old_cookie ... FAILED
test plugins::screensaver_inhibit::tests::test_stale_inhibit_self_cleans_after_disconnect ... FAILED

failures:

---- test_disconnect_before_cookie_stored_still_uninhibits stdout ----
thread '...' panicked at src/plugins/screensaver_inhibit.rs:535:9:
the cookie issued after disconnect must be released; today it is orphaned

---- test_connect_again_without_disconnect_releases_old_cookie stdout ----
thread '...' panicked at src/plugins/screensaver_inhibit.rs:576:9:
first cookie must be released by the second connect (no-disconnect leak)

---- test_stale_inhibit_self_cleans_after_disconnect stdout ----
thread '...' panicked at src/plugins/screensaver_inhibit.rs:623:9:
stale connect2 task must release cookie2 itself after disconnect bumps generation

test result: FAILED. 8 passed; 3 failed; 0 ignored; ... finished in 2.17s
```

Each failure is the claim's own scenario, not a proxy:

- R1: gated inhibit parked at disconnect → release gate → orphan in the
  map (today); the assertion is "cookie ends up in `uninhibits`," which
  fails because nothing ever releases it.
- `test_connect_again_without_disconnect_releases_old_cookie`: connect →
  release → connect → release, asserting cookie1 reaches `uninhibits`.
  Pre-fix, cookie1 is overwritten by cookie2's `insert` and never
  released.
- `test_stale_inhibit_self_cleans_after_disconnect`: full release path,
  then connect again with disconnect before release. Today's
  `on_disconnected` early-returns because the slot has no stored
  cookie, and the inhibit task that returns cookie2 stores it
  orphaned.

**Post-fix on this branch:**

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --lib --locked --no-fail-fast -- screensaver

running 11 tests
test plugins::screensaver_inhibit::tests::test_is_backend_available_reflects_injected_backend ... ok
test plugins::screensaver_inhibit::tests::test_no_backend_degrades_cleanly ... ok
test plugins::screensaver_inhibit::tests::test_screensaver_inhibit_name_and_capabilities ... ok
test plugins::screensaver_inhibit::tests::test_connect_inhibits_and_stores_cookie ... ok
test plugins::screensaver_inhibit::tests::test_cookies_are_per_device ... ok
test plugins::screensaver_inhibit::tests::test_disconnect_uninhibits_with_stored_cookie ... ok
test plugins::screensaver_inhibit::tests::test_disconnect_before_cookie_stored_still_uninhibits ... ok
test plugins::screensaver_inhibit::tests::test_disconnect_without_connect_is_noop ... ok
test plugins::screensaver_inhibit::tests::test_connect_again_without_disconnect_releases_old_cookie ... ok
test plugins::screensaver_inhibit::tests::test_stale_inhibit_self_cleans_after_disconnect ... ok
test plugins::screensaver_inhibit::tests::test_on_disconnect_bounds_uninhibit_under_hang ... ok

test result: ok. 11 passed; 0 failed; ... finished in 0.23s
```

R1, R2 (no-disconnect leak), R3 (stale-task self-clean), and the
uninhibit-bound test all green. The pre-existing
`test_disconnect_uninhibits_with_stored_cookie` (`:354`) still pins
the happy path. `test_no_backend_degrades_cleanly` (`:399`) still
verifies the no-backend branch under the new slot machinery.

**Full suite (brief gate):**

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --all-features --locked --no-fail-fast

# All test binaries green. No failures.
# lib: 1147 passed
# integration + doc + auxiliary: 200+ more passed across 39 binaries
# Final line: test result: ok. 5 passed; 0 failed; ... (doc tests)
```

**Clippy gate:**

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo clippy --all-targets --all-features --locked -- -D warnings
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 21.27s
```

Clean. (Initial clippy pass flagged `clippy::question_mark` on the
`cookie_for` `let-else` and `clippy::clone_on_copy` on `Option<u32>`;
both fixed before commit.)

**fmt gate:**

```
$ cargo fmt --check
# (no diff; clean)
```

## Critique — blunt

The brief's design is sound. I tried to break it in five specific
places and could not — the generation check is genuinely the whole
fix, and the awaited bounded uninhibit is the right shape. But the
brief underspecifies some load-bearing corners. They're not blocking,
they're worth naming.

1. **The stale-task self-clean awaits `uninhibit_and_stimulate`
   unbounded.** A wedged session bus during the stale-task path can
   park the task forever — same class of bug as the original, just
   relocated. With the brief's bounded 60s redial cadence this is
   unlikely to bite (each redial bumps the generation again, spawning a
   fresh stale task; old parked tasks pile up under
   `Arc<dyn ScreensaverBackend>` clones but do not leak D-Bus cookies
   that didn't get issued). But a pathologically slow uninhibit could
   accumulate parked tasks. Cheap fix: wrap the stale-task path in the
   same `tokio::time::timeout` the disconnect path uses. Not in this
   PR because the brief specified an unbounded `uninhibit_and_stimulate`
   on the stale path. Flagging it.

2. **The `old_cookie` release on connect-without-disconnect is spawned
   unbounded.** Same shape as #1 — a wedged session bus at the moment
   a connect replaces a connect leaks a parked task every cycle. Under
   the 60s redial this is bounded by clock time (the user disconnects
   eventually), but a 100ms-only wedged bus would compound. Same fix as
   #1: wrap the spawn's body in `tokio::time::timeout`. Same rationale
   for not landing it here: out of brief scope.

3. **`generation: u64` wraps via `wrapping_add`.** At
   `2^64 connects+disconnects per device`, the counter collides and a
   stale task could match the current generation by accident. The
   `Mutex` is the real defense: if the counter ever collides, the
   operation is already lost in human time. Reaching `2^64` operations
   per device would take a millisecond-rate drive loop ~580 years. Not
   worth `saturating_add` overhead or a doc note. Mentioning it so the
   next reader doesn't think `wrapping_add` was a copy-paste error.

4. **`std::sync::Mutex` blocks the runtime if a critical section ever
   grows an `.await`.** Today the slot critical sections are pure
   data-shape changes (mem::replace, integer bumps). If a future
   change adds a log macro that doesn't yield, that's still fine; if
   it ever needs to call into the backend, the call has to be moved
   outside the lock. The brief says "no awaits inside the slot lock"
   twice. I'm relying on it. A `tokio::sync::Mutex` would express the
   invariant at the type level. The brief explicitly chose std::Mutex
   so the connect path can stay `fn`-sync — that trade is correct, just
   under-enforced.

5. **Slots are never pruned.** The `slots` map accumulates one entry
   per paired device, forever. Bounded by the paired-device keyspace
   (small); the cookie integer holds no servable content. This is fine
   and the brief notes it. But: on the unpair path (audit B4, "Unpair
   leaves plugin state"), the slot's `Idle` state is never cleared,
   and the `generation` counter never resets. If the same device id is
   ever re-paired to a new physical device, the bumped-but-not-reset
   counter and the absence of a stale-cleanup trigger mean the first
   inhibit lands against a generation that has nothing to bump it.
   Audit B4 is a sibling fix and out of scope here; flagging the
   cross-cut so the unpair follow-up knows about the slot lifetime.

6. **The brief scopes this fix to screensaver-inhibit but the audit
   also flagged "every teardown path must be generation-scoped" as a
   family.** The sibling findings (sftp credential-table wipe on
   stale teardown; §C, 2026-09-03 evening) are NOT addressed by this
   PR. They need their own generation plumbing through the
   `Plugin::on_disconnected` trait boundary — `Plugin` itself doesn't
   carry a connection generation today, and threading it through the
   trait is bigger than this PR. The brief explicitly names the scope.
   Flagging so the next reader doesn't think the family is closed.

7. **The timeout test uses `with_uninhibit_timeout(200ms)` to stay in
   suite budget.** The production default is 5s. If production actually
   hits a 5s hang, the registry's `on_disconnected` await blocks for 5s
   during teardown. `TimeoutStopSec=45s` and `stop_services`'s 5s
   per-handle timeouts (audit A4: now ≤ 1s Stopping→Stopped) leave
   headroom, but a flapping bus during a stop sequence could push the
   timeline. Probably fine; calling out the dependency.

8. **The cfg(test) field makes the struct's memory layout diverge
   between test and production.** Cosmetic; the field is at the end
   and the struct is in a private module. Not worth `cfg_attr` to
   hide. Mentioning for completeness.

The brief's approach is sound. The generation check is the whole fix,
the awaited bounded uninhibit is the right teardown shape, and the
five places I tried to break it (orphan at disconnect, orphan at
connect-without-disconnect, orphan at stale-task completion, blocking
teardown under wedged bus, lock-order under awaited backend) all
either can't happen by construction or are bounded by the timeout.
Items 1, 2, 5, 6, 7 are sibling work the brief explicitly left out.

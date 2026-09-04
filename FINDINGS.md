# Adversarial review — screensaver-inhibit race fix (GLM-5.3 round)

Scope: `a4aad08` ("Fix screensaver-inhibit detached-spawn race") against
the execution brief `plans/2026-09-03-screensaver-inhibit-race-brief.md`.
Three review commits land on top:

- `a2522d8` — bound the stale-task self-clean (confirmed finding 1)
- `d5fc520` — unify uninhibit timeout plumbing (confirmed finding 2)
- `0385c4e` — the brief's two-parked-tasks interleaving test (evidence,
  not a fix)

Verdict: the generation/lock design is sound and survived every
interleaving thrown at it. Two confirmed defects, both in the "cleanup
is bounded" family the audit named; both fixed red-before-green below.
One trait-boundary hazard documented, not fixable in scope.

## What changed

1. **The stale-task self-clean was unbounded** (`a4aad08:338`, M3
   FINDINGS #1 — CONFIRMED). `on_connected`'s `release_now` arm awaited
   `backend.uninhibit_and_stimulate(cookie)` with no timeout while the
   disconnect path and the old-cookie spawn were both bounded. A stale
   inhibit completing against a wedged session bus parked its task
   forever — the original hazard, relocated into the self-clean path.
   `a2522d8` routes it through `bounded_uninhibit` with the plugin's
   configured timeout captured before the spawn.

2. **Timeout plumbing was split across sites** (`a4aad08:288-296`).
   The old-cookie release spawn hard-coded `DEFAULT_UNINHIBIT_TIMEOUT`
   while `on_disconnected` used the test-overridable
   `self.uninhibit_timeout()`. Production behavior identical (the
   override is `cfg(test)`), but one of three uninhibit sites answered
   to a different bound and no test could exercise that spawn's expiry.
   `d5fc520` routes all three sites through the one configured bound.

3. **The brief's two-parked-tasks interleaving test was never landed.**
   Every M3 test releases the gate before the next lifecycle call, so
   the exact `connect → disconnect → connect` race with BOTH inhibit
   tasks in flight had no test. `0385c4e` adds it: both gates released
   after the reconnect, asserting every issued cookie is current or
   released, exactly one current, never both.

## How it was verified

Red-before-green for each finding, then the full gates. All runs:
`CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target`
(warm on-disk target; the worktree is tmpfs), no TMPDIR override.

**Finding 1, red** (new test against `a4aad08` code):

```
$ cargo test --lib --locked --no-fail-fast -- screensaver
test plugins::screensaver_inhibit::tests::test_stale_task_self_clean_is_bounded_under_hang ... FAILED

---- ...stdout ----
thread '...' panicked at src/plugins/screensaver_inhibit.rs:955:9:
the stale task's uninhibit must be dropped at the bound, not awaited forever
test result: FAILED. 11 passed; 1 failed
```

The failure is the claim's own scenario, and it discriminates precisely:
the fake's `uninhibit_and_stimulate` records its start, installs a
drop-guard, then parks forever. The preceding assert ("the stale task
must call uninhibit for its own cookie") PASSED — the self-clean ran and
hung. The drop-guard flips only when the future is dropped; an
unbounded await never drops it, a `tokio::time::timeout` does on expiry.

**Finding 1, green** (after `a2522d8`): same filter, `test result: ok.
12 passed; 0 failed`.

**Finding 2, red** (new test after `a2522d8`, before `d5fc520`):

```
$ cargo test --lib --locked --no-fail-fast -- old_cookie_release_spawn_honors
thread '...' panicked at src/plugins/screensaver_inhibit.rs:1009:9:
the release spawn's uninhibit must be dropped at the configured bound
test result: FAILED. 0 passed; 1 failed
```

Cookie 1 stored, second connect fires the release spawn, the call
starts into the hanging fake — but the spawn bounded at the hard-coded
5 s, so the drop-guard missed the 2 s poll window.

**Finding 2, green** (after `d5fc520`): `test result: ok. 13 passed; 0
failed`.

**Interleaving test, expected green, green + stress**: `0385c4e` passes
by invariant (see Falsification attempts #3). Stability runs of the
three tests added this round: 25/25, 25/25 consecutive debug passes and
40/40 direct test-binary passes for the stale-bound test.

**Full suite:**

```
$ cargo test --all-features --locked --no-fail-fast
[exited with code 0]  # every test binary green under --no-fail-fast

# lib binary headline (separate run, cached build):
running 1150 tests
test result: ok. 1150 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.77s
```

**Clippy:**

```
$ cargo clippy --all-targets --all-features --locked -- -D warnings
   Compiling rust-connect v0.1.0 (...)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 33.42s
```

Clean, zero warnings.

**fmt:**

```
$ cargo fmt --check
# no output, exit 0 — clean
```

## Falsification attempts (attack surface, on record)

Each interleaving from the review brief, with disposition:

1. **Disconnect while inhibit in flight (the fixed race).** Proven by
   the landed R1 test and by invariant: `begin_disconnect` bumps the
   generation synchronously; the parked task's later check-and-store is
   one atomic critical section on the same mutex, so it must observe
   the bump. Not broken.

2. **Double connect, no disconnect.** `begin_connect` notes a stored
   `Inhibited(old)` cookie and releases it on a bounded spawn; the
   still-in-flight variant (first task parked when the second connect
   lands) is covered by the generation mismatch — the first task
   self-cleans. Landed test pins the stored-cookie variant. Not broken.

3. **Connect → disconnect → connect, BOTH tasks racing to the slot
   lock.** The M3 suite never parked two tasks at once; `0385c4e` now
   does. Invariant: each task's store decision compares against the
   CURRENT generation under the slot mutex, and only the newest
   connect's generation can match (generations strictly increase per
   bump). Whichever task wakes first, the stale one cannot clobber the
   newer one's slot. Accounting asserted per cookie: current XOR
   released. Not broken.

4. **Disconnect arriving between the task's generation check and its
   state write.** Impossible by construction: the check and the write
   are the same critical section (`release_now` block,
   `screensaver_inhibit.rs:317-327` at `a4aad08`); the disconnect's
   bump-and-take is the same lock. Whichever critical section is
   scheduled last sees a coherent state: store-then-take (disconnect
   releases the stored cookie) or bump-then-check (task sees mismatch,
   self-cleans). No interleaving exists between the two halves.

5. **Disconnect without connect.** `begin_disconnect` on a fresh/Idle
   slot is a no-op (generation bump, `taken = Idle`). Landed test
   `test_disconnect_without_connect_is_noop`. Not broken.

6. **Disconnect notify landing before the spawned task's first poll.**
   Safe: the generation is captured synchronously inside `on_connected`
   (`begin_connect`), not inside the task. The task's check happens
   after, whenever the scheduler runs it, and sees the bump. This is
   why the fix does not depend on spawn scheduling at all.

7. **Lock discipline.** Full inventory of production lock/await sites
   (grep, lines < 451 at review head): every `.await` is lock-free
   (`bounded_uninhibit`, backend internals), pre-lock
   (`enable_session_backend`), or post-release (both spawned tasks,
   `on_disconnected`). No path holds a slot mutex across an await; no
   path holds the map lock while taking a slot lock (`slot_for`
   releases the map guard before returning the Arc; `begin_*` and
   `cookie_for` take slot locks only afterwards); the map write lock in
   `slot_for` never nests a slot lock (the closure constructs one). The
   get-or-create race (two threads miss the read, both take the write)
   resolves via `entry().or_insert_with`. Not broken.

8. **Trait boundary.** `notify_connected` fires on every established
   connection, including link replaces (`listener.rs:250` spawns the
   500 ms-delayed init-packet pass per connection), and the
   `connection_replaced` arm (`connection_loop.rs:422`) deliberately
   skips `notify_disconnected` — together these make the
   connect-without-disconnect path a routine (60 s redial), not an
   anomaly, which is what the old-cookie release exists for. The
   registry serializes per-notify (`registry.rs:214-237`) and snapshots
   the plugin Arcs before awaiting. Two concurrent `notify_connected`
   for the same device serialize on the slot mutex; the later
   generation wins and the loser self-cleans. No connect path bypasses
   `notify_connected` (pairing-completion and listener both funnel
   through `send_plugin_init_packets`). One hazard found and NOT
   fixable in scope — see Critique.

## M3 FINDINGS.md adjudication

- **#1 (stale self-clean unbounded): CONFIRMED.** `a4aad08:338` awaited
  the backend call bare. Fixed in `a2522d8` with the pinning test the
  M3 lane should have written — the `HangingBackend` fake existed for
  exactly this and was never pointed at the stale path.
- **#2 (old-cookie spawn "also unbounded"): WRONG — the code was
  right.** `a4aad08:288-296` already wrapped that spawn in
  `bounded_uninhibit`. The M3 critique misdescribed its own landed
  code; the real unbounded site was #1. (The spawn did hard-code the
  timeout constant — that was real, and is finding 2 here, a weaker
  defect than "unbounded".)
- **#3 (u64 wrap): dismissed.** ~2^64 bumps per device is unreachable
  in human time; even on collision the mutex keeps state coherent and
  the worst case is one orphaned cookie in a scenario that cannot
  occur. `wrapping_add` is fine.
- **#4 (std Mutex grows no await): dismissed as a defect, kept as a
  note.** The invariant holds today with the inventory above to back
  it; the brief chose std::Mutex deliberately so `on_connected` stays
  a sync fn. Nothing to fix.
- **#5 (slots never pruned): overstated, out of scope.** The claimed
  re-pair hazard self-heals — `begin_connect` releases a leftover
  `Inhibited` cookie on the first connect after re-pairing. The
  residue is one small map entry per device id ever seen, holding a
  cookie integer. B4 unpair-time pruning stays sibling work.
- **#6 (family scope / Plugin-trait generation plumbing): agreed, out
  of scope, parked.** See Critique for the concrete inversion this
  review found that only trait-level generations can close.
- **#7 (test override vs production 5 s): resolved by `d5fc520`.** All
  three sites now answer to one configured bound; the 5 s default's
  interaction with systemd teardown headroom is unchanged and remains
  a documented dependency, not a defect.
- **#8 (cfg(test) layout divergence): dismissed.** Cosmetic.

## Critique — blunt

The design is right and I could not break it. That is not the same as
the brief being complete. Where it falls short:

- **The brief specified the unbounded await that became finding 1.**
  The fix-shape pseudocode has the stale path doing
  `uninhibit_and_stimulate(cookie).await` bare (`brief.md:66`) while
  bounding only the disconnect path. The M3 lane implemented the brief
  faithfully and then correctly flagged the result in its own FINDINGS
  #1 — but declined to fix it because "the brief specified" it. That is
  the executor copying the spec's bug and filing it as a footnote. The
  review round exists precisely to catch this: a spec that says "park a
  task forever on a wedged bus" in one clause while its own defect
  statement ("teardown latency at the mercy of a wedged session bus")
  forbids it elsewhere is not a contract worth honoring literally.
- **The brief's companion-test list was silently narrowed.** The
  deterministic both-parked interleaving (`brief.md:127-131`, "assert
  every cookie the fake ever issued is either the current cookie_for
  value or in uninhibits, and exactly one is current") never landed;
  every landed test parks at most one task. The M3 FINDINGS claims
  "the five places I tried to break it" — the one interleaving that
  actually exercises two racing tasks against the slot lock was not
  among them. A reviewer reading only the FINDINGS would believe the
  case was covered.
- **The M3 FINDINGS misdescribes its own code** (#2 claims unbounded
  where the code was bounded, and misses that #1's site was the real
  one). Both errors point the same direction: the critique was written
  from the brief and the diff summary, not from rereading the landed
  file. Any reader trusting it over the code would have gone hunting
  for a bug that did not exist while the real one sat one function
  down.
- **Found this round, not fixable here: a missed-inhibit window at the
  trait boundary.** Concrete interleaving: link replace where the OLD
  loop takes the `device_disconnected` error arm before the
  replacement registers its cancel token — its `cm.disconnect(gen_old)`
  returns `Ok(true)` (it still owns the slot), so `notify_disconnected`
  fires. The registry awaits plugins in map order; sftp's teardown can
  hold that loop for seconds (the registry comment says so), so the
  screensaver `on_disconnected` can land AFTER the replacement's
  500 ms-delayed `notify_connected`. The disconnect then bumps the
  generation past the new connect's, the in-flight inhibit self-cleans,
  and the slot sits `Idle` while the device is CONNECTED: the screen
  locks with a phone attached until the next redial (≤60 s) re-inhibits.
  Pre-fix, the same interleaving produced the reverse and worse failure
  (orphaned inhibit, screen never locks), so the fix strictly improves
  it — but only connection generations plumbed through the `Plugin`
  trait can close it, which the brief explicitly scoped out (M3 #6).
  Not a defect in this module; recorded so the follow-up brief inherits
  it.
- **Wall-clock assertions in tests are mild flake exposure.** The
  50 ms sleeps in the gated tests are load-bearing only as a yield —
  on the default current-thread test runtime the spawned task is
  granted its first poll the moment the test task sleeps, so 50 ms of
  wall time is not a race window in any realistic scheduling regime.
  `test_on_disconnect_bounds_uninhibit_under_hang`'s
  `elapsed < bound + 250 ms` real-time assert is the one with genuine
  (if small) CI-load exposure: it measures wall time around a 200 ms
  timer with 125% headroom. Acceptable; worth knowing if it ever fires
  on a starved runner.
- **What the tests still do not cover:** nothing here exercises the
  real zbus backend (by design, per the brief); the oracle after deploy
  (journal accounting under a real wifi flap) is the actual proof and
  remains open. The unit suite proves the state machine; it cannot
  prove the D-Bus call shapes.

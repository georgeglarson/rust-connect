# FINDINGS — generation-scoped teardown, GLM-5.3 adversarial review round

Review lane for branch `fix-generation-scoped-teardown` (this worktree is
cut from it). Verdict up front: **no confirmed findings.** Every
falsification attempt below either held or resolved as by-design /
pre-existing, with the evidence named. Zero production-code commits from
this round; the only change is this file.

## What changed

Nothing in `src/`. The review attempted to break the branch in the seven
shapes the brief enumerated; each attempt is recorded below with its
outcome. Three temporary flip experiments (guard disabled, unpair
ordering disabled, over-broad guard) were applied, run, and reverted via
`git checkout` — the working tree is byte-identical to the reviewed
commits.

## How it was verified

### Flip experiments (red-test honesty)

All runs: `CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target
cargo test --all-features --locked --no-fail-fast --lib -- --test-threads=1`
with the five R-test filters, `set -o pipefail`, exit codes checked.

- Baseline (branch as committed): 5/5 green, `test result: ok. 5 passed;
  0 failed`.
- **Flip 1 — guard disabled** (`if false && cm.get_generation(...)` in
  `notify_disconnected`): exit 101. R1/R3/R4 FAILED with the exact
  defect-naming panics the implementer recorded:
  - R1 `registry.rs:685`: "stale teardown must not reach any plugin while
    a live generation holds the device; got [\"dev-1\"]"
  - R3 `sftp/mod.rs:2328`: "the replacement's freshly-stored credentials
    were wiped by the stale teardown"
  - R4 `screensaver_inhibit.rs:1118`: "stale teardown lifted the live
    replacement's inhibition / left: None / right: Some(100)"
  - R2 green (genuine path unaffected), R5 green (guard off → dispatch
    proceeds — correct for this flip).
- **Flip 2 — unpair ordering reverted, guard on** (`if false &&` on the
  unpair disconnect block): exit 101, ONLY R5 FAILED at
  `device.rs:1170`: `left: [] / right: ["peeraaaaa…"]` — the half-state
  red the implementer claimed. R1 stayed green (isolates the ordering
  fix from the guard).
- **Flip 3 — over-broad guard** (guard returns on every call): R2 FAILED
  at `registry.rs:721`: `left: [] / right: ["dev-1"]`. R2 pins the
  genuine path; a no-op registry cannot pass it.

Adjudication: the red-before-green record is honest. Each R-test fails
for the reason it names, in the flip that isolates exactly its defect.

### Attack surface, item by item

1. **Guard placement / read-only.** The guard is the first statement of
   `notify_disconnected` (`registry.rs:260-270`), ahead of the plugin
   snapshot. One early return: an `info!` log, no state touched.
   `get_generation` takes a tokio read lock on `connections` (plus a
   std read lock on the test shadow under cfg) and writes nothing. No
   path through the guard mutates the manager, lifecycle, or plugin
   state. Confirmed read-only.
2. **Call-site re-audit (independent).** Repo-wide grep finds exactly 8
   production `notify_disconnected` sites — the implementer's table
   lists 8 (the brief said 7; the grep is the authority). Verified each
   against the code, not the table:
   - `connection_loop.rs:282,408,444,469` — all four are
     `match cm.disconnect(id, gen) { Ok(true) => … notify }`. Removes-
     before-notify, ownership-gated.
   - `listener.rs:183,204` — identical ownership gate. The claim "only
     fire for the active generation" holds: the generation was minted by
     THIS handler's `accept_incoming`; a replacement flips `disconnect`
     to `Ok(false)` and notify is skipped (replacement owns state).
   - `device.rs:550` (`disconnect_device`) — gated on `Ok(true)`.
   - `device.rs:327` (`unpair_device`) — the forced site, now
     disconnect-before-notify (see item 5).
   No site can notify while a live entry exists except through the
   designed residual window (replacement registers after the owning
     disconnect) — where skipping is the correct behavior. Also grepped
   for direct `plugin.on_disconnected(` bypasses: the only production
   dispatch point is `registry.rs:285`; all other hits are plugin unit
   tests.
3. **Test generation shadow.**
   - `disconnect(id, gen)` with a shadow-marked device and empty real
     map: skips the stale check (real map empty), removes nothing, clears
     the shadow, returns `Ok(true)` — exactly the production semantics
     of "slot already empty → true". R5 depends on this; verified by
     R5's green and flip 2's red.
   - **Divergence proof.** Every write to the real `connections` map is
     either `disconnect`'s `remove` (which also clears the shadow) or an
     `insert` (inbound.rs:250,317,362,477; outbound.rs:73,372 — all
     verified `insert`), which MASKS the shadow because both
     `get_generation` and `is_connected` consult the real map first.
     Once masked, the shadow can never re-emerge: the only unmasking
     operation is `disconnect`, which clears it in the same critical
     section. So the invariant "shadow visible ⟹ real map empty for
     that device" is inductive over all operations. No sequence gives a
     production-shaped caller a wrong answer.
   - `Ok(false)` arm does NOT clear the shadow — correct: on that arm a
     real entry holds the slot and masks the shadow; clearing would be
     irrelevant, and the arm means "a newer generation owns the link",
     which is about the real map.
   - Release leak: `test-helpers` is enabled only by the self
     dev-dependency (`Cargo.toml:126`); no `.cargo/config.toml` exists
     (only `audit.toml`), no `required-features`, not a default feature.
     `cargo build --release` cannot compile the shadow in.
   - Nit (not fixed, no caller): `is_current_generation`
     (`connection/mod.rs:901`) does not read the shadow. A future test
     marking a shadow and asserting `is_current_generation(dev, gen)`
     would get `false` where the shadow's claim implies `true`. No
     current test does this; flagging so the seam's contract is known.
4. Red-test honesty: see flips above.
5. **Unpair edge cases.**
   - (a) Unpair while connected: R5, green / flip-2 red.
   - (b) Replacement races between unpair's `get_generation` and
     `disconnect`: `disconnect` returns `Ok(false)`, the slot keeps the
     replacement, the guard correctly stands the unpair's
     `notify_disconnected` down, and the trust-boundary teardown is
     DEFERRED, not lost — the replacement's own loop exit arm (read
     error / peer close) disconnects and notifies, and the daemon
     shutdown arm (`connection_loop.rs:441`) flushes it unconditionally
     as a backstop. `delete_device` has the identical race shape
     (`device.rs:439-444`), so this is parity, not a new hole. The
     peer learned of the unpair via `notify_peer_unpair` on the old
     link and will drop the link itself.
   - (c) Never-connected unpair: `is_connected` false → no disconnect →
     `notify_disconnected` with no live entry → teardown dispatches.
     The common offline-unpair path is unchanged.
   - (d) `delete_device` parity: same
     `is_connected → get_generation → disconnect` block, verified
     character-for-character. One divergence: ordering of
     `sftp.cleanup_device` — delete runs it BEFORE the disconnect,
     unpair AFTER. Benign: `cleanup_device` is documented-idempotent
     and mount-lock serialized (`sftp/mod.rs:478-510`), and the
     pre-fix unpair already raced `cleanup_device` against the loop's
     exit teardown (the peer closes on `pair=false`). Not a defect;
     noting it because the implementer's "mirrors delete_device" claim
     is true of the disconnect block, not of the surrounding order.
   - Pre-existing, unchanged by this branch: unpair can double-dispatch
     plugin teardown (its own notify plus the loop exit's). Handlers
     are idempotent (sftp `remove` on absent key, screensaver uninhibit
     on empty slot). The guard only ever skips; it cannot duplicate.
6. **Wiring census.** The only production `PluginRegistry` construction
   is `app.rs:91` (wired); every `AppState` path — `new`,
   `new_without_input`, bootstrap, service_manager, orchestrator —
   funnels through `new_inner`, which wires it. All other
   `PluginRegistry::new()` hits are `#[cfg(test)]` modules or
   integration-test files. `PluginRegistry::default()` exists but has
   no production caller (grep). No unguarded production registry.
7. **Concurrency.** The guard calls `get_generation` while holding NO
   registry lock (it precedes the plugins snapshot), so there is no
   registry → manager lock pair at all. sftp's inner guard takes
   mount-lock → manager-read (`sftp/mod.rs:704` then `:727`); no path
   takes manager locks then mount/plugins locks (`disconnect`'s only
   nested acquisition is connections → cancel_tokens, the documented
   order). No cycle, no deadlock. No lost genuine teardown (every
   genuine path disconnects first; shutdown flushes); no new double
   teardown (see 5d).

### Gates

```
$ set -o pipefail; CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
    cargo test --all-features --locked --no-fail-fast
```
39 test binaries, every one `test result: ok … 0 failed`; lib suite
`test result: ok. 1157 passed; 0 failed; 0 ignored` (30.88s). Exit 0
under pipefail. Run WITHOUT a TMPDIR override and at DEFAULT test
threads — the parallel-mode clipboard_x11 flake the implementer dodged
with `--test-threads=1` did not reproduce.

```
$ cargo clippy --all-targets --all-features --locked -- -D warnings
    Finished `dev` profile … in 25.24s        # exit 0
$ cargo fmt --check                           # exit 0, clean
```

## Critique — blunt

Arguing against the brief, as instructed:

- **The guard's correctness rests on an undocumented convention, and
  the diff does nothing to make the convention load-bearing.** Every
  caller of `notify_disconnected` must now know: dispatch is
  conditional on the manager having no entry. That contract lives in a
  comment and in FINDINGS tables. The next contributor who adds a call
  site (a ninth) gets no compiler error when they notify with a live
  entry — they get a silently swallowed teardown whose only trace is an
  INFO line. The brief chose registry-level specifically to avoid
  trait churn, and that's the right cost call today, but the
  mitigation for the convention's fragility (a debug-level counter, a
  lint, a doc on the fn) was parked as "separate change" by both the
  implementer and the brief. The doc-comment on `notify_disconnected`
  itself is where the contract belongs and is the one cheap thing this
  branch could have added without scope creep. I did not add it —
  "do not refactor beyond a confirmed finding" — but this is the
  weakest joint in the design and it is deliberate.
- **The residual window is real and the fix's own oracle can't see
  it.** The guard narrows the customer windows to the
  check-to-dispatch interleave; within that window sftp's inner guard
  still stands but the screensaver does NOT (its only protection was
  the registry guard that just raced past). The journal oracle
  (`teardown_superseded_by_live_replacement` fires, no
  `sftp_disconnect_cleanup_superseded` storms) confirms the guard works
  when it wins the race; there is no observable for the races it loses.
  The brief scoped this out with an honest cost argument (17 impls of
  churn), and I could not construct a production-plausible interleaving
  where the loss is worse than the pre-fix behavior — the loss requires
  the replacement to complete a full TLS handshake and `insert` between
  one `get_generation` read and a loop over plugin Arcs. But "could not
  construct" is my bound, not a proof.
- **The test shadow adds a second source of truth for liveness that
  only discipline keeps honest.** Three production methods now branch
  under `cfg(any(test, feature = "test-helpers"))`, and the invariant
  that makes the shadow sound (every real-map `remove` also clears it;
  every `insert` masks it) is enforced only by those call sites staying
  in agreement. The next person who adds a real-map removal path — say
  a `remove_all` for shutdown — must remember the shadow or tests will
  silently lie (shadow-live for a really-disconnected device → guard
  skips a teardown the test expected to run → test goes red in a way
  that reads like a guard bug, not a shadow bug). The divergence proof
  in this review is against today's call sites only. `is_current_generation`
  already forgot the shadow; that's the pattern's first instance.
- **R5 tests the fix against a seam, not against a link.** The unpair
  test marks the shadow and never establishes a real connection, so the
  path where `disconnect` actually shuts a socket down (cancel-token
  removal, loop eviction, `connection_disconnected` logging) is not
  exercised on the unpair path by this branch. The full suite covers
  disconnect mechanics elsewhere, so this is a coverage seam, not a
  hole — but the brief's "in-process; no LAN" budget is doing quiet
  load-bearing work here.
- **What I tried to break and could not:** the eight-site audit
  (including the two listener identity-exchange sites the brief
  flagged as unknown — they are ownership-gated exactly as claimed);
  a shadow/real-map divergence (proved impossible under current write
  sites); the `Ok(false)` shadow-clearing question (correct as
  written); unpair-race B4 safety (deferred-and-flushed, `delete_device`
  parity); lock inversion (none exists — the guard holds no registry
  lock during its manager read); wiring census (single production
  construction, wired); `test-helpers` release leak (Cargo.toml +
  .cargo checked). The specific parts of the brief's approach that
  survived contact: the registry-level placement (per-device plugin
  state really is uniform across plugins — sftp creds, screensaver
  cookie, notification history all die together), and the decision to
  keep sftp's inner guard as defense-in-depth (it covers the residual
  window for the highest-value asset).

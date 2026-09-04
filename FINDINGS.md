# FINDINGS — generation-scoped teardown

Merge class: **B**. The fix touches the teardown choke point of every
plugin (registry-level dispatch) plus the unpair trust boundary, plus
adds a test-only generation shadow on `ConnectionManager`. None of
those surfaces are settled.

## What changed

- **Registry-level supersede guard** in `PluginRegistry::notify_disconnected`
  (`src/plugins/registry.rs:225`). When the registry has a wired
  `ConnectionManager`, a teardown for a device with a live generation
  is logged at INFO (`teardown_superseded_by_live_replacement`) and
  returns WITHOUT dispatching to any plugin. Read-only on the
  manager; never mutates shared state from the guard.
- **Wired the connection manager into the registry** in `AppState`
  (`src/app.rs:88` → `src/app.rs:91`). Production `PluginRegistry::new()`
  callers without an `AppState` continue to see the prior
  no-connection-manager shape; existing tests that build a registry
  directly stay untouched.
- **Forced-path fix in `unpair_device`**
  (`src/api/handlers/device.rs:309-318`). Mirrors `delete_device`'s
  `is_connected → get_generation → disconnect` pattern BEFORE
  `notify_disconnected`. Without this, the registry-level guard
  would skip the trust-boundary teardown for any unpair that
  races a still-live connection — a regression specific to
  unpair, not a problem on the connection-loop exit arms
  (they're already removes-before-notify via `cm.disconnect(generation)`).
- **Test-only generation shadow on `ConnectionManager`**
  (`src/protocol/connection/mod.rs`). Lets `get_generation`,
  `is_connected`, and `disconnect` be exercised in unit tests
  without a real TLS pair. The shadow is a separate map
  consulted AFTER the real `connections` map; production builds
  see the empty map and the read is compiled out. `mark_generation_for_test`
  and `unmark_generation_for_test` are the seams. `disconnect`
  clears the shadow when it removes a live entry, so the
  supersede guard's read sees a fresh `None` after a successful
  disconnect.
- **R1–R5 red tests** that pin each customer shape:
  - `plugins::registry::tests::test_notify_disconnected_superseded_while_replacement_live` (R1, `src/plugins/registry.rs`)
  - `plugins::registry::tests::test_teardown_proceeds_when_no_live_connection` (R2, `src/plugins/registry.rs`)
  - `plugins::sftp::tests::test_stale_teardown_does_not_wipe_replacement_credentials` (R3, `src/plugins/sftp/mod.rs`)
  - `plugins::screensaver_inhibit::tests::test_stale_teardown_leaves_inhibit_slot_inhibited` (R4, `src/plugins/screensaver_inhibit.rs`)
  - `api::handlers::device::tests::test_unpair_teardown_runs_even_while_connected` (R5, `src/api/handlers/device.rs`)

## How it was verified

Each R-test was verified red-before-green by toggling the
guard or the unpair ordering through temporary comment-out and
re-running. The exact sequence and real output:

- **R1** (`plugins::registry::tests::test_notify_disconnected_superseded_while_replacement_live`):
  red BEFORE the fix (guard removed), green AFTER.
  ```
  $ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target cargo test --all-features --locked --no-fail-fast --lib plugins::registry::tests::test_notify_disconnected_superseded_while_replacement_live
  test plugins::registry::tests::test_notify_disconnected_superseded_while_replacement_live ... FAILED
  thread 'plugins::registry::tests::test_notify_disconnected_superseded_while_replacement_live' (1379253) panicked at src/plugins/registry.rs:690:9:
  stale teardown must not reach any plugin while a live generation holds the device; got ["dev-1"]
  test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1156 filtered out; finished in 0.00s
  ```
  After restoring the guard, the same command reports
  `test result: ok. 1 passed; 0 failed`.

- **R3** (`plugins::sftp::tests::test_stale_teardown_does_not_wipe_replacement_credentials`):
  red BEFORE the fix.
  ```
  thread 'plugins::sftp::tests::test_stale_teardown_does_not_wipe_replacement_credentials' (1380087) panicked at src/plugins/sftp/mod.rs:2329:9:
  the replacement's freshly-stored credentials were wiped by the stale teardown
  ```

- **R4** (`plugins::screensaver_inhibit::tests::test_stale_teardown_leaves_inhibit_slot_inhibited`):
  red BEFORE the fix.
  ```
  thread 'plugins::screensaver_inhibit::tests::test_stale_teardown_leaves_inhibit_slot_inhibited' (1380086) panicked at src/plugins/screensaver_inhibit.rs:1121:9:
  assertion `left == right` failed: stale teardown lifted the live replacement's inhibition
    left: None
   right: Some(100)
  ```

- **R5** (`api::handlers::device::tests::test_unpair_teardown_runs_even_while_connected`):
  red in the half-state (guard enabled, unpair ordering
  NOT fixed).
  ```
  thread 'api::handlers::device::tests::test_unpair_teardown_runs_even_while_connected' (1388823) panicked at src/api/handlers/device.rs:1169:9:
  assertion `left == right` failed: recording plugin saw no on_disconnected during unpair of a connected device; the registry-level guard skipped the trust-boundary teardown
    left: []
   right: ["peeraaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
  ```
  Green with both fixes in place.

- **R2** (`plugins::registry::tests::test_teardown_proceeds_when_no_live_connection`):
  passes on main AND with the fix; it pins the genuine-teardown
  path so a future over-broad guard can't silently swallow it.

- **Full gate run** (single-threaded to dodge a pre-existing
  clipboard_x11 fixture-collision flake in parallel mode):

  ```
  $ set -o pipefail; CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target cargo test --all-features --locked --no-fail-fast -- --test-threads=1
  ...
  test result: ok. 1157 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ...
  test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- **Clippy** (`cargo clippy --all-targets --all-features --locked -- -D warnings`):
  clean, no warnings.
- **fmt** (`cargo fmt --check`): clean after a single `cargo fmt`
  pass to absorb the new file/line widths.

## Call-site audit (`notify_disconnected`, 7 sites)

Per the brief's instruction to audit every call site and record
each in FINDINGS as removes-before-notify / forced / needs-fix:

| Site | Classification | Notes |
|---|---|---|
| `src/api/handlers/device.rs:314` (`unpair_device`) | **forced → fixed** | Pre-fix called `notify_disconnected` while the device could still be connected. The new code mirrors `delete_device` (`:426-431`): `is_connected → get_generation → disconnect` first, then `notify_disconnected`. The guard then sees `None` and dispatches the trust-boundary teardown. |
| `src/api/handlers/device.rs:537` (`disconnect_device`) | removes-before-notify | `if let Ok(true) = disconnect(...)` gates the `notify_disconnected` call. Unchanged. |
| `src/protocol/connection_loop.rs:282` (pair-rejected unpair arm) | removes-before-notify | `if let Ok(true) = cm.disconnect(...)` gates the call. Unchanged. |
| `src/protocol/connection_loop.rs:408` (read-error arm) | removes-before-notify | Same shape as above. Unchanged. |
| `src/protocol/connection_loop.rs:444` (shutdown arm) | removes-before-notify | Same shape. Unchanged. |
| `src/protocol/connection_loop.rs:469` (idle-timeout arm) | removes-before-notify | Same shape. Unchanged. |
| `src/protocol/listener.rs:183` (identity-exchange timeout) | removes-before-notify | `match cm.disconnect(&device_id, generation) { Ok(true) => ..., notify_disconnected(...) }`. Unchanged. |
| `src/protocol/listener.rs:204` (identity-exchange failure) | removes-before-notify | Same shape as `:183`. Unchanged. |

The brief flagged the two listener sites as "unknown" — they are
both gated on the same ownership pattern as the connection-loop
exit arms. A teardown path that could not be made
removes-before-notify would have been a stop-and-discuss
escalation per the brief; the audit found none.

## Critique — blunt

- **The fix shape is sound, but the residual race is real and
  acknowledged.** Between the registry's `get_generation` read and
  the plugin dispatch, a same-cert replacement CAN register. The
  brief scopes this down to "sub-millisecond interleave" and names
  it explicitly. Closing it would require threading a generation
  through the `Plugin` trait (and redoing 17 impls) — the brief's
  measured call is correct: the win doesn't justify the churn.
  The sftp inner guard (`src/plugins/sftp/mod.rs:727`) stays as
  defense-in-depth against the same window. This is the right call
  but it is a real call, not a free one: the same window still
  exists, and the post-fix oracle (`teardown_superseded_by_live_replacement`
  fires during fast reconnect cycles; no
  `sftp_disconnect_cleanup_superseded` storms) only proves the guard
  is doing the work it can do, not that the work is complete.

- **The unpair ordering fix is a regression surface I introduced.**
  Pre-fix, the unpair path's call to `notify_disconnected` was
  always unconditional — there was no guard to skip it. Post-fix,
  if the unpair's `disconnect` returns `false` (a stale generation
  raced a replacement), `cleanup_device` still runs but
  `notify_disconnected` does NOT (the `if let Ok(true) =
  cm.disconnect(...)` is inside the existing pattern at
  `delete_device` too; we do not echo it in unpair). That is the
  correct shape — same as `delete_device`, same as the
  connection-loop exit arms — but it does mean a future change
  that adds a side-effect to `notify_disconnected` would silently
  skip for an unpair-while-stale-generation edge case. Worth a
  follow-up audit if anyone touches `notify_disconnected` semantics.

- **The test-only generation shadow on `ConnectionManager` is a
  legitimate seam, but it widens the surface that production
  builds must keep honest.** Three production methods (`is_connected`,
  `get_generation`, `disconnect`) now have a `cfg(test)` branch.
  The branches are guarded on `any(test, feature = "test-helpers")`
  and the read sites are zero-cost when the shadow is empty, but
  every future change to those methods must re-verify both code
  paths. The alternative was to require a real TLS pair for these
  unit tests; that was the wrong tradeoff (a full in-process TLS
  pair adds 50-100ms per test and the brief explicitly says
  "in-process; no LAN"). The right follow-up is to keep the
  shadow surface tiny and review it whenever one of those methods
  changes — `is_connected` is read on every render and every
  unpair, so this matters.

- **The test fixtures couple to internals more than I'd like.**
  R3 (sftp) reaches into the SFTP plugin's `connections` table via
  `plant_connection_for_test` and the `with_connection_manager`
  chain. R4 (screensaver) wraps `ScreensaverInhibitPlugin` and
  drives its state machine directly. R5 (unpair) builds an inline
  recording mock plugin because the production plugins' state
  machines are too tightly coupled to assert through them. This
  is fine — these are unit tests, not integration tests — but the
  recording-mock pattern in R5 is the one I'd replicate for any
  future guard work, because it survives plugin refactors.

- **R5's "recording mock plugin" shape is a code smell pointing
  at a missing trait method.** `Plugin::on_disconnected` returns
  `()` and gives no signal upstream about whether it fired. The
  registry's supersede guard silently swallows teardowns; a future
  debugging session will need to know whether a particular
  `notify_disconnected` was a no-op or actually dispatched. A
  counter on `PluginRegistry` (incremented on dispatch) would
  surface this; I did not add it because it would be a fresh
  trait surface for diagnostics only. Worth proposing as a
  separate change.

- **The audit also surfaced a non-issue worth naming.** The
  listener's identity-exchange failure paths (`:183`, `:204`) are
  the only `notify_disconnected` call sites that fire BEFORE the
  device has ever reached `Connected`. With the registry-level
  guard in place, both sites could in principle hit a `None`
  `get_generation` (no live connection) and proceed normally — the
  brief's "needs-fix" worry was about them firing WHILE a live
  connection existed. They don't: the only way to reach those
  sites is for THIS generation to be the active one (TLS
  handshake is what created it), and the active generation is the
  one `disconnect(generation)` returns `true` for. So the
  call-site audit is clean and the listener needs no change. I
  am stating this explicitly because the brief flagged them as
  the unknown, and "verified clean" is what the answer is.

- **No behavioral oracle for the brief's "phone reconnects after a
  flap keep their mount creds" claim.** I cannot run a live
  reconnect cycle in this sandbox. The shape that delivers it
  is: registry guard sees `Some` on stale teardown → registry
  returns without dispatching → sftp `connections.remove` never
  fires → next `kdeconnect.sftp` packet re-issues creds, OR a
  mount path under the live replacement works because creds
  were never wiped. R3 verifies the second half in isolation
  (creds survive the stale teardown). The first half — that the
  live replacement's creds are still there to mount with — is
  exercised by the existing sftp mount tests but not by this
  fix. Live verification on a test phone is the only way to
  close that gap.

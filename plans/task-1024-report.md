# vk #1024 report — roll the zbus connection after session-bus drop

Branch: `fix-mpris-conn-roll` (worktree `~/repos/rust-connect-fix-mpris-conn-roll`)
Commit: `5f76095` — "fix(mpris): roll the zbus connection after session-bus drop (vk #1024)"

## What changed

`src/plugins/mpris/zbus_backend.rs`:

- `ZbusMprisBackend.conn` field: `zbus::Connection` → `Arc<tokio::sync::RwLock<zbus::Connection>>`
  (zbus_backend.rs:96-104).
- New private `async fn current_conn(&self) -> zbus::Connection` (zbus_backend.rs:132-137):
  clones a snapshot under a short read lock, never held across an await on a proxy call.
- `connect()` wraps the initial connection in `Arc::new(RwLock::new(conn))` (zbus_backend.rs:126).
- `player_proxy`, `player_state`, `album_art` (the three call sites that built proxies over
  `&self.conn`) now call `self.current_conn().await` first and build the proxy over the local
  snapshot (zbus_backend.rs:148-154, 552-558, 646-652).
- `start_watching` passes the shared `Arc<RwLock<..>>` unchanged (it already did `self.conn.clone()`,
  which now clones the Arc instead of the Connection — no body change needed, only the field-type
  change made this correct).
- `watch_supervisor` (zbus_backend.rs:695-786):
  - Signature takes `Arc<RwLock<zbus::Connection>>` instead of `zbus::Connection`.
  - Each loop iteration snapshots the current conn for `watch_loop` and records `Instant::now()`
    before the call and `.elapsed()` after, to drive the hot-loop guard.
  - On successful re-acquire: `*conn.write().await = new_conn;` — installs the recovered
    connection into the shared cell instead of dropping it. This is the fix for defect #1 (control
    methods) and defect #2 (the watch loop reusing the dead connection), since the next loop
    iteration's snapshot now picks up the fresh connection too.
  - Deleted the stale SAFETY/band-aid comment block (old zbus_backend.rs:739-747); replaced with a
    doc comment on `watch_supervisor` itself stating the install-on-recovery contract.
  - Hot-loop guard: `backoff_ms` only resets to 500 if the just-finished `watch_loop` ran for
    `>= HEALTHY_RUN_THRESHOLD` (`Duration::from_secs(5)`). Otherwise it logs
    `mpris_watch_hot_loop_guard` and backs off exponentially, same as a failed reconnect.
- Added `use std::time::{Duration, Instant};` to the module's imports.

No changes to `mod.rs` (the `BackendLost` handling at mod.rs:755 is untouched) or to any other
plugin/wire-shape code, per the brief's restriction.

## Red before green

New file `tests/mpris_bus_recovery.rs` (its own test process — see the module doc for why it
must stay the only `#[tokio::test]` in the file: it sets `DBUS_SESSION_BUS_ADDRESS`
process-wide via `std::env::set_var` before any zbus connection exists).

Run against the **unfixed** tree (before the `zbus_backend.rs` edits, only the test file
present):

```
running 1 test

thread 'backend_survives_session_bus_restart' (43793) panicked at tests/mpris_bus_recovery.rs:271:6:
fake player was not re-discovered after bus restart
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test backend_survives_session_bus_restart ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 15.07s
```

This matches the predicted failure mode exactly: on the unfixed tree, `watch_supervisor`
re-acquires a fresh connection successfully after the bus restarts, but drops it — so the very
next `watch_loop` iteration keeps calling `DBusProxy::new` against the same dead original
connection, fails fast, and the cycle repeats forever. Re-discovery (the test's step 5) never
completes, so the test hung on `recv_until`'s bounded 15s wait and then failed with the assertion
above (it did not hang indefinitely — the bounded wait did its job).

Run against the **fixed** tree: passes in 1.59s.

```
running 1 test
test backend_survives_session_bus_restart ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.59s
```

The red test and the fix are combined into a single commit (the suite must stay green
per-commit; the red run above was captured before committing, per the brief).

## Gates (all green)

- `cargo build --locked` — clean (production-only build).
- `cargo test --all-features --locked` — full suite green: 905 lib unit tests (matches baseline),
  every integration test file (`mpris_bus_recovery` 1/1, `mpris_session_bus` 0/0 ignored — needs a
  live session bus, `protocol_integration` 41/41, `usb_integration` 2/2 + 3 ignored requiring
  hardware, etc.), 6 doc-tests. 0 failed anywhere.
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo fmt --check` — clean (one `cargo fmt` pass applied to both changed files before the
  commit; the diff was pure line-wrapping, no semantic change).

## Uncertainty / deferred items

- The hot-loop guard's 5s threshold is untested in isolation — the red/green test above exercises
  the "successful, healthy reconnect" path (recovery completes in ~1.5s of actual bus downtime,
  well past the point where `watch_loop` would have started blocking on `stream.next()` again), not
  the specific "reconnect succeeds but watch_loop still fails fast" branch the guard defends
  against. That branch's precondition (a `Connection::session()` call that succeeds, handed a
  connection whose subsequent `DBusProxy::new` or `list_names`/`receive_name_owner_changed` calls
  fail) doesn't have a clean reproduction with a real dbus-daemon — it would need a mock/fake
  D-Bus transport to force that specific failure shape. Left as written-per-brief rather than
  additionally instrumented; flagging here rather than silently calling it fully covered.
- Did not add a unit test for `current_conn` in isolation — it's a two-line lock-and-clone helper
  exercised end-to-end by every existing unit test that touches `player_proxy`/`player_state` plus
  the new integration test.

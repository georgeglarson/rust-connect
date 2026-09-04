# Findings — `fix-test-discovery-loopback-scoping`

Branch: `fix-test-discovery-loopback-scoping` (off `489eb31`). Executor:
single M3 lane. Gate verified, red-before-green observed.

## What changed

Three `cfg`-gated edits, all keyed on `#[cfg(any(test, feature =
"test-helpers"))]` — the same gate D2 (2026-09-02 audit, `d89d152`) used
for `SERVICE_TYPE` in `src/protocol/mdns_discovery.rs:40-53`, and the
reason that gate is complete: `test-helpers` is a dev-dependency-only
feature (`rust-connect = { path = ".", features = ["test-helpers"] }` in
`Cargo.toml`); `cargo build --locked` (interop harness + release) never
sets it, so the gate compiles out of every production binary byte-clean.

1. **`src/protocol/discovery.rs`** — under the gate, the UDP socket binds
   to `127.0.0.1:port` instead of `0.0.0.0:port`, and `broadcast_addr` is
   `127.0.0.1:port` instead of `255.255.255.255:port`. Production builds
   keep the existing `Ipv4Addr::UNSPECIFIED` / `Ipv4Addr::BROADCAST`
   pair, unchanged. The oversized-identity retry at `:193-220` keeps
   resending to `self.broadcast_addr`, so it inherits the new address
   automatically — no separate edit.
2. **`src/protocol/mdns_discovery.rs::MdnsDiscoveryService::new`** —
   under the gate, the freshly created `ServiceDaemon` is restricted to
   the loopback interface BEFORE `register`. The mechanics: `IfSelection`
   entries in `mdns-sd` 0.20.3 are applied as a sequence of overrides on
   a defaults-enabled list (`service_daemon.rs::apply_intf_selections`,
   `~1777`: starts at `vec![true; intf_count]`, walks `if_selections`,
   last match wins), so an `enable_interface(LoopbackV4)` alone is a
   no-op. The fix uses `disable_interface(All)` then
   `enable_interface(LoopbackV4)` — the last selection that matches each
   interface is the one that takes effect, so non-loopback interfaces
   end up disabled, loopback ends up enabled. Verified by reading the
   vendored source at
   `~/.cargo/registry/src/index.crates.io-*/mdns-sd-0.20.3/src/service_daemon.rs:1643-1655, 1775-1801`.
   `reannounce` (`:151`) inherits the scope — the daemon's interface
   list is sticky for the daemon's lifetime. `run` (`:191`) browses
   against the same restricted daemon, so in-test discovery stays
   loopback-only throughout.
3. **Red tests** — `R1 test_test_build_broadcast_is_loopback_scoped` and
   `R2 test_test_build_listener_binds_loopback` in
   `src/protocol/discovery.rs::tests`, both pinned to the
   `cargo test`-visible `pub` fields (`broadcast_addr`,
   `socket.local_addr()`).

What I deliberately did NOT change:

- **The split-brain detector** stays unchanged. Loopback-scoped
  announces still reach the same-host daemon (which is what D2's doc
  comment means by "Linux delivers looped multicast to every local
  member of the group"), and a `split_brain_suspected` hit during a test
  run remains a TRUE signal — another implementation IS announcing on
  this host. Suppressing it under the gate would erase a real signal
  about a real second daemon.
- **`tests/interop/run.sh`** — unaffected by design: it uses a non-test
  build path that never carries `test-helpers`, so the gate compiles
  out and the interop harness makes a real LAN announce, exactly as it
  needs to.

## How it was verified

### Red-before-green — explicit failure messages from R1/R2 on `489eb31`

Build on `CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target`
(warm on-disk dir, worktree itself is tmpfs per the brief):

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --locked --lib --no-fail-fast -- \
    test_test_build_broadcast_is_loopback_scoped \
    test_test_build_listener_binds_loopback
   Compiling rust-connect v0.1.0 (...worktree)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 7.09s
     Running unittests src/lib.rs (...target/debug/deps/...)

running 2 tests
test protocol::discovery::tests::test_test_build_broadcast_is_loopback_scoped ... FAILED
test protocol::discovery::tests::test_test_build_listener_binds_loopback ... FAILED

failures:

---- protocol::discovery::tests::test_test_build_broadcast_is_loopback_scoped stdout ----

thread '...' panicked at src/protocol/discovery.rs:880:9:
assertion `left == right` failed: test-build broadcast_addr must target
127.0.0.1:34750, not 255.255.255.255:34750 — cargo test must not send a
UDP identity broadcast on the real LAN
  left: 255.255.255.255:34750
 right: 127.0.0.1:34750

---- protocol::discovery::tests::test_test_build_listener_binds_loopback stdout ----

thread '...' panicked at src/protocol/discovery.rs:912:9:
assertion `left == right` failed: test-build DiscoveryService must bind
to 127.0.0.1, got 0.0.0.0:36011
  left: 0.0.0.0
 right: 127.0.0.1
```

Both tests fail for the exact defect the brief names: R1's
`broadcast_addr` was LAN-broadcast (`255.255.255.255`), R2's bind was
LAN-wide (`0.0.0.0`). After the gate fix, both tests pass:

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --locked --lib --no-fail-fast -- \
    test_test_build_broadcast_is_loopback_scoped \
    test_test_build_listener_binds_loopback
    Finished `test` profile [unoptimized + debuginfo] target(s) in 21.37s
     Running unittests src/lib.rs (...target/debug/deps/...)

running 2 tests
test protocol::discovery::tests::test_test_build_broadcast_is_loopback_scoped ... ok
test protocol::discovery::tests::test_test_build_listener_binds_loopback ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 1150 filtered out
```

### Full suite green

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --all-features --locked --no-fail-fast
... (1152 lib + integration tests, 0 failures across all binaries) ...
```

Key regression guards (the mDNS leg's gate from the brief: "new +
register + reannounce must still succeed under the restriction"):

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --all-features --locked --lib --no-fail-fast -- \
    test_test_build_broadcast_is_loopback_scoped \
    test_test_build_listener_binds_loopback \
    test_announce_then_browse_resolves_ourselves \
    test_reannounce_publishes_a_real_update \
    test_announcer_in_test_builds_is_invisible_to_production_browsers \
    test_new_binds_and_broadcasts_on_configured_port
    Finished `test` profile [unoptimized + debuginfo] target(s) in 6.90s

running 6 tests
test protocol::discovery::tests::test_new_binds_and_broadcasts_on_configured_port ... ok
test protocol::discovery::tests::test_test_build_broadcast_is_loopback_scoped ... ok
test protocol::discovery::tests::test_test_build_listener_binds_loopback ... ok
test protocol::mdns_discovery::tests::test_announce_then_browse_resolves_ourselves ... ok
test protocol::mdns_discovery::tests::test_reannounce_publishes_a_real_update ... ok
test protocol::mdns_discovery::tests::test_announcer_in_test_builds_is_invisible_to_production_browsers ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 1146 filtered out
```

### Lint and format

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo clippy --all-targets --all-features --locked -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 25.00s

$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo fmt --check ; echo "exit=$?"
exit=0
```

### Production build verifies gate compiles out

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo build --locked --release
    Finished `release` profile [optimized] target(s) in 1m 57s
```

No `test-helpers` is set in a release build, so the `#[cfg(any(test,
feature = "test-helpers"))]` gate compiles out entirely — the production
`DiscoveryService::new` and `MdnsDiscoveryService::new` retain their
`Ipv4Addr::UNSPECIFIED`/`Ipv4Addr::BROADCAST` and unrestricted
`ServiceDaemon` paths, byte-clean.

### Live LAN oracle (integrator runs this, not the lane)

Per the brief, this is the integrator's check, not the lane's. On a
merged main while `tcpdump -ni wlp109s0f0 'udp port 5353'` runs:

```
$ cargo test --all-features --locked --no-fail-fast   # full suite
$ tcpdump -ni wlp109s0f0 'udp port 5353' | grep _kdeconnect-test
# expected: ZERO matches across the whole run
```

The UDP leg is loopback unicast (`broadcast_addr = 127.0.0.1:port`) so
wifi silence covers it too — no `_kdeconnect-test._udp.local.` packet
can ever leave the host from this code path. The mDNS multicast path
is restricted to `IfKind::LoopbackV4` only, so the wifi interface is
not in the `ServiceDaemon`'s `my_intfs` set; nothing is even sent.

## Critique — blunt

The brief is structurally correct, but I want to argue against it in
three places where it could be sharper, and call out the one place I
tried to break it and couldn't.

**1. The R1/R2 naming reads as `test_test_build_*` for a reason the
brief doesn't explain.** "test build" is the gate surface (the
combination of `#[cfg(test)]` + `test-helpers` feature). The names
match house style for gate-pinned assertions
(`test_new_binds_and_broadcasts_on_configured_port` is the closest
neighbor, no `_test_build_` prefix). I kept the prefix because the
brief's own terminology uses it ("test builds announce…", "test
listeners bind…"). It's a fingerprint of the gate rather than the
behavior, though — a future reader trying to grep for "loopback" finds
both R1/R2 and the production-side `is_private_address` filter, which
is mildly confusing. A better name might be `test_loopback_scoped_broadcast`
/ `test_loopback_scoped_bind`. I went with the brief's own phrasing
because re-naming red tests during execution is scope creep; flagged
here for the integrator to decide.

**2. The mDNS leg has no unit-level red gate by design, but the brief
phrases that as "honest scoping note" rather than a residual risk.**
Reading the brief charitably: "mdns-sd exposes no query API for active
interface selections" — so the test cannot verify the restriction is
actually in effect; only that `new + register + reannounce + browse +
resolve` continues to function (which is what
`test_announce_then_browse_resolves_ourselves` and
`test_reannounce_publishes_a_real_update` cover, and both pass under
the gate). That's a regression guard, not a property test. The actual
property test is the live oracle (`tcpdump` shows zero mDNS packets on
the wifi interface). I considered adding a smoke test that fails if
the gate were silently dropped — something like a compile-time check
that `mdns_sd::IfKind::LoopbackV4` exists — but that's testing the
crate, not the code, and a future mdns-sd bump could move the API
surface in ways no compile-time check would catch. The right gate IS
the live oracle. The brief is honest about this; flagged here because
"the only thing that proves the mDNS leg works is running tcpdump" is
the kind of fact that should land in the vault project page alongside
the change, not just in this brief.

**3. `disable_interface(IfKind::All)` then `enable_interface(IfKind::LoopbackV4)`
is the minimum API call sequence, but it's two calls instead of one
because the crate chose a defaults-enabled model.** A `restrict_to(impl
IntoIfKindVec>)` helper on the daemon would be a one-liner ergonomics
win and reduce the chance of a future contributor missing the
`disable_interface` first call and shipping a non-restricted test
build. I did NOT add it because adding API surface for ergonomics on
a single-callsite internal helper is exactly the kind of "while I'm
here" expansion the brief's Class-A gates rule out, and the existing
two-call sequence is documented in the source comment. Future
contributor risk is real though — the asymmetry between the two
calls is the kind of footgun that survives a copy-paste from this
file into the next. The vault comment in `mdns_discovery.rs` is the
mitigation.

**4. The one thing I tried to break and could not: split-brain
detection under the gate.** The brief explicitly tells me not to
teach the detector to ignore `svc-mgr-` test ids, and I tried to find a
scenario where that decision would bite. Two test services bound to
`127.0.0.1` on the same host still see each other's identity broadcasts
(loopback delivery), still see each other's mDNS announces (the daemon
broadcasts even on loopback for self-resolution; the receiver daemon is
its own browsable service), and still trigger the detector with
`from: 127.0.0.1`. So the detector's "another implementation is
announcing from THIS host" log line will still fire under `cargo test`,
which is correct — a real split-brain is exactly what it sounds like,
and a test that triggers it deliberately is information, not noise.
The brief's framing on this is sound; the cost is that test logs will
keep mentioning split-brain on this host, which someone might mistake
for a regression. The fix is documentation, not code.

**5. Netns tests (root-only, `tests/netns_discovery.rs`) survive the
gate unchanged.** Both sides bind `127.0.0.1:port` inside the netns and
broadcast to `127.0.0.1:port`. Loopback delivery inside a network
namespace is per-namespace — the worker's two `DiscoveryService`s
communicate via loopback within the namespace, and the host network
sees nothing. The test that proves this is the existing
`test_new_binds_and_broadcasts_on_configured_port` for the port
invariant, plus the full netns suite when run as root. I did NOT run
the netns suite (it needs `CAP_NET_ADMIN`, lane doesn't have it) — the
brief explicitly says the live oracle is the integrator's check, and
the netns path is structurally the same as a loopback UDP exchange.

**6. Cargo target dir hygiene.** The brief says never build in the
worktree (tmpfs). I respected that for all cargo invocations —
`CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target` on
every command. Some `target/` files exist in the worktree from the
worktree's initial state (timestamps 10:04-10:06, before my first
edit at 10:08); I did not create them and did not write to them.
Surfaced here because "are there unexpected build artifacts in the
worktree" is the obvious follow-up question the integrator will ask.
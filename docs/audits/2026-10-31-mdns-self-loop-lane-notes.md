# Lane notes — mDNS self-loop storm (fix-mdns-self-loopback)

## What changed

**`src/protocol/mdns_discovery.rs`**

- New private helper `disable_multicast_loopback(&ServiceDaemon) -> Result<()>`
  (`:301`). It calls `set_multicast_loop_v4(false)` and
  `set_multicast_loop_v6(false)` and maps each failure into
  `Error::DiscoveryError`, the way the surrounding mdns-sd error mapping
  already does it.
- `MdnsDiscoveryService::new` calls it immediately after
  `ServiceDaemon::new(...)` and **before** `register` (`:322`). The daemon
  processes commands in order, so the socket option is in force before the
  first announcement goes out. Probing is untouched, no interface
  allowlist was added, `reannounce` is untouched.

**`src/services/service_manager.rs`**

- The `"Discovered device via mDNS"` info log moved from the top of
  `on_mdns_device_resolved_with_udp_port` to below the self-identity
  guard. Same structured fields (`device_id`, `device_name`, `address`,
  `srv_port`, `protocol_version`, `event`). A self-resolve that still
  slips through is no longer logged as a discovered device.

## The measurement that drove the test design

`IP_MULTICAST_LOOP` is a **send-path** option on Linux
(`ip_mc_output` → `ip_dev_loopback_xmit`). The loopback **device**,
however, hands every packet sent on it to every local member of the
multicast group, and that delivery is not governed by the option. So the
loopback interface the rest of this suite uses cannot exhibit the fix at
all.

Measured 2026-10-31 on this host (linux 7.2.12) with mdns-sd 0.20.3,
daemon pinned to one interface, self-resolves in a 6s window:

```
lo       loop ON  -> self_resolves=1     lo       loop OFF -> self_resolves=1
wlan     loop ON  -> self_resolves=1     wlan     loop OFF -> self_resolves=0
```

Same result at the raw-socket level (`IP_MULTICAST_LOOP` on/off, own
packet vs. the live daemon's packets on the same group):

```
lo       loop=ON  -> saw_own_packet=True    lo       loop=OFF -> saw_own_packet=True
docker-br loop=ON -> saw_own_packet=True    docker-br loop=OFF -> saw_own_packet=False
wlan     loop=ON  -> saw_own_packet=True    wlan     loop=OFF -> saw_own_packet=False
```

Consequence: a test written on `lo` would pass with or without the fix.
The new test therefore runs on a **non-loopback** interface.

## The test that was red before and green after

`test_announcer_does_not_resolve_itself` (`:1215`) — same daemon, same
interface, the only difference being `IP_MULTICAST_LOOP`.

Command:

```
cargo test --offline --lib protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself
```

**RED** — against the unchanged behaviour (loopback left on, i.e. the
old `new`):

```
running 1 test
test protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself ... FAILED

---- protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself stdout ----

thread 'protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself' panicked at src/protocol/mdns_discovery.rs:1234:9:
our own announcement came back to us on docker0 even with multicast loopback disabled: resolved [("sl-05934374ca8d44de83d1e3c1404bdb2b._kdeconnect-test._udp.local.", "sl-05934374ca8d44de83d1e3c1404bdb2b")]

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1238 filtered out; finished in 10.05s
```

**GREEN** — with the fix applied:

```
running 1 test
test protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1238 filtered out; finished in 10.03s
```

The control arm
`test_multicast_loopback_carries_our_own_announcement_back` (`:1261`)
runs the same setup with the option **on** and asserts the self-resolve
is still there. It is what stops the subject test from becoming a
vacuous green: if the control ever stops seeing the self-resolve, the
subject test has stopped proving anything. It also carries the
announce-shape coverage the deleted test had (instance = device id, SRV
port, TXT records).

## Tests deleted, and why

- **`test_announce_then_browse_resolves_ourselves` — deleted.** It
  asserted that our own announcement comes back to our own browser, which
  is the bug. Its coverage (the announce → browse round trip, the
  reference shape) moved into the control arm above, which observes the
  same round trip on a real interface.

## Tests kept, and why

- **`test_reannounce_publishes_a_real_update` — kept.** It proves
  `reannounce` publishes a new name by watching our own announcement come
  back, so it does depend on loopback delivery. It still passes, and it
  does not re-enable the loopback option to do it: on `lo` the device
  delivers our packets back regardless of `IP_MULTICAST_LOOP`. The
  caveat is recorded in FINDINGS.md — that observation channel is one
  production no longer has on a real interface.
- **`test_announcer_in_test_builds_is_invisible_to_production_browsers`
  — kept.** It browses the *production* service type while the fixture
  announces under the test-only type, so it never depended on loopback
  delivery. Unaffected by the fix.

## Interface handling in the new test

The test's daemon is pinned to one non-loopback interface, chosen from
`/proc/net/dev` (`:1067`): host-local bridges first (`docker*`, `br-*`,
`veth*`, `virbr*`), then tunnels (`tailscale*`, `tun*`, `tap*`, `wg*`,
`zt*`), then physical NICs. On this host it lands on `docker0`, so the
test's multicast never reaches the physical LAN. The service type is the
test-only `_kdeconnect-test._udp.local.`, so no production peer reacts to
it either. If the host has no usable non-loopback interface the test
prints a visible skip line and returns, following the convention in
`tests/netns_discovery.rs`.

## Final `cargo test` summary line

```
test result: ok. 1239 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.67s
```

All 39 test binaries green. `cargo fmt --check` clean, `cargo clippy
--all-targets` clean (no new warnings).

---

## Fix-ups after review (2026-10-31)

The adversarial review found the loopback-option fix incomplete for
production. Three things came back: the daemon still hears itself on
`lo` in production builds (finding 1, HIGH); the impact claim in
FINDINGS.md was wrong about the option alone (finding 3, MEDIUM); and
the SKIP branch of the new interface-pinned tests was quiet (finding
2, MEDIUM). Finding 4 (mdns-sd's internal `.unwrap()` on setsockopt) is
recorded below as a known crate limitation; no code change.

### Finding 1 — exclude the loopback interface in production

**`src/protocol/mdns_discovery.rs`**

- New private pure function `production_interface_policy() -> &'static
  [IfKind]` (`:341`) over a new private
  `static PRODUCTION_INTERFACE_POLICY: [IfKind; 2] =
  [IfKind::LoopbackV4, IfKind::LoopbackV6]`.
- `MdnsDiscoveryService::new` applies that policy — one
  `daemon.disable_interface(kind)` per kind, each failure mapped into
  `Error::DiscoveryError` the way the loopback helper does it (`:374`).
  It runs **before** `register`, so the exclusion is in force before
  the first announcement.
- The doc comment on `disable_multicast_loopback` no longer claims the
  option alone stops our packets reaching our socket. It now says the
  option covers the REAL interfaces and the interface exclusion covers
  `lo`.

**Why the option alone was not enough** (the review's evidence, re-read
against vendored mdns-sd 0.20.3): the daemon enumerates every
interface including the loopback (`my_ip_interfaces_inner(true, ...)`,
`service_daemon.rs:4467`, loopback kept at `:4474`), announces
per interface (`:2152` iterates `self.my_intfs`), and re-applies the
interface selections on every interface refresh (`:1916`). `IP_MULTICAST_LOOP`
is a send-path option; the loopback DEVICE hands every packet sent on
it to every local member of the group regardless of it. The lane's own
measurement already showed this: `lo loop OFF -> self_resolves=1`.

### The ordering claim, confirmed twice

The brief asks for the ordering claim to be confirmed against
mdns-sd's `apply_intf_selections` and the line stated. Here it is:

`service_daemon.rs:1777-1790`

```rust
fn apply_intf_selections(&mut self, interfaces: Vec<Interface>) {
    // By default, we enable all interfaces.
    let intf_count = interfaces.len();
    let mut intf_selections = vec![true; intf_count];

    // apply if_selections
    for selection in self.if_selections.iter() {
        // Mark the interfaces for this selection.
        for i in 0..intf_count {
            if selection.if_kind.matches(&interfaces[i]) {
                intf_selections[i] = selection.selected;   // :1787 — LAST match wins
            }
        }
    }
    ...
```

`if_selections` is a `Vec` the daemon appends to, in the order the
commands arrive (`enable_interface` pushes at `:1648`,
`disable_interface` at `:1662`), and the command channel is FIFO. So
the selections are applied in call order and the last match for an
interface wins.

The production `new` therefore calls, in order:

1. `disable_interface(LoopbackV4)` — production policy
2. `disable_interface(LoopbackV6)` — production policy
3. `disable_interface(All)` — test-cfg block only
4. `enable_interface(LoopbackV4)` — test-cfg block only

For the IPv4 loopback address: (1) false, (3) false, (4) true. Final:
**enabled** in test builds, **disabled** in production builds (where
(3) and (4) do not exist).

**Confirmed empirically, not just by reading.** A temporary test (run,
recorded here, not committed) registered a service and read the
`DaemonEvent::Announce` event's interface list:

```
TEMP production policy only: announced on ["laptop.local.:enp0s20f0u6u3u4",
  "laptop.local.:docker0", "laptop.local.:vethe77d646", "laptop.local.:wlp109s0f0",
  ... ]                                  <- no `lo`
TEMP policy + test-cfg block: announced on ["laptop.local.:lo", "laptop.local.:lo"]
```

Production announces on the real interfaces and not on `lo`. The
test-cfg block overrides the production exclusion and puts the daemon
back on `lo` only. Test builds keep mDNS on the loopback and off the
LAN, exactly as before the fix-up.

### The host-independent guard

`test_production_interface_policy_excludes_loopback` (`:1031`) asserts
the policy disables both loopback kinds and does not disable
`IfKind::All` or a whole address family. It needs no network, no
interface and no daemon.

**RED** — with the policy deliberately broken (loopback not excluded):

```
running 1 test
test protocol::mdns_discovery::tests::test_production_interface_policy_excludes_loopback ... FAILED

---- protocol::mdns_discovery::tests::test_production_interface_policy_excludes_loopback stdout ----

thread 'protocol::mdns_discovery::tests::test_production_interface_policy_excludes_loopback' (566385) panicked at src/protocol/mdns_discovery.rs:1035:9:
the production interface policy must exclude the IPv4 loopback: []
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

failures:
    protocol::mdns_discovery::tests::test_production_interface_policy_excludes_loopback

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1239 filtered out; finished in 0.00s
```

The same test also fails on a policy that excludes everything
(`[All]`, "must exclude the IPv4 loopback: [All]") and on a policy
that adds `All` to the two loopback kinds ("must not exclude every
interface: [LoopbackV4, LoopbackV6, All]").

**GREEN** — with the policy restored:

```
running 1 test
test protocol::mdns_discovery::tests::test_production_interface_policy_excludes_loopback ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1239 filtered out; finished in 0.00s
```

### Finding 2 — the SKIP branch, made loud

Both interface-pinned tests now print the interfaces they found and the
reason, via `eprintln!`, instead of a bare "this host has no
non-loopback interface":

```
SKIP test_announcer_does_not_resolve_itself: no non-loopback interface
the mDNS daemon can announce on. Interfaces found on this host: [...].
Reason: the loopback interface cannot exhibit the fix (Linux loops
multicast back on `lo` regardless of IP_MULTICAST_LOOP), so the
measurement needs a real interface. A host with no bridge and no
physical NIC therefore gets NO coverage from this test — the
host-independent guard is
test_production_interface_policy_excludes_loopback.
```

**CI hosts without a bridge get NO coverage from those two tests.** The
SKIP stays — putting the test's multicast on a physical LAN is not
acceptable. The host-independent guard is
`test_production_interface_policy_excludes_loopback`, which runs
everywhere and fails if someone re-includes `lo`.

### A pre-existing flake, found on the way (not in the brief)

`test_announcer_does_not_resolve_itself` failed at random: four runs in
five on this host. **It also fails on the pre-fix tree** — the same
five-run sweep against `HEAD` (33686a5) gave one failure in five, so
this is not something the loopback exclusion introduced.

Cause: mdns-sd reports a name change for the HOSTNAME as well as for
the service instance, and the control arm of the same test runs
concurrently on the same interface with multicast loopback ON. Its
daemon claims `laptop.local.`; the subject daemon probes that same
hostname, gets an answer, and renames itself to `laptop-2.local.` — a
conflict with a sibling test daemon, not our own answer coming back
to us. The probe now keeps the two apart: `renamed` holds the INSTANCE
renames (the `(N)` suffix runaway the live daemon showed), and
`host_renamed` holds the hostname ones. The probe also waits for the
daemon thread to exit before returning — `shutdown()` only queues the
exit, so the sockets stayed open until `cleanup()` ran, and a daemon
still lingering on the interface answers the next probe's hostname
question.

Six consecutive runs of the mdns module, green:

```
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.04s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.03s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.04s
```

The subject test still detects the bug. With its arm forced back to
the pre-fix behaviour (multicast loopback ON):

```
thread 'protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself' (602411) panicked at src/protocol/mdns_discovery.rs:1376:9:
our own announcement came back to us on vethe77d646 even with multicast loopback disabled: resolved [("sl-2f8b971eb4c3449f969dddfe53cc01ab._kdeconnect-test._udp.local.", "sl-2f8b971eb4c3449f969dddfe53cc01ab")]

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1239 filtered out; finished in 10.02s
```

### Finding 4 — known crate limitation, no code change

mdns-sd's own `setsockopt` path unwraps internally (`service_daemon.rs`
`set_multicast_loop_v4` / `set_multicast_loop_v6`), so a kernel that
refuses the option panics the daemon thread instead of returning an
error to us. Nothing in this crate can catch that. Recorded here as
a limitation to live with, not a bug to fix here.

### Gates

```
$ cargo fmt --check
(clean)

$ cargo clippy --offline --all-targets
(no warnings, no errors)

$ cargo test --offline
test result: ok. 1240 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.73s
```

All 39 test binaries green. The full suite was run three times in a
row (1240 passed, 0 failed each time).

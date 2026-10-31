# FINDINGS — mDNS self-loop storm (fix-mdns-self-loopback)

Two passes: the original lane (loopback option) and the fix-ups after
the adversarial review (2026-10-31). Both are recorded here.

## What changed

### The original lane

**`src/protocol/mdns_discovery.rs`**

- New private helper `disable_multicast_loopback(&ServiceDaemon) -> Result<()>`
  (`:308`): `set_multicast_loop_v4(false)` + `set_multicast_loop_v6(false)`,
  each failure mapped into `Error::DiscoveryError` the way the surrounding
  mdns-sd error mapping already does it.
- `MdnsDiscoveryService::new` calls it right after `ServiceDaemon::new(...)`
  and before `register`.
- Probing untouched, no interface allowlist, `reannounce` untouched, the
  mdns-sd crate and `Cargo.toml` untouched.

**`src/services/service_manager.rs`**

- The `"Discovered device via mDNS"` info log moved below the
  self-identity guard in `on_mdns_device_resolved_with_udp_port`. Same
  structured fields.

**Tests (`src/protocol/mdns_discovery.rs`)**

- Deleted `test_announce_then_browse_resolves_ourselves` (it asserted the
  bug: our own announcement coming back to our own browser).
- Added `test_announcer_does_not_resolve_itself` and the control arm
  `test_multicast_loopback_carries_our_own_announcement_back`, plus the
  probe helpers they share.

### The fix-ups after review (2026-10-31)

**`src/protocol/mdns_discovery.rs`**

- New private pure function `production_interface_policy() -> &'static
  [IfKind]` (`:341`) over a new private
  `static PRODUCTION_INTERFACE_POLICY: [IfKind; 2] =
  [IfKind::LoopbackV4, IfKind::LoopbackV6]` (`:331`).
- `MdnsDiscoveryService::new` applies that policy — one
  `daemon.disable_interface(kind)` per kind, each failure mapped into
  `Error::DiscoveryError` (`:374`). It runs BEFORE `register`, so the
  exclusion is in force before the first announcement, and BEFORE the
  test-cfg interface-pin block, which re-enables the loopback for test
  builds (mdns-sd applies interface selections in order and the last
  match wins — `service_daemon.rs:1783-1790`, `apply_intf_selections`).
- The doc comment on `disable_multicast_loopback` no longer claims the
  option alone stops our packets reaching our socket: the option covers
  the REAL interfaces, the interface exclusion covers `lo`.
- New unit test `test_production_interface_policy_excludes_loopback`
  (`:1031`): the host-independent guard. The policy must disable both
  loopback kinds and must not disable `IfKind::All` or a whole address
  family.
- The self-loop probe now counts only OUR INSTANCE's renames in
  `renamed` and keeps hostname renames ("laptop.local." ->
  "laptop-2.local.") in a separate `host_renamed`, and waits for the
  daemon thread to exit before returning. Both are the fix for a
  pre-existing flake (details below).
- Both interface-pinned tests now `eprintln!` the interfaces they found
  and the reason when they SKIP.

**Not changed:** `Cargo.toml`, the mdns-sd crate, `service_manager.rs`,
probing, `reannounce`, the service type, and any circuit breaker.

## How it was verified

### The claim's own scenario, failing BEFORE

The original lane's subject test against the unchanged behaviour —
`IP_MULTICAST_LOOP` left on, which is what the old `new` had:

```
$ cargo test --offline --lib protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself
running 1 test
test protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself ... FAILED

---- protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself stdout ----

thread 'protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself' panicked at src/protocol/mdns_discovery.rs:1234:9:
our own announcement came back to us on docker0 even with multicast loopback disabled: resolved [("sl-05934374ca8d44de83d1e3c1404bdb2b._kdeconnect-test._udp.local.", "sl-05934374ca8d44de83d1e3c1404bdb2b")]

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1238 filtered out; finished in 10.05s
```

### The same scenario, passing AFTER

```
$ cargo test --offline --lib protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself
running 1 test
test protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1238 filtered out; finished in 10.03s
```

And the control arm, which must still see the self-resolve, so the
subject test cannot pass vacuously:

```
$ cargo test --offline --lib protocol::mdns_discovery::tests::test_multicast_loopback_carries_our_own_announcement_back
test protocol::mdns_discovery::tests::test_multicast_loopback_carries_our_own_announcement_back ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1238 filtered out; finished in 10.05s
```

Re-verified after the fix-ups, by forcing the subject arm back to the
pre-fix behaviour (multicast loopback ON in that arm only):

```
thread 'protocol::mdns_discovery::tests::test_announcer_does_not_resolve_itself' (602411) panicked at src/protocol/mdns_discovery.rs:1376:9:
our own announcement came back to us on vethe77d646 even with multicast loopback disabled: resolved [("sl-2f8b971eb4c3449f969dddfe53cc01ab._kdeconnect-test._udp.local.", "sl-2f8b971eb4c3449f969dddfe53cc01ab")]

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1239 filtered out; finished in 10.02s
```

### The review's finding 1 — the production interface policy

The new host-independent guard, RED with the policy deliberately broken
(loopback not excluded):

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

The same test also fails on a policy of `[All]` ("must exclude the IPv4
loopback: [All]") and on `[LoopbackV4, LoopbackV6, All]` ("must not
exclude every interface: [LoopbackV4, LoopbackV6, All]").

GREEN with the policy restored:

```
running 1 test
test protocol::mdns_discovery::tests::test_production_interface_policy_excludes_loopback ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1239 filtered out; finished in 0.00s
```

### The ordering claim, confirmed against mdns-sd's source

`service_daemon.rs:1777-1790`:

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
```

`if_selections` is appended in command-arrival order (`enable_interface`
pushes at `:1648`, `disable_interface` at `:1662`) and the command channel
is FIFO, so the production exclusion is applied before the test-cfg
block's `enable_interface(LoopbackV4)` and the test build still wins.

Confirmed empirically too, not just by reading: a temporary test (run and
recorded in LANE-NOTES, not committed) registered a service and read the
announce event's interface list.

```
TEMP production policy only: announced on ["laptop.local.:enp0s20f0u6u3u4",
  "laptop.local.:docker0", "laptop.local.:vethe77d646", "laptop.local.:wlp109s0f0",
  ... ]                                  <- no `lo`
TEMP policy + test-cfg block: announced on ["laptop.local.:lo", "laptop.local.:lo"]
```

### The measurement the test design rests on

`IP_MULTICAST_LOOP` is a send-path option on Linux
(`ip_mc_output` → `ip_dev_loopback_xmit`); the loopback *device* hands
every packet sent on it to every local member of the multicast group
regardless. Measured 2026-10-31 (linux 7.2.12), mdns-sd 0.20.3, daemon
pinned to one interface, self-resolves in a 6s window:

```
lo       loop ON  -> self_resolves=1     lo       loop OFF -> self_resolves=1
wlan     loop ON  -> self_resolves=1     wlan     loop OFF -> self_resolves=0
```

Same at the raw-socket level (own packet vs. the live daemon's packets on
the same group):

```
lo       loop=ON  -> saw_own_packet=True    lo       loop=OFF -> saw_own_packet=True
docker-br loop=ON -> saw_own_packet=True    docker-br loop=OFF -> saw_own_packet=False
wlan     loop=ON  -> saw_own_packet=True    wlan     loop=OFF -> saw_own_packet=False
```

The `lo` row is the whole reason the production daemon now excludes the
loopback interface: the option cannot close that leg, because the option
is not what governs it.

### A pre-existing flake, and what was done about it

`test_announcer_does_not_resolve_itself` failed at random — four runs in
five on this host. The same five-run sweep against the pre-fix tree
(33686a5) gave one failure in five, so the flake predates the fix-ups:

```
$ git show HEAD:src/protocol/mdns_discovery.rs > src/protocol/mdns_discovery.rs   # pre-fix tree
$ cargo test --offline --lib protocol::mdns_discovery::
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.04s
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.03s
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
mdns-sd renamed our instance after reading our own answer as a conflict: ["laptop-2.local.", "laptop-2.local."]
test result: FAILED. 15 passed; 1 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
```

Cause: mdns-sd reports a name change for the HOSTNAME as well as for the
service instance, and the control arm of the same test runs concurrently
on the same interface with multicast loopback ON. Its daemon claims
`laptop.local.`; the subject daemon probes that same hostname, gets an
answer, and renames itself to `laptop-2.local.` — a conflict with a
sibling test daemon, not our own answer coming back to us.

The fix separates the two (`renamed` for instance renames, `host_renamed`
for hostname ones) and makes the probe wait for the daemon thread to exit
before returning, so a daemon lingering on the interface cannot answer the
next probe's hostname question. Six consecutive mdns-module runs, green:

```
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.04s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.05s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.03s
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1223 filtered out; finished in 10.04s
```

### Gates

```
$ cargo fmt --check
(clean)

$ cargo clippy --offline --all-targets
(no warnings, no errors)

$ cargo test --offline
test result: ok. 1240 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.73s
```

All 39 test binaries green. The full suite was run three times in a row,
1240 passed and 0 failed each time.

## Critique — blunt

**The original lane's fix was wrong about where the self-echo came
from.** `IP_MULTICAST_LOOP` is a send-path option, and the review
established that the daemon enumerates every interface including `lo`,
announces on each, and re-applies the interface selections on every
refresh. On the loopback DEVICE the option is not what governs
delivery, so the daemon went on hearing itself in production while the
moved log line hid it. The lane's own measurement had the answer in it
(`lo loop OFF -> self_resolves=1`) and the fix did not read it as the
finding it was. A green test on `docker0` was read as "the self-loop is
gone" when it only said "the self-loop is gone on interfaces where the
option governs delivery."

**The impact claim in the first FINDINGS.md was wrong, and in the
direction that flattered the fix.** It said two patched instances on one
host are mutually deaf on the mDNS leg *because of the loopback option*.
That is not true of the option alone: the `lo` device delivers regardless,
so before the interface exclusion the two daemons still heard each other.
The claim becomes TRUE once `lo` is excluded — and taking the
same-host rust-connect-versus-rust-connect case away from the
split-brain detector is a consequence of THIS fix-up, not of the original
one. The trade-off is accepted: one daemon per host is the deployment
shape, the UDP broadcast leg still feeds the detector while mDNS is
unhealthy and nothing is connected, and any avahi or kdeconnect-kde
instance on the host still trips the mDNS leg because its own socket
keeps loopback on. What is lost is exactly the rust-connect-versus-
rust-connect case, which was previously caught by accident: the two
daemons heard each other only because they were hearing their own
echoes. A detector fed by a bug is a detector that stops working when
the bug is fixed.

**The instance-rename assertion was vacuous, and it was vacuous in the
lane's own favour.** mdns-sd renames the HOSTNAME ("laptop.local." ->
"laptop-2.local.") when it hears a conflicting claim; the "(N)" suffix
on the service instance is a different mechanism. Measured here: with
multicast loopback ON the control daemon renames its hostname and its
instance name carries no "(N)". So `probe.renamed.is_empty()` never
caught the bug it was written for — it caught foreign hostname claims
instead, which is how the test ended up failing at random. The fix-ups
split the two counters, and the instance-level assertion is now a guard
that does not fire in this environment rather than a detector. The real
detector is the self-resolve assertion, and that one is red-before /
green-after.

**Nothing measures the storm.** `sample_metrics`' warn threshold is
untouched and no circuit breaker was added, which is defensible, but
nothing here shows the symptom went away. The fix is verified by "we no
longer hear ourselves" — the mechanism. The 50 packets/second figure,
the 600 self-resolves a minute, the 516,000 suffix: none of them have
been re-measured on a live daemon. This lane changed a socket option
and an interface list and watched tests flip.

**The loopback exclusion has a cost nobody has measured.** On these
hosts nothing else listens to mDNS on `lo`, so excluding it is free —
today. A daemon that published on `lo` for a local consumer (a
same-host service, a test fixture, a future local bridge) would now be
invisible. The policy is one line to widen, but the choice was made on
the evidence of "nothing else on these hosts", which is an observation
about today rather than a property of the code.

**`reannounce` still has no observation channel in production.** The
surviving test `test_reannounce_publishes_a_real_update` proves
`reannounce` publishes a new name by watching our own announcement come
back. It passes on `lo`, where the device delivers our packets back
whatever the option says — a reason production does not share, now
doubly so, since production no longer listens on `lo` at all. On a real
interface, after this fix, our own announcement is not observable to us
any more; "a peer sees the new name" needs a second host. The test will
keep passing long after it stops meaning anything.

**Platform semantics are still unverified.** mdns-sd's own doc comment
for these setters quotes Winsock: `IP_MULTICAST_LOOP` applies to the
receive path on Windows and the send path on Unix. The daemon is
responder and querier at once, so setting it on our one daemon covers
both roles on either platform — in theory. Nobody has run this on
Windows, and the suite here is Linux-only. The interface exclusion is
platform-independent and helps on both, but it is untested away from
Linux.

**mdns-sd's internal `.unwrap()` on setsockopt is a live crash risk.**
The crate unwraps the `setsockopt` result internally, so a kernel that
refuses `IP_MULTICAST_LOOP` panics the daemon thread instead of
returning an error we could handle. No code change here; recorded in
LANE-NOTES as a known limitation of the dependency.

**The two interface-pinned tests put the test suite's multicast on a
real interface, and that remains a departure.** The D2 posture
(2026-09-02) keeps test multicast off the LAN. The bridge-first
interface selection keeps it on a host-local bridge on this host, but a
host whose only non-loopback interface is a physical NIC puts test
multicast on the wired LAN. The service type is test-only, so no
production peer reacts, but the packets are still on the wire. A CI
host without a bridge gets no coverage from those two tests at all, and
the host-independent guard is now the policy unit test rather than a
measurement.

**What I tried to break and could not.** The central claim — that
turning the option off stops our own announcement reaching our own
socket on a real interface — held under three independent measurements
(raw socket, mdns-sd directly, and this crate's own test), and the
ordering assumption held too: mdns-sd's command channel is FIFO, so a
`SetOption` sent before `Register` is in force before the first
announcement goes out, and `apply_intf_selections` applies selections in
arrival order with the last match winning — confirmed by reading
`service_daemon.rs:1783-1790` and, independently, by watching a
production-policy daemon announce on four real interfaces and not on
`lo`, while the same daemon with the test-cfg block announced on `lo`
alone. The blast radius is one socket option and two interface
exclusions on one daemon: receiving other hosts' announcements is
untouched, probing is untouched, and the production interface set loses
only the loopback.

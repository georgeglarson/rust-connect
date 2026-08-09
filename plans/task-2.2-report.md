# Task 2.2 report — event-driven, roaming-safe discovery (vk #994)

Worktree: `~/repos/rust-connect-feat-task-2.2`, branch `feat-task-2.2-event-discovery`, off `127d9c9`.
Four commits, one per piece, in the brief's order:

| Piece | SHA | Summary |
|---|---|---|
| 1. Network-change watcher | `92736cf` | `services::network_watcher` — if-watch address-change detection + suspend/resume watchdog, debounced |
| 2. Re-announce on change | `4d93ec3` | `services::discovery_coordinator` — UDP broadcast + mDNS reannounce on the debounced event |
| 3. Remove broadcast-forever | `314e093` | `services::broadcast_fallback` — announce on start/change only, bounded mDNS-down backoff |
| 4. Netns test suite | `7d02a95` | `tests/netns_discovery.rs` — root-only, on-demand, real netlink/socket coverage |

## Dependency justification (piece 1)

Evaluated three options for interface/address-change watching on Linux:

- **Hand-rolled raw netlink** — reinvents parsing/decoding a maintained crate already does correctly. Rejected.
- **`rtnetlink`** — full netlink route-*manipulation* crate; this daemon only needs to *watch*, a bigger API surface than needed. Rejected.
- **`if-watch`** (chosen) — purpose-built for exactly this, used by libp2p and others, tokio-native (no competing async runtime enters the tree). Wraps `rtnetlink` internally on Linux, so picking it directly costs nothing over what a raw-netlink implementation would ALSO have needed (`netlink-packet-core`, `netlink-packet-route`, `netlink-proto`, `netlink-sys`).

Marginal dependency-tree cost: `if-watch`, `netlink-packet-core`, `netlink-packet-route`, `netlink-proto`, `netlink-sys`, `rtnetlink`, `paste` — 7 new crates. `default-features = false` on the `if-watch` entry to avoid pulling the Windows backend's transitive deps (`windows-threading` et al.), which are outside this crate's build targets and were causing a spurious license-check concern in a throwaway research spike before the flag was added.

`cargo deny check` became a required gate from piece 1 onward per the repo's PR checklist (Cargo.toml changed). One new advisory surfaced: `paste` (transitive via `netlink-packet-core`) triggers RUSTSEC-2024-0436 (unmaintained). Added to `deny.toml`'s `[advisories].ignore` with a documented justification (build-time proc-macro codegen only, never in the runtime attack surface; advisory's own text says "no safe upgrade is available"; a suggested-replacement fork `pastey` exists for future re-evaluation).

## Debounce + fallback policy — before/after cadence

| Scenario | Before (main @ 127d9c9) | After (this branch) |
|---|---|---|
| Healthy host (mDNS up, reachable) | UDP broadcast every 60s, forever, unconditionally | UDP broadcast on start + on debounced network change only — matches both references |
| mDNS down, ≥1 device connected | Same 60s-forever | No periodic broadcast (device already reachable via the connected channel) |
| mDNS down, no device connected | Same 60s-forever | Backoff: 5s, 10s, 20s, 40s, 80s, 160s, capped at 300s — resets to 5s the moment mDNS recovers or a device connects |
| Interface/address change | No reaction at all (the `mdns_discovery.rs:84` TODO) | Debounced (750ms trailing-edge) → UDP broadcast once + mDNS reannounce |
| Suspend/resume | No reaction | Detected via CLOCK_BOOTTIME vs CLOCK_MONOTONIC gap (>5s slack) → same debounced reaction as an address change |

Debounce window (750ms) and the suspend-watchdog constants are **our own evidence-based picks, not upstream's**: kde's own "debounce" (`lanlinkprovider.cpp:73-75`) is a 0ms singleshot QTimer that coalesces same-Qt-event-loop-tick signal emissions, which doesn't transfer to netlink delivering a real transition as several separate, asynchronously-arriving messages over tens-to-hundreds of milliseconds. Documented in `network_watcher.rs`'s module doc, not silently substituted as if it were a citation.

Upstream cites for the cadence policy: kdeconnect-kde `lanlinkprovider.cpp:149,192`; kdeconnect-android `LanLinkProvider.java:567,572-584` (announce on start + network change, no periodic timer). The bounded mDNS-down fallback is our own **documented divergence** — upstream can assume avahi is always present on the OSes it ships on; this daemon runs on hosts where mDNS can be genuinely absent (no multicast, a died daemon, a sandboxed environment).

Full citations and the parity-checklist.md rows this closes are in the piece 2/3 commit messages and `docs/parity-checklist.md` (Discovery table's "Broadcast cadence" and "Immediate re-broadcast on network change" rows, both now CONFORMANT; former gaps 1 and 5 removed).

## if-watch coverage — honest limitation, empirically grounded

`if_watch::tokio::IfWatcher` subscribes to `RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR` (verified in the vendored source) — address changes only, not `RTMGRP_LINK` (raw link up/down) or `RTMGRP_IPV4_ROUTE` (default-route changes). This session **empirically verified** that a pure `ip link set dev down` still fires the watcher in practice: bringing a Linux interface down removes its IPv6 link-local address entirely, and bringing it back up re-adds it — that address churn, not any direct link-state subscription, is the actual mechanism (confirmed live via `ip netns` + `ip -6 addr show` before writing the netns test that exercises it). This is stated as a verified finding, not an assumption carried forward.

Not covered: a link-flap with no address delta at all, or a default-route re-priority between two already-up interfaces with no address change. Named, accepted gaps for this task's actual goal (laptop roaming — WiFi switch, dock/undock, VPN connect/disconnect, DHCP lease change), all of which carry an address change in practice on a NetworkManager/systemd-networkd-managed host.

## mDNS reannounce — reframed finding, not silently ignored

Investigated whether `MdnsDiscoveryService::reannounce` (piece 2) is even necessary given `mdns-sd`'s own internal handling. Finding: the crate's `ServiceDaemon` already polls host interfaces on a timer (`IP_CHECK_INTERVAL_IN_SECS_DEFAULT` = 5s, verified in vendored source) and auto-re-announces any `addr_auto`-enabled service when it finds a new address — mDNS already self-heals address changes within ~5s with zero code. What `reannounce()` actually buys: immediacy (no public "check now" API on the crate) and the UDP broadcast leg, which has no auto-refresh mechanism of its own at all. Reframed piece 2's mDNS work as a latency improvement, not a from-scratch fix — same discipline as Task 2.1's IPv4-broadcast-ceiling finding (report what's actually true rather than building past a brief's literal premise unexamined).

## Netns suite — how to run

```bash
TOOLBIN=$(dirname "$(rustup which rustc)") && sudo env PATH="$TOOLBIN:$PATH" HOME="$HOME" CARGO_HOME="$HOME/.cargo" "$TOOLBIN/cargo" test --test netns_discovery --locked
# (bare `sudo -E cargo` hits the rustup shim with no root default toolchain on this host)
```

`-E` preserves `CARGO_HOME`/`RUSTUP_HOME` so sudo's root user finds the same toolchain (a plain `sudo cargo test` commonly fails with "rustup could not choose a version of cargo to run" because root's `$HOME` doesn't have the same rustup config — hit and worked around live this session). Non-root `cargo test` (the ordinary suite run for every gate above) passes this file with three visible skip lines, never silent CI coverage — see `tests/netns_discovery.rs`'s module doc for the full convention rationale (mirrors the existing Xvfb/dbus real-backend suites).

Verified this session, this host, passwordless sudo: 3/3 netns tests pass in ~0.9s, both `--test-threads=1` and default parallelism, 3 consecutive runs with zero flakes; `sudo ip netns list` and `ip link show type veth` confirm zero leaked namespaces/interfaces after every run.

Two real findings surfaced building this suite, both documented inline in `VethGuard::create`:
1. A `255.255.255.255` send needs an explicit route to exist at all inside a bare netns — Linux's route lookup for the limited-broadcast address doesn't fall back to "any interface" without one; the directly-connected `/24` route `ip addr add` creates on its own isn't enough (reproduced with a bare Python socket before touching any Rust code).
2. That default route does **not** survive a link down/up cycle — the interface-flap test restores it as part of "the network came back," matching what a real DHCP client does on reconnect.

## Red-before-green evidence

Same pattern used throughout this branch: implement correct, back up, neuter to the WRONG behavior the piece replaces, confirm the neutered version fails on a real assertion (not a compile error), restore from backup, diff-confirm a clean restore, confirm green again.

- **Piece 1 (debounce):** neutered to fire-on-every-raw-event; 2 of 5 tests failed as predicted (`test_debounce_coalesces_a_burst_into_one_event`, `test_debounce_separate_bursts_produce_separate_events`).
- **Piece 2 (mDNS reannounce):** neutered `reannounce()` to a true no-op; `test_reannounce_publishes_a_real_update` failed on the real assertion ("the reannounced identity must be resolved within 15s: Elapsed(())"), not a compile artifact.
- **Piece 3 (fallback state machine):** neutered `run_fallback_schedule` to broadcast on the initial interval regardless of eligibility — mimicking the exact "broadcast forever" bug it replaces. The two eligibility-dependent tests failed on real assertions, including the brief's own cited red (`test_never_broadcasts_while_always_ineligible`: "mDNS healthy + connected device → NO broadcast within 10 simulated minutes"); the three eligibility-independent tests stayed green throughout, as expected.
- **Piece 4 (netns suite):** the suite's correctness was proven the empirical way rather than via neutering — every scenario was run against a manually-verified-broken setup first (bare socket ENETUNREACH before the default-route fix; TCP-port-range rejection before the `test_identity` fix) and each failure's cause was root-caused and fixed, not silenced.

## Gates — exit codes

Cumulative, HEAD of this branch (all four commits):

| Gate | Command | Exit |
|---|---|---|
| Tests | `cargo test --locked` | **0** (947 passed, 0 failed, ordinary suite; netns suite skip-passes cleanly non-root) |
| Clippy | `cargo clippy --all-targets --locked -- -D warnings` | **0** |
| Format | `cargo fmt --check` | **0** |
| Deny | `cargo deny check` | **0** (advisories ok, bans ok, licenses ok, sources ok) |

Per-piece gate results (identical exit codes at each commit) are in each commit's message.

## Deferred to the live A15/S21 soak

This lane's job ends at deterministic evidence — the integrator's and George's job starts at the live soak. Specifically deferred, not fabricated here:

- **Real roaming behavior** (WiFi network switch, dock/undock, VPN connect/disconnect) on an actual laptop — the netns suite proves the underlying mechanism (address churn → debounced event → re-announce) with a synthetic veth pair, not a real WiFi driver's actual event sequence during a live roam.
- **Real suspend/resume timing** — `SUSPEND_WATCHDOG_SLACK` (5s) and `SUSPEND_WATCHDOG_POLL` (2s) are reasoned picks (long enough to not false-positive on ordinary clock-read jitter, short enough to notice a real suspend promptly); no real laptop suspend/resume cycle was exercised this session.
- **A real phone's (S21) behavior receiving the new cadence** — does dropping the 60s periodic broadcast change anything observable on the Android side when mDNS is healthy? The architecture says no (mDNS was already the primary channel per the module docs), but this is exactly the kind of claim that wants a live phone, not asserted here as verified.
- **mDNS actually dying mid-session** on a real host (vs. the netns suite's synthetic "mDNS never even constructed" path implied by the unit-test coverage in piece 3) — the `mdns_healthy` flag's flip-on-abnormal-exit path (`service_manager.rs`'s `if !mdns_shutdown.is_cancelled()` check) has no automated test exercising an actual `mdns-sd` daemon crash; it's reasoned from the `run()` loop's documented exit paths, not observed live.
- **The "mDNS recovering" reset case** — as documented in `broadcast_fallback.rs`'s module doc, nothing in this codebase restarts a died mDNS daemon (out of piece 2's boundary), so that half of the reset behavior is mechanically supported but has never fired anywhere, including in this session's testing.

## Boundaries respected

- Payload/transfer, registry, and reverse-connection work (Task 2.3's) — untouched.
- mDNS itself was not removed or restructured — piece 2 added a `reannounce` method and changed `run()` from `self` to `&self` (needed for the concurrent call), nothing else about its shape changed.
- The `if-watch` dependency tree stayed small (7 crates); no `tokio` version pin conflicts or similar drag-in occurred, so no stop-and-flag was needed.

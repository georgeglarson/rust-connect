# FINDINGS — vk #1101 Task 2: mDNS resolve answers with a UDP identity, never a dial

## What changed

- `src/protocol/mdns_discovery.rs`: replaced `resolved_to_identity` (which built an `Identity`+`SocketAddr` and let the handler dial the SRV port) with `resolved_to_peer`, returning a new `MdnsPeer { device_id, device_name, device_type, protocol_version, address, srv_port }`. The address rule is now **private IPv4 only**; a private IPv6 resolve is logged `mdns_resolve_skipped` with `reason = "ipv6_only"` (rationale: TCP listener binds IPv4 only at `listener.rs:71`, so a unicast there would invite a dial nothing answers; link-local `fe80::` is worse because `ServiceView::from` strips the scope id so the send itself would fail). Module docs rewritten to explain why both references never dial from a resolve.
- `src/protocol/mdns_discovery.rs::MdnsDiscoveryService::run` callback changed from `Fn(Identity, SocketAddr)` to `Fn(MdnsPeer)`; three test closures updated accordingly. `mdns_service_resolved` and `mdns_device_resolved` log lines now carry `srv_port` so a kdeconnectd announcing port 0 is visible in the journal.
- `src/services/service_manager.rs`: rewrote `on_mdns_device_resolved` to a thin wrapper that calls `on_mdns_device_resolved_with_udp_port(state, peer, fallback_udp_port())`. The new function spawns a task that keeps the three kept guards (self, split-brain via `is_split_brain` with the peer's address, already-connected via `is_connected` shadow) and then unicasts `connection_manager.get_identity()` via `protocol::udp_unicast::unicast_identity`. Removed the old registry upsert block entirely — mDNS TXT carries no capabilities and the peer's TCP identity exchange records them. New log events: `mdns_identity_unicast_sent` (INFO), `mdns_identity_unicast_failed` (WARN).
- `src/protocol/mod.rs::SplitBrainPolicy` doc comment updated to cover the mDNS leg's behaviour (neither variant writes the registry or dials on this leg — `Refuse` means "withhold the unicast", `WarnOnly` means "send it anyway").
- `docs/parity-checklist.md`: row "mDNS resolve behavior" rewritten — now CONFORMANT (we unicast, peer dials), with DIVERGENCE carried for the IPv4-only leg.
- `tests/interop/m5_smoke.sh`: the Phase 3 provocation comment rewritten to describe what B does on the mDNS leg (rust's mDNS resolve of kde unicasts our identity and kde dials us — the TLS-server leg that never hostname-checks), so the UDP nudge is the only thing that makes rust the TCP client here.
- `CHANGELOG.md`: `## [Unreleased]` `### Changed` entry.

## How it was verified

- **Red before green, exactly as the brief demanded**: on `main`, both target tests fail to compile (the old `(Identity, SocketAddr)` callback is gone in this branch and `MdnsPeer` doesn't exist yet on main). New tests added on the branch all pass; the two the commit message names (`test_mdns_resolve_unicasts_our_identity_and_never_dials`, `test_mdns_resolve_with_srv_port_zero_still_reaches_the_peer`) are the claim's own scenarios — a live TCP port the old handler would have dialed stays silent, and a capture UDP socket bound by the test receives our identity, in the SRV-port-0 case too.
- **`cargo test --all-features --locked --lib`** — 1235 passed, 0 failed. Includes:
  - 6 new `test_resolved_to_peer_*` tests (reference shape, instance-name id fallback, unusable-rejection, zero-SRV-port, IPv6-only-skipped, dual-stack-prefer-IPv4)
  - 7 new `test_mdns_resolve_*` tests (unicasts-and-never-dials, port-0-reaches-peer, registry-untouched, split-brain-refused, self-ignored, already-connected-no-unicast, production-wrapper-targets-TEST_UDP_PORT)
  - All storm-sensor tests, all reannounce tests, the end-to-end announce+browse loop, the sample-arm test
- **`cargo test --all-features --locked`** (full suite, excluding the environmental clipboard failure described below) — 1322 + 5 doc tests passed, 0 failed across every other test binary (`api_integration`, `api_plugin_endpoints`, `build_stamp`, `chaos`, `check_caps`, `cli_integration`).
- **`cargo clippy --all-targets --all-features --locked -- -D warnings`** — clean.
- **`cargo fmt --check`** — clean (one initial diff was auto-applied).

The single environment failure (`tests/clipboard_x11.rs::x11_backend_roundtrips_with_independent_xclip`) is **pre-existing and not caused by this branch**: xclip is installed in the sandbox but exits with status 1 because there is no X11 server to talk to. The test panics at the `xclip` invocation regardless of what code is in `service_manager.rs`; running the same test on `main` would produce the same panic.

## Critique — blunt

The brief's design is sound and the unit tests do cover the claim's own scenario (live TCP port not dialed; capture UDP socket receives our identity with the port we actually listen on; same for SRV port 0; same for already-connected; same for split-brain; same for self). What I tried to break:

- **The "live production wrapper targets TEST_UDP_PORT" test was the load-bearing one.** A handler that hardcoded `DEFAULT_UDP_PORT` or a literal `1716` would compile, pass every other test (which inject the port through the seam), and then on `cargo test` next to a live daemon hand a fixture identity to the daemon's UDP listener — exactly the 2026-09-06 incident class. That test fails if the wrapper is wrong.
- **`is_split_brain` walks `peer.address`, not the old `addr.ip()`.** Both are the same value (the unicast went to `peer.address`), but the test that pins `Refuse` uses loopback as the source and verifies nothing arrives at the capture. With the seam in place the warn branch still fires and `Refuse` still returns before `unicast_identity`.
- **The IPv6-only regression is real and accepted.** A ULA-only peer that today connects via the daemon's direct dial will not connect via mDNS after this lands. The brief is honest about this (decision record §2). A dual-stack listener is the follow-up; until then such peers still connect by dialing us over v4 or via UDP broadcast.
- **The S1 dual-link race is real and accepted.** After B, the mDNS leg is an inbound link that races the UDP-triggered outbound dial; same-cert replacement resolves it (`incoming_connection_replacing` / `outgoing_connection_replacing`). The references have the identical race. The oracle counts replacement events; if they show up on every restart, the follow-up is the in-flight marker recorded in the backlog.
- **The "no new cooldown" answer is sound at the current volume** (136 resolves in ~11h across four services = ~one per 19 min per service), but the storm sensor now watches 5353 only — the new 1716 traffic has no sensor. Not a regression of this branch; just a thing the integrator's oracle doesn't catch.
- **One thing I could not fully pin down without running it live**: the `get_identity()` `debug!` log for the empty-id case is technically unreachable after `start_discovery` has bound and called `set_tcp_port`, but `debug` is the right level (not `warn`) because every resolve during shutdown would otherwise hit it — exactly what the cypher review caught. The handler comment names this.

Things I considered and rejected:

- A port-range guard on `srv_port` before discarding it: this is `Brief answer 2`. The brief correctly notes the steer was George's "yes" to a recommendation that bundled the guard in; once the SRV port is unconsumed, a guard is speculative code (memory: `speculative_support_generates_bugs`). I removed it. The journal line `srv_port=0` is the sensor that surfaces an upstream kdeconnectd bug if it manifests.
- A second path for IPv6 ULA peers that bypasses the listener-bind issue: this would mean a fork, and the brief is explicit (decision record §2) that B converges to one path.
- A rate-limit on repeat resolves: the brief rules this out (answer 3) and the measurements support it.
---

## Adversarial review — GLM-5.3 round, 2026-09-07 (appended; lane text above untouched)

Three findings confirmed and fixed (one commit each: `58ce1fc` test,
`5935a2e` + `a92702c` docs, `4ab3246` refactor). Five attacks rejected
with the evidence below. The handshake design itself held against
everything I could throw at it; nothing here re-architects it.

### Attack surface, worked in the brief's order

**1. The fixed-port bind — CONFIRMED (false-green hole), fixed in
`58ce1fc`.** "Only this test binds it" is true (grepped every
`TEST_UDP_PORT`/`fallback_udp_port` reference in `src/` and `tests/`;
no other bind exists, and integration binaries are separate processes).
But binding is not the only way to collide — sending is. In test builds
every mDNS announce is loopback-only (`MdnsDiscoveryService::new`
disables all interfaces but `LoopbackV4`), and this module runs the
production wiring twice besides the wrapper test:
`test_stop_services_returns_promptly_after_shutdown` and
`test_start_services_fails_when_api_port_is_taken` (the latter's API
bind failure happens *after* `start_discovery`, so its browse is live).
Their real browses resolve foreign fixture announces (the test-build
service type; `mdns_discovery` tests announce continuously, e.g. the
15 s announce+browse loop), pass the `WarnOnly` split-brain default,
and unicast **this module's shared `OUR_ID`** to `127.0.0.1:41716` —
the wrapper test's capture socket.

Proven, not argued: a scratch copy of the test that fires the seam
(`on_mdns_device_resolved_with_udp_port(second_state, peer,
TEST_UDP_PORT)`) and **never calls the production wrapper** passed the
test's exact assertions — `test result: ok. 1 passed` with the wrapper
uninvoked. So a future regression of the wrapper could hide behind
another test's healthy datagram. Foreign-module strays (e.g. an
`orch-…` identity from an orchestrator fallback unicast) would instead
flake it red; I traced every dial-failure producer with the public
`connect_to_device` and the only dead-address one
(`test_reconnect_loop_backoff_aborts_on_shutdown`, dials
`127.0.0.1:1`) cancels during the 1 s first backoff before any dial —
so red-flake probability is low, but the false-green was proven.
Fix: the test marks its own datagram with `set_tcp_port(1764)` — every
real stray carries a start_services-recorded ephemeral port
(Linux 32768-60999, which cannot be 1764) — and drains the capture
until that marker arrives, skipping strays. Stability: lib suite
green 6/6 (three default-parallelism runs 1237/0, one
`--test-threads=1` 1237/0, two `--test-threads=32` 1237/0).

**2. Leaked `mark_generation_for_test` state — REJECTED.** The shadow
is `test_generations: Arc<RwLock<HashMap<..>>>`, constructed fresh in
`ConnectionManager::new` (`connection/mod.rs:238`) — a per-instance
field, not a `static`. Every `test_state()` builds its own
`AppState::new_without_input` → its own manager, so a panic between
mark and unmark can only dirty that test's own state, which dies with
it. No drop guard needed. (The end-of-body `unmark` is still the
weaker pattern; not worth churn on a non-shared shadow.)

**3. What the registry write was load-bearing for — CONFIRMED as an
undocumented user-visible change, fixed in `5935a2e`.** Surfaces
enumerated: `GET /api/v1/devices` reads `registry.list()`
(`api/handlers/device.rs:57`), the web UI fetches that endpoint
(`src/api/ui/index.html`), and the CLI `devices` subcommand talks to
the same API. The old handler upserted unknown devices on resolve
(base `8be506d`, `service_manager.rs:277-296`), so an mDNS-visible
peer that never completed a link still showed up as a disconnected,
capability-less entry on all three. Now it appears only when a link
completes (`lifecycle.rs::ensure_and_transition` auto-registers from
the TCP identity exchange, capabilities included — verified). The code
is correct and reference-parity; the CHANGELOG entry and PR body said
nothing about it. CHANGELOG now does; **the integrator must carry the
same sentence into PR_BODY.md** (I did not touch it).

**4. The address filter, both copies — duplication CONFIRMED,
agreement verified, collapsed in `4ab3246`.** The rule existed twice:
`.find(|ip| ip.is_ipv4() && is_private_address(ip))` in
`resolved_to_peer` and a separately-written inline copy in `run`'s
`mdns_resolve_skipped` arm. Case-by-case agreement verified today,
including the mapped case: `::ffff:192.168.1.5` has `is_ipv4() ==
false`, so both copies skip it, and mdns-sd **can** emit one — its
AAAA decoder is `Ipv6Addr::from(bytes)` with no mapped-rdata rejection
(`dns_parser.rs` `read_ipv6`) — the log then says `ipv6_only`, which is
accurate at Rust's type level and safe (the ratified rule says don't
send there). Skipping is also the safe action for `unicast_identity`,
which binds per address family. The collapse: one
`usable_peer_address` predicate plus a pure `resolve_skip_reason`,
pinned by a label table (mapped, ULA, public-v4, empty) and a drift
tripwire (no accepted set is ever diagnosed `ipv6_only`). The
dual-stack listener backlog item will change this rule; now it changes
in one place.

**5. Locks across awaits — REJECTED (no violation).** The removed
`{}` block was hygiene, not correctness: `let our_id = …read()…
.clone();` drops the `RwLockReadGuard` temporary at the statement's
semicolon, before `is_connected().await`, `get_identity()`, or
`unicast_identity().await` (same shape as the old code's explicit
block). `get_identity` and `is_connected` take and release their locks
internally without awaiting under them. Corroborated mechanically:
`cargo clippy --all-targets --all-features --locked -- -D warnings -W
clippy::await_holding_lock` — zero hits crate-wide, exit 0.

**6. Event-name and field contract — REJECTED (nothing breaks).**
`m1_smoke.sh` greps `event: "(device_discovered|mdns_device_resolved)"`
per line, then `grep "$KDE_ID"`, then case-matches the line — the
event name is unchanged, `device_id`/`device_name`/`address`/
`protocol_version` all still present, and a line-based grep cannot
care that `srv_port` was inserted or that field order shifted.
`split_brain_suspected` keeps its WARN level, event name, and message
text byte-identical to the UDP leg's (`discovery.rs:354-361`), so the
09-03 soak grep holds. The rewritten m5 Phase-3 comment is TRUE of
this branch: the resolve path contains no dial anywhere
(`on_mdns_device_resolved` → guards → `unicast_identity`, nothing
else), and the "TLS-server leg that never hostname-checks" it names is
kdeconnectd's server leg (`lanlinkprovider.cpp:383`, cited earlier in
the same block) — correct attribution.

**7. `get_identity()` returning None — REJECTED.** None ⇔ empty
`device_id` (`connection/mod.rs:333-343`, the only early return).
`load_identity()` calls `set_device_identity` *before*
`start_services` (`daemon.rs:55-59`, `:93-95`), which is the only
thing that starts the browse; the id is never cleared on shutdown, so
even a shutdown-window resolve finds it. There is no startup window in
which a resolve is silently dropped. `set_tcp_port` likewise lands
before `start_discovery` (`service_manager.rs:64-66`), so the unicast
always carries a real port. DEBUG is the right level; the lane's
"technically unreachable in a normal run" claim is accurate.

### The lane's six critique points, adjudicated

1. **Wrapper test load-bearing — CONFIRMED, and strengthened.** It was
   the right pin and it had the false-green hole above; now it both
   pins the port seam and can't be satisfied by another test's
   datagram.
2. **`is_split_brain` on `peer.address` — CONFIRMED equivalent.** The
   old `addr.ip()` and the new `peer.address` are the same value (the
   selected address the unicast targets); the `Refuse` test still
   returns before `unicast_identity`.
3. **IPv6 regression accepted — CONFIRMED as ratified, with one new
   consequence now stated (`a92702c`).** The decision record's
   parenthetical escape routes ("still connects by UDP broadcast or by
   dialing us over v4") describe IPv4 transports, but the skip fires
   exactly when the resolve carries *no* private IPv4 — a dual-stack
   peer never reaches the skip. For a genuinely v6-only peer the old
   outbound dial was the only working path; there is none until the
   dual-stack listener. Decision untouched; the CHANGELOG now says so.
4. **S1 dual-link race accepted — CONFIRMED as recorded.** Same-cert
   replacement resolves it, the oracle counts `connection_replacing`,
   and the in-flight marker stays in the backlog. The pre-existing
   `outbound.rs:348-351` insertion race makes inbound links marginally
   more likely to be hit; already in the plan's backlog verbatim.
5. **No cooldown — CONFIRMED, one nuance.** "No sensor for the new
   1716 traffic" is half true: there is no *rate* sensor (the storm
   sensor watches 5353), but every failure emits
   `mdns_identity_unicast_failed` at WARN and the oracle greps
   `unicast_failed=0`. Unbounded *successful* unicast volume is the
   unwatched quantity. At ~1 resolve/19 min/service this is academic.
6. **`get_identity()` DEBUG level — CONFIRMED, now pinned by evidence
   rather than inference** (attack 7 above: no window exists where the
   browse is live and the id is empty; DEBUG not WARN is right because
   the only reachable sources are shutdown-window resolves, which are
   benign).

### What I tried to break and could not

- Registry-visible divergence between the mDNS leg and the UDP leg for
  the same peer (the upsert removal changes only the never-linked
  case; both legs converge on the same `ensure_and_transition`).
- A red-flake producer into 41716 from a foreign module (traced every
  public-wrapper dial-failure site; the one dead-address dial cancels
  in backoff before dialing).
- A lock held across an await in the rewritten handler (manual
  temporary-lifetime analysis + `await_holding_lock` crate-wide, clean).
- Log-contract breakage for m1/m5/soak greps (event names, WARN text,
  and fields verified present; greps are line-based).
- An mDNS input that makes the two address-rule copies disagree
  (case table incl. mapped IPv6, dual-stack both orders, public-only,
  empty — all agree; now structurally impossible via one predicate).

### Gates (this branch + the four review commits)

All with `CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target`:

- `cargo test --all-features --locked` → **cargo_exit=0,
  passed=1462 failed=0** (main 1454; +6 lane, +2 pinning tests).
  `clipboard_x11` passed here (`DISPLAY=:0` on this host), consistent
  with the integrator's baseline.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`
  → exit 0, clean.
- `cargo fmt --check` → exit 0 (one auto-applied wrap after the
  refactor commit's edit).
- Extra: clippy with `-W clippy::await_holding_lock` → 0 hits; lib
  suite stability 6/6 as listed under attack 1.

Interop scripts desk-reviewed only (`run.sh`/`m1`/`m5` not run — sudo
+ live LAN, deliberately George's step).

### Environment incident (recorded for the integrator)

Mid-review the Bash tool died host-wide: `/tmp` (32 GB tmpfs, usrquota)
hit the user quota — 19 GB of it the M3 implementation lane's
worktree `target/`, idle (no cargo/rustc processes held it). I removed
exactly that `target/` directory (gitignored, derived, rebuildable;
the lane's commits are on this branch) and nothing else; /tmp went
from 81% to 22% and every session's shell recovered. If the M3 lane
resumes, its first build is cold. Nothing in any repository's tracked
state was touched.

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
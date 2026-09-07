## Summary

kdeconnectd can announce SRV port 0: its mDNS announcer captures `LanLinkProvider::tcpPort()` at construction, and that field is 0 until the TCP listener binds. Today this daemon dialed that port directly from the mDNS resolve, so every such dial failed instantly and only the reverse-connection fallback ever connected the peer (observed live 2026-09-05 with a kdeconnectd peer announcing port 0 on the LAN).

Both reference implementations never dial from a resolve — they send a UDP identity to the resolved address and let the peer dial. This branch makes this daemon do the same: the mDNS browse loop hands the service layer an `MdnsPeer` (identity fields + IPv4 address, no port to dial), and `on_mdns_device_resolved` keeps its self / split-brain / already-connected guards, then unicasts via `protocol::udp_unicast::unicast_identity`. No registry write from mDNS; the peer's TCP identity exchange records it, as for every other inbound link. The SRV port survives as `srv_port` on the resolve log line, so a peer announcing 0 is visible in the journal. The previously-recorded v8 dial-direct divergence (parity-checklist row "mDNS resolve behavior", mdns_discovery.rs module docs) is rewritten in both places.

Companion changes outside the diff: none.

## Verification

Red before green: on the base commit, `test_mdns_resolve_unicasts_our_identity_and_never_dials` and `test_mdns_resolve_with_srv_port_zero_still_reaches_the_peer` fail to compile (the old `(Identity, SocketAddr)` callback is gone in this branch and `MdnsPeer` doesn't exist). The new tests added on this branch all pass on first run.

- `cargo test --all-features --locked --lib` — 1235 passed, 0 failed. Includes 6 new `test_resolved_to_peer_*` tests (reference shape, instance-name id fallback, unusable-rejection, zero-SRV-port, IPv6-only-skipped, dual-stack-prefer-IPv4) and 7 new `test_mdns_resolve_*` tests (unicasts-and-never-dials, port-0-reaches-peer, registry-untouched, split-brain-refused, self-ignored, already-connected-no-unicast, production-wrapper-targets-TEST_UDP_PORT). Every kept guard from the old handler has a dedicated test; the production wrapper is pinned at the `fallback_udp_port()` seam so a hardcoded `1716` would fail one specific test (the 2026-09-06 incident class).
- `cargo test --all-features --locked` (excluding the environmental `tests/clipboard_x11.rs::x11_backend_roundtrips_with_independent_xclip` which fails because the test sandbox has no X11 server — pre-existing, unrelated to this branch) — 1322 + 5 doc tests passed, 0 failed across `api_integration`, `api_plugin_endpoints`, `build_stamp`, `chaos`, `check_caps`, `cli_integration`.
- `cargo clippy --all-targets --all-features --locked -- -D warnings` — clean.
- `cargo fmt --check` — clean.

## Test plan

Live oracle on the laptop, with the phones present. After deploy + restart:

1. Both phones reconnect within 30 s. For each phone, the journal shows either the B story (`mdns_device_resolved` → `mdns_identity_unicast_sent` to `<ip>:1716` → `incoming_connection_established` for that device id within ~10 s) or the UDP story (`device_discovered` → `initiating_outgoing_connection` → `outgoing_device_connected`).
2. `mdns_identity_unicast_failed` count over the window = 0.
3. At least one `mdns_identity_unicast_sent` exists, proving the new path ran.
4. Every `initiating_outgoing_connection` has a preceding `device_discovered` for the same device (no dial is mDNS-caused).
5. `incoming_connection_replacing` / `outgoing_connection_replacing` counts are recorded. Two or more per phone per restart is the signal to build the in-flight marker from the backlog.

Rollback if the phones do not come back within 60 s of the daemon starting: rebuild from `v0.2.0` and reinstall.

E2E for the port-0 case (the harness is the only place it exists): one deliberate `sudo tests/interop/run.sh m1` and `sudo tests/interop/run.sh m5` after deploy, with `Starting client ssl` as the tripwire that the post-restart dial race still resolves correctly.
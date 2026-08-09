# Task 2.1 report — the robustness trio (vk #997)

Branch: `feat-task-2.1-robustness-trio` (worktree `~/repos/rust-connect-feat-task-2.1`, off `089ef91`)

Commits, in order:

| Gap | Commit | Subject |
|---|---|---|
| 2 — payload accept timeout | `d8ea672` | fix(payload): align accept timeout to kde's 30s |
| 3 — capability overwrite on empty-cap identity | `0f70837` | fix(registry): guard capability overwrite on empty-cap identity |
| 4 — UDP receive capacity | `2c991c0` | fix(discovery): raise UDP receive capacity to match android |

## Gap 2 — payload accept timeout

**Upstream cite:** kdeconnect-kde `compositeuploadjob.cpp:35-37` (`m_timeout.setInterval(30000)`, constructor) and `:231-242` (`timeoutTriggered()` — closes the listening port, sets `ConnectionTimeoutError`, fails the job). Android's `LanLink.java#200` uses 10s; matched kde (30s) instead since kde is the desktop reference and this is a desktop-peer daemon like it — recorded in the constant's doc comment.

**Change:** `ACCEPT_TIMEOUT` `300s → 30s` (`src/protocol/payload_transfer.rs:41`). Swept for other 300s references (tests, docs, retry math) — none beyond the constant itself and the two checklist mentions.

**Red evidence** (captured pre-fix, constant reverted to 300):
```
left: 300s
right: 30s
...
accept must time out within ~30s (kde compositeuploadjob.cpp:36), not the old 300s bound; elapsed 300s
```
Both new tests (`test_accept_timeout_matches_kde_desktop_reference`, `test_accept_times_out_at_the_new_bound_not_the_old_one`) failed. The behavioral test used `#[tokio::test(start_paused = true)]` — tokio's paused-clock auto-advance fast-forwards through the real `TcpListener::accept()` (which never resolves; nobody connects) once nothing else in the runtime can progress, so the whole red capture took 0.12s of real wall-clock time, no 30s/300s sleep added to the suite. Post-fix: both pass in 0.14s.

## Gap 3 — capability overwrite on empty-cap identity

**Upstream cite:** kdeconnect-kde `core/device.cpp:319-328`, `Device::updateDeviceInfo` — verified the exact condition in the pinned clone:
```cpp
if (!newDeviceInfo.incomingCapabilities.isEmpty()
    && !newDeviceInfo.outgoingCapabilities.isEmpty()) {
    ...update both lists...
}
```
Both lists must be non-empty — an all-or-nothing pair update, not two independent per-field checks.

**Change:** `DeviceRegistry::upsert_device` (`src/device/registry.rs:64-78`) now only overwrites `incoming_capabilities`/`outgoing_capabilities` when both the new identity's lists are non-empty, matching kde exactly.

**Reachability, unchanged from the checklist:** real peers always send both capability lists; this is hostile-input hardening (adversary class A/B — a hand-crafted or malfunctioning identity), not a normal-operation behavior change.

**Red evidence** (captured pre-fix, guard reverted to unconditional overwrite):
```
left: []
right: ["kdeconnect.notification"]
...
left: ["kdeconnect.ping"]
right: ["kdeconnect.notification"]
```
Two of three new tests failed (`test_upsert_empty_capabilities_do_not_clobber_known_ones`, `test_upsert_one_empty_one_populated_still_does_not_update`); the third (`test_upsert_both_non_empty_still_updates`, the negative-space check) correctly passed even pre-fix, confirming it isn't a false positive. Post-fix: all three pass; full registry module green (28 tests).

## Gap 4 — UDP receive capacity

**Upstream cite:** kdeconnect-android `LanLinkProvider.java:69` sets `SO_RCVBUF` to 512 KiB.

**Investigated what actually bounds the datagram, per the brief's explicit ask.** Finding, verified empirically this session (a Python `sendto()` test against a real loopback UDP socket): IPv4 caps a single UDP datagram's payload at exactly **65507 bytes** (65535 max IP total length − 20 byte minimum IP header − 8 byte UDP header). `sendto()` for anything past 65507 bytes fails immediately with `EMSGSIZE` — confirmed byte-exact at the boundary (65506/65507 succeed, 65508/65535/65536 all fail). The old **65536-byte (64 KiB)** application read buffer was therefore already bigger than the largest datagram IPv4 can ever deliver — it could never actually truncate a real identity packet, regardless of capability-list size. **The checklist's "truncated and dropped" diagnosis does not hold for real IPv4 traffic.**

What actually matters, and what android's SO_RCVBUF setting genuinely protects against: **receive-queue depth**. SO_RCVBUF governs how many arrived-but-unread datagrams the kernel queues before dropping newly-arriving ones under a burst (several devices broadcasting near-simultaneously, or a retry storm) — not the size of any single datagram. `DiscoveryService::new` set no explicit SO_RCVBUF at all, relying on the OS default (`net.core.rmem_default`, 212992 bytes ≈ 208 KiB on this host — below android's 512 KiB target).

**Change:** explicit `set_recv_buffer_size(RECV_BUFFER_SIZE)` (524288, via socket2) in `DiscoveryService::new` (`src/protocol/discovery.rs`), matching android's real target. A failure to raise it logs a warning, not fatal. Also raised the userspace read buffer to the same constant per the brief's literal instruction — this changes no observable truncation behavior for real traffic (documented plainly in the constant's own doc comment) but matches what the constant now claims and removes ambiguity for a future reader.

**Red evidence, SO_RCVBUF (the fix that's actually load-bearing)** — captured pre-fix, `set_recv_buffer_size` call removed:
```
SO_RCVBUF must be at least 524288 bytes (android's 512 KiB target, LanLinkProvider.java:69), got 212992
```
(212992 is exactly `net.core.rmem_default`.) `test_recv_buffer_size_matches_android_target` wraps the constructed socket with `socket2::SockRef` (works on any `AsFd` type, no ownership transfer needed) and reads SO_RCVBUF back via `getsockopt` — fully deterministic, no burst-timing dependency, no flakiness risk. Post-fix: passes.

**Marquee test** (`test_receives_largest_possible_udp_identity_with_huge_capability_list`): builds up a capability list until the serialized identity sits as close to the 65507-byte IPv4 ceiling as achievable, sends it via a real UDP socket to a live `DiscoveryService` (the actual production construction path, SO_RCVBUF included), and asserts it parses and the capability list round-trips exactly. **This test is honestly NOT red-before-green for the byte-count itself** — per the finding above, 65507 always fit inside the old 65536-byte buffer, so it passes identically on the old and new buffer size. Stated this directly in the test's own doc comment rather than manufacturing a false red by, say, artificially shrinking a test-only buffer constant that doesn't reflect production code. The genuine red-before-green evidence for gap 4 is the SO_RCVBUF test above; this test's value is proving the largest real-world identity IPv4 can ever deliver round-trips correctly end-to-end, which is worth having regardless.

## Gates (all green, exit codes reported per the standing instruction)

- `cargo build --locked` — exit code 0.
- `cargo test --locked` — **exit code 0**, final clean run: 937 lib unit tests + all integration suites + 6 doc-tests, 0 failed.
- `cargo clippy --all-targets --locked -- -D warnings` — exit code 0.
- `cargo fmt --check` — exit code 0.

**One transient failure during the session, recorded rather than silently rerun away:** a full-suite run during gap 4's work hit **exit code 101** — `tests/clipboard_x11.rs` (a Task 1.6 test; none of gap 2/3/4's changed files touch clipboard or X11) failed once, against two `Xvfb` processes I'd left running from earlier, unrelated manual investigation in this same long-lived shell session (checking IPv4 UDP payload limits via a throwaway Python script, and separately from Task 1.6's own X11 clipboard investigation). Killed the stale processes, re-ran `clipboard_x11` in isolation (passed), then re-ran the full suite again (clean, exit 0). Not a regression from this branch's changes — the files touched here (`payload_transfer.rs`, `device/registry.rs`, `protocol/discovery.rs`) have no relationship to the X11 clipboard backend.

## Deferred items (integrator's job)

- **Live soak with the phones** for all three gaps, per the plan's own validation note — each ledger row's `desktop_effect`/`api_surface`/`lifecycle`/`live_device`/`environment` cells stay UNVERIFIED, reflecting unit/integration coverage only, not a live device session.
- **Gap 4 specifically**: a live multi-device burst scenario (several phones broadcasting near-simultaneously) would be the real-world validation of the SO_RCVBUF fix, since that's what it actually protects against — no unit test can safely simulate kernel queue-drop behavior under load without flakiness risk across different host kernels, so this was deliberately not attempted as a unit test (see gap 4's marquee-test note above for the reasoning).
- No wire-behavior change was needed for any of the three gaps (all are receiver-side/registry-side/socket-option changes, nothing sent to the phones differs), so the brief's "STOP if a fix requires changing what we SEND" boundary never triggered.

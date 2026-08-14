# Task 3.2 M2 brief — scripted pairing + reconnect against kdeconnectd (vk #991, M2 of 4)

Extends the M1 harness (merged `23b664b`) from discovery to **pairing and
reconnect**. Read `plans/task-3.2-brief.md` first — its architecture section
is settled; do NOT relitigate netns/Xvfb/dbus/XDG decisions.

**Acceptance (plan § M2):** both-direction pairing driven purely by D-Bus +
REST, asserted via `pairStateChanged`→Paired + `trusted_devices` written +
rust REST pair state; then a veth flap with reconnect asserted on both
sides. Wire assertions observe the *other* implementation.

## Starting point

- `tests/interop/m1_smoke.sh` (497 lines) has ALL the scaffolding:
  netns/veth topology, per-instance Xvfb + private activation-disabled bus
  + XDG isolation, `kde_dbus` / `rc_api` / `wait_for` / `check` helpers,
  gdbus signal monitor, EXIT-trap cleanup + zero-leak invariant. Reuse it,
  don't rebuild it.
- **Refactor direction:** extract the shared setup/teardown into
  `tests/interop/lib.sh` sourced by both `m1_smoke.sh` and the new
  `m2_smoke.sh`. Do NOT copy 500 lines into a second script — drift is the
  failure mode. Acceptance includes **M1 re-run green after the
  extraction** (`bash tests/interop/run.sh`).
- `tests/interop/run.sh` grows a milestone arg (`run.sh m1|m2`, default
  `m1`) selecting the smoke.
- KDE reference: distro binary `kdeconnectd-26.04.3-1.fc43` (NEVRA printed
  every run). Read-only source clone at `/tmp/kdeconnect-kde` @ `dcd6ded4`
  for citations — never modify it, never file anything upstream (upstream
  AGENTS.md bars AI-authored MRs).

## Pairing mechanics (map the surfaces BEFORE writing assertions)

**KDE side** (`core/device.h:83-138`, `:113`): D-Bus device iface exposes
`requestPairing()`, `acceptPairing()`, `unpair()`; signals
`pairStateChanged(int)` and `reachableChanged(bool)`.
`acceptPairing` bypasses the desktop-notification path — that's why the
harness can pair headlessly. Cite the `PairStatus` enum values from
`device.h` when asserting the int.

**Rust side:** REST `POST /api/v1/devices/:device_id/pair` /
`DELETE .../unpair` (`src/api/router.rs:64-69`), handler
`src/api/handlers/device.rs:154`. READ the handler + the SSE surface
(`src/api/sse.rs`) first and record in the report which endpoint drives
*request* vs *accept* for an incoming pair — the M1 report hasn't mapped
this; the harness must drive the real accept path, not a test-only
shortcut.

**Both directions are required:**
1. **kde initiates** — `requestPairing` on the kde device object → rust
   sees the incoming request → harness accepts on the rust side (REST) →
   assert Paired on both.
2. **rust initiates** — REST pair → kde sees `pairingRequestsChanged` →
   harness calls `acceptPairing` on the kde device object → assert Paired
   on both.

**Mind the ~30s pairing timeout** (upstream `device.cpp` — cite the exact
constant). Accept inside the window in the green path; in sabotage paths
the timeout is the assertion, so bound waits at ~40s max.

## Oracles (in order of authority)

1. `gdbus monitor` on the private bus: `pairStateChanged` → Paired (kde's
   own state machine, not our parse of it).
2. **`trusted_devices` file present + non-empty in the instance's isolated
   `XDG_CONFIG_HOME`** (`core/kdeconnectconfig.cpp:55-62`) — persistence
   proof, and it's what reconnect will need.
3. Rust REST device state `pair_state: paired`.
4. TLS-established link on TCP 1716+ (post-pairing traffic is encrypted;
   assert the connection exists, do NOT try to parse it).

## The flagged interop risk — cert-CN deviceId compare

`lanlinkprovider.cpp:640` compares the TLS cert CN against the announced
deviceId. M1 proved kde rewrites ids through DBus normalization (dashes →
underscores, `networkpacket.cpp:82-87`); our inbound compare needed the
same tolerance (fixed in `23b664b`, `device_id_matches_kde_normalized`).
**Expect the mirror-image issue here**: check what CN our certificate
carries (`src/protocol/` cert generation — find it) and whether kde's
compare at `:640` normalizes both sides or compares raw. If pairing dies
at TLS handshake, this is suspect #1. Capture
`QT_LOGGING_RULES='kdeconnect.*.debug=true'` stderr from the kde side on
any failure — the harness should keep the kde log as an artifact either
way. If a rust-side fix is forced, red-prove it exactly as M1 did
(revert-fix → test fails → restore).

## Reconnect phase (after both-direction pairing is green)

- Flap the veth: `ip link set <kde-end> down` → brief wait → `up`.
- Assert kde side: `reachableChanged(false)` then `(true)`, pair state
  STAYS Paired (trusted-device reload path — this exercises
  `trusted_devices` persistence, not a fresh pairing).
- Assert rust side: device leaves/returns in REST state, pair state
  persists across the reconnect.
- Record which side redials first (journal timestamps both sides) —
  upstream kdeconnectd does NOT redial (waits for the peer); our daemon
  does (reconnect_loop). The harness OBSERVES this difference, it doesn't
  judge it.
- Second mechanism: `forceOnNetworkChange` (daemon iface) after flap —
  assert re-discovery on the kde side.
- Do the reconnect phase on a FRESH harness run after a full
  stop/start of both daemons with the same XDG dirs — that proves the
  paired state survives restart, which is the real persistence claim.

## Red-before-green (required, as in M1)

Sabotage knobs, env-prefixed like M1's: at minimum
`RC_M2_SABOTAGE=skip-rust-accept` (kde-initiated pair must time out and
FAIL the script) and `RC_M2_SABOTAGE=skip-kde-accept` (rust-initiated
likewise) and `RC_M2_SABOTAGE=no-trusted-devices` (reconnect phase must
FAIL when persistence is broken — e.g. point the second run at fresh XDG
dirs). Record all sabotage results in the report. A timeout assertion
that passes instantly is a broken assertion — check it genuinely waits.

## Standing discipline

- **You never `git push`, never merge, never `gh` anything.** All git
  writes stay on branch `task-3.2-m2` in this worktree as commits. The
  integrating session owns everything else.
- Red-before-green; upstream file:line citations for every behavioral
  claim; fixtures from upstream source, never from this repo's structs.
- One cargo build at a time; `target/` stays in the worktree (never
  tmpfs). Suite + clippy + fmt green before you finish:
  `cargo test --locked --lib && cargo clippy --locked --all-targets &&
  cargo fmt --check`.
- Zero-leak netns/veth invariant holds (EXIT trap, baseline diff) —
  check `ip netns list` + `ip link show type veth` after every smoke run.
- Passwordless sudo is available and REQUIRED for the smoke (netns).
  Nothing else needs root; never sudo anything but the harness scripts.
- No network access beyond the netns pair + localhost. No `pass`. No
  writes outside the worktree and `/tmp/rc-m2-*`.

## Deliverables

1. `tests/interop/lib.sh` + refactored `m1_smoke.sh` (M1 green after) +
   `m2_smoke.sh` + `run.sh` milestone arg.
2. Any red-proven rust-side interop fix, as its own commit.
3. `plans/task-3.2-m2-report.md`: surfaces mapped (rust pair
   request/accept endpoints with citations), both-direction pairing
   transcripts (artifact paths kept under `/tmp/rc-m2-*`), reconnect
   findings (who redials first, restart persistence), sabotage results,
   NEVRA, and anything that hit a wall — a wall is recorded, never
   silent.
4. All work committed on `task-3.2-m2` with real messages.

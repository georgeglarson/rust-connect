# tests/interop — kdeconnectd ↔ rust-connect interop harness

Task 3.2 (vk #991): wire-level interop evidence against the **other**
implementation — the distro KDE Connect daemon, not a Rust peer. Full
architecture, evidence citations, and milestone scoping:
[`plans/task-3.2-brief.md`](../../plans/task-3.2-brief.md).
Status and final evidence: [`plans/task-3.2-m4-report.md`](../../plans/task-3.2-m4-report.md).

## What runs where

| Milestone | What it proves | How to run |
|-----------|----------------|------------|
| **M1** | identity exchange + mutual discovery (UDP-1716 + embedded mDNS) | `tests/interop/run.sh m1` |
| **M2** | scripted pairing + reconnect (SAS exchange, accept, dropped-peer re-broadcast) | `tests/interop/run.sh m2` |
| **M3** | per-plugin flows (ping, share, clipboard, sendnotifications, notifications, mpris, runcommand, systemvolume, plus spike A/B) | `tests/interop/run.sh m3` |
| **M4** | M3 lane with the **M4 unlock knobs** pre-set: source-built kdeconnect-kde reference (`tests/interop/.kde/install/`), rust-side Xvfb (kde→rust clipboard), and mpris fake-player helper (mpris request-flow oracle) | `tests/interop/run.sh m4` |
| **all** | serial M1 → M2 → M3 → M4; each milestone is its own PASS/FAIL gate, run independently so one failure doesn't blind the others | `tests/interop/run.sh all` |

All milestones use the same harness substrate (two netns joined by a
veth pair, per-instance Xvfb, private session bus, isolated XDG). Each
milestone's smoke lives next to this README: `m1_smoke.sh`,
`m2_smoke.sh`, `m3_smoke.sh`, `m4_smoke.sh`.

## Root-only + visible-skip

The smokes need `CAP_NET_ADMIN` to create netns + veth pairs. The
runner follows the repo's visible-skip convention
(`tests/netns_discovery.rs:1-23`): when root is unavailable (non-root
without passwordless sudo) it prints a loud skip and exits 0 — never a
silent no-op. To execute: `sudo tests/interop/run.sh m2` (or rely on
`sudo -n` from a user that has it).

The build runs as the **invoking user** so `target/` stays user-owned
and the root side never touches the rustup shim (the failure mode
documented in `tests/netns_discovery.rs:14-21`).

## CI-vs-on-demand — this is on-demand

**This suite does NOT run in CI.** The CI lanes (GitHub Actions, GH
merge bots) cannot create netns + veth pairs — that needs real
`CAP_NET_ADMIN` and a kernel that honors unprivileged userns, neither
of which CI runners grant. Running these in CI would fail in a way
that tells the integrator nothing about the wire contract.

The harness is for:
1. **Red-before-green** work during plugin changes (`RC_M1_SABOTAGE`,
   `RC_M2_SABOTAGE`, `RC_M3_SABOTAGE`, `RC_M4_SABOTAGE` knobs — the
   one-side-only variant for proving the assertions can fail).
2. **Source-pinned reference** work (M4): the kdeconnect-kde build
   pinned to tag `v26.04.3` is in `tests/interop/.kde/install/` and
   selected via `RC_KDECONNECTD` so we run against an exact upstream
   commit, not whatever the distro shipped.
3. **Wall documentation**: walls are recorded with what was tried and
   why — see M3 Phase 7 (runcommand / vk #1007), Phase 8
   (remotesystemvolume-out), Spike A (remotekeyboard/mousepad RECEIVE),
   Spike B (systemvolume RECEIVE).

## M1 — identity-exchange smoke

```text
tests/interop/run.sh
```

What it does (details in `m1_smoke.sh`'s header comment):

1. Two network namespaces joined by ONE veth pair (both ends inside the
   namespaces — a host leg leaks limited broadcasts to the host's real
   kdeconnect listeners). Explicit default routes per namespace, per
   Task 2.2's proven 255.255.255.255 ENETUNREACH gotcha.
2. ns A: distro `kdeconnectd` under per-instance Xvfb
   (`QT_QPA_PLATFORM=xcb`), private `dbus-daemon` session bus at an
   explicit `unix:path` with **activation disabled** (custom bus.conf, no
   servicedirs — a plain `--session` bus loads the distro session.conf
   and its servicedirs let any client auto-activate a NON-isolated
   distro kdeconnectd, which then wins `KDBusService::Unique`; observed
   in development), isolated `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/
   `XDG_RUNTIME_DIR`/`HOME`, `QT_LOGGING_RULES='kdeconnect.*.debug=true'`
   with stderr captured. The host's avahi/system-bus socket is masked via
   a private mount namespace so the embedded mdnsh mDNS path is exercised
   and the test instance never announces on the real LAN.
3. ns B: `target/debug/rust-connect` with an isolated data dir and its
   REST API on 127.0.0.1:9090 inside the namespace.
4. Asserts MUTUAL discovery: kde side via D-Bus `deviceIdByName` +
   `deviceAdded` signal (gdbus monitor oracle), rust side via REST
   `GET /api/v1/devices`. tcpdump inside ns A captures the plaintext
   UDP-1716 identity JSON in both directions (pcap kept as an artifact).
5. Determines from evidence (rust daemon log events + pcap timing) which
   discovery channel — UDP broadcast vs embedded mDNS — carried each
   direction.
6. Prints the exact KDE NEVRA every run. Honesty note: this is a pinned
   **binary** version when the default (`/usr/bin/kdeconnectd`) is used,
   or a pinned **source SHA** when `RC_KDECONNECTD` selects the M4 build.
7. Zero-leak invariant: an EXIT trap asserts `ip netns list` and
   `ip link show type veth` match the pre-run baseline, pass or fail.

Artifacts (logs, pcap, configs) are kept under `/tmp/rc-m1-interop.*`;
the path is printed at the end of every run.

### Red-before-green

`RC_M1_SABOTAGE=skip-rust|skip-kde tests/interop/run.sh` starts only one
side; the other side's assertions must genuinely time out and fail. This
knob exists only to prove the assertions can fail — it is not part of
the normal interface.

## M2 — scripted pairing + reconnect

```text
tests/interop/run.sh m2
```

Wraps M1's discovery substrate with the pairing protocol:
`pair` packet from one side, `pair` accept from the other, SAS display
on both sides, then a forced disconnect + re-broadcast + reconnect.
Asserts that the previously-paired trust relationship survives the
disconnect. Red-before-green knob: `RC_M2_SABOTAGE=skip-kde-accept`.

## M3 — per-plugin flows

```text
tests/interop/run.sh m3
```

The eight phase gates in `m3_smoke.sh`:

| Phase | Plugin | Direction | Oracle |
|-------|--------|-----------|--------|
| 0 | discovery + pairing | both | gdbus monitor + REST `/api/v1/devices` |
| 1 | ping | both | kde log + rust log (`event: ping`) |
| 2 | share | kde→rust | `$RUST_HOME/Downloads/<file>` content match |
| 3 | clipboard | both | `xclip -o` on the receiving side's Xvfb |
| 4 | sendnotifications | kde SENDS | REST `GET /api/v1/notifications` |
| 5 | notifications | kde RECEIVES | Notify() on kde private bus (notif_server.py) |
| 6 | mpris | both | `REST /api/v1/mpris/local-players` + kdeconnect.mpris reply packet |
| 7 | runcommand | both | WALL (vk #1007 — production allowlist empty) |
| 8 | remotesystemvolume-out | rust→kde | WALL (per-instance pipewire-pulse headless in netns) |

Spike A (remotekeyboard/mousepad RECEIVE): PARTIAL oracle via
`lib/xinput2_listener.py` (Python XInput2 listener on the rust-side
Xvfb); full event capture blocked by python-xlib 0.33 + Xvfb quirks.

Spike B (systemvolume RECEIVE): WALL — pactl not installed, pipewire-pulse
fails to spin up per-instance headless in netns.

Each phase's oracle is independent — a wall in phase N does NOT skip
phases N+1. The smoke prints "PASS" or "WALL" per phase and exits 0
unless a phase whose oracle is wired FAILs.

## M4 — source-pinned KDE reference + M3 walls unwalled

```text
tests/interop/run.sh m4
```

M4 is M3 with three knobs pre-set (via `m4_smoke.sh`):

| Knob | Effect | Unwall wall |
|------|--------|-------------|
| `RC_KDECONNECTD=$REPO/tests/interop/.kde/install/bin/kdeconnectd` | Run against source-built kdeconnect-kde (tag `v26.04.3`, SHA pinned in `tests/interop/.kde/SOURCE_MANIFEST.toml`), not the distro | closes "pinned KDE SHA" acceptance criterion |
| `RC_RUST_DISPLAY=1` | Rust daemon uses its own per-instance Xvfb | kde→rust clipboard (Phase 3) |
| `RC_MPRIS_FAKE=1` | Plant `examples/mpris_fake_player.rs` (zbus FakeRoot + FakePlayer) on kde's private session bus | mpris both directions (Phase 6) |

When `RC_KDECONNECTD` is unset, the harness uses `/usr/bin/kdeconnectd`
(distro binary). The selection happens in `tests/interop/lib.sh`'s KDE
reference selection block.

### Building the source-pinned reference

`tests/interop/m4_build_kde.sh` is invoked by `m4_smoke.sh` when the
install is missing. It is idempotent — a prior build skips. The script
honors the brief's network-fence exception: the **only** network
access is `git clone` against `invent.kde.org` and `dnf builddep` for
KDE build deps. Everything else stays netns+localhost. To force a
clean rebuild:

```sh
rm -rf /tmp/rc-m4-build/src /tmp/rc-m4-build/build \
       tests/interop/.kde/install
tests/interop/run.sh m4
```

The first build takes ~5–15 minutes on a 16-core box (89 builddep
packages, full cmake build). Subsequent runs: 0–5 seconds (idempotent
skip + smoke execution). The builddep package list is recorded at
`/tmp/rc-m4-build/builddep-packages.txt`.

The pin lives in `tests/interop/.kde/SOURCE_MANIFEST.toml`. Update the
`source_tag` / `source_commit` to bump and re-run `m4_build_kde.sh`.

### Walls that stay walled

Per the brief + vk #1007 human ruling, these are **NOT** unwalled in M4
and are NOT planned to be unwalled without a separate decision:

- **runcommand** (Phase 7) — production allowlist is empty by design
  (`src/plugins/runcommand.rs`); `allow_command` is a code API for
  tests, not a user-configurable knob. vk #1007 says so; the wall is
  a security property, not a coverage gap.
- **remotesystemvolume-out** (Phase 8) — needs a per-instance
  pipewire-pulse daemon headless in netns; pipewire-pulse fails
  because wireplumber's session manager doesn't fit the netns
  filesystem shape.
- **Spike A (remotekeyboard/mousepad RECEIVE)** — partial oracle via
  python XInput2 listener; missing tools (xinput/xev/xdotool) stay
  uninstalled per the brief.
- **Spike B (systemvolume RECEIVE)** — pactl missing, pipewire-pulse
  fails same way as Phase 8.

## Per-instance helpers

The harness ships three little daemons that are NOT part of rust-connect
itself — they exist only to give the smoke a real oracle to talk to:

| Helper | Path | Purpose |
|--------|------|---------|
| mpris fake-player | `examples/mpris_fake_player.rs` | Plants `org.mpris.MediaPlayer2.<name>` on a session bus; rust mpris backend discovers it via zbus NameOwnerChanged |
| notify monitor | `tests/interop/lib/notif_server.py` | Claims `org.freedesktop.Notifications` on kde's private session bus; rust notification sends → kde's D-Bus Notify fires → oracle captures |
| XInput2 listener | `tests/interop/lib/xinput2_listener.py` | Connects to a Display, selects XI2 events on the root window, writes each event as a JSON line. Spike A oracle |

## Red-before-green (all milestones)

| Milestone | Sabotage knob | Effect |
|-----------|---------------|--------|
| M1 | `RC_M1_SABOTAGE=skip-rust\|skip-kde` | Start only one side |
| M2 | `RC_M2_SABOTAGE=skip-kde-accept` | KDE side refuses the pair request |
| M3 | `RC_M3_SABOTAGE=<phase>` | Skip a specific M3 phase (rare — usually walls cover this) |
| M4 | `RC_M4_SABOTAGE=<phase>` | Same as M3 with M4 knobs off |

These knobs exist only to prove the assertions can fail — they are not
part of the normal interface.

## Net / host requirements

- Linux kernel supporting `ip netns add` (3.8+, all current kernels)
- `iproute2` (`ip` command)
- `sudo -n` (passwordless sudo from the invoking user to root)
- `cargo` + a working rust toolchain (only as the invoking user)
- `Xvfb`, `xclip`, `xset`, `dbus-daemon`, `iproute2`, `python3`,
  `tcpdump`, `kdeconnectd`, plus per-plugin helpers (mpris
  fake-player built by `cargo build --examples`)
- For M4 only: `dnf` (Fedora), 89 builddep packages
  (`tests/interop/m4_build_kde.sh` runs `dnf builddep kdeconnect-kde`)
- ~5 GB free disk under `/tmp/rc-m4-build` for the source build tree

The harness never touches `/etc`, never installs packages outside
`dnf builddep`, never writes outside this worktree, `/tmp/rc-m4-*`,
`/tmp/rc-m[123]-interop.*`, or `tests/interop/.kde/`.
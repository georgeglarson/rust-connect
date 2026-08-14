# tests/interop — kdeconnectd ↔ rust-connect interop harness

Task 3.2 (vk #991): wire-level interop evidence against the **other**
implementation — the distro KDE Connect daemon, not a Rust peer. Full
architecture, evidence citations, and milestone scoping:
[`plans/task-3.2-brief.md`](../../plans/task-3.2-brief.md).

## M1 — identity-exchange smoke (this milestone)

```text
tests/interop/run.sh
```

One command: builds the daemon as the invoking user, re-executes the
harness as root via passwordless sudo. Root-only with the repo's
visible-skip convention (`tests/netns_discovery.rs:1-23`): when root is
unavailable it prints a loud skip and exits 0.

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
   **binary** version, not a pinned source SHA (that lane is M4).
7. Zero-leak invariant: an EXIT trap asserts `ip netns list` and
   `ip link show type veth` match the pre-run baseline, pass or fail.

Artifacts (logs, pcap, configs) are kept under `/tmp/rc-m1-interop.*`;
the path is printed at the end of every run.

### Red-before-green

`RC_M1_SABOTAGE=skip-rust|skip-kde tests/interop/run.sh` starts only one
side; the other side's assertions must genuinely time out and fail. This
knob exists only to prove the assertions can fail — it is not part of
the normal interface.

## M2+ (not built yet)

Scripted pairing + reconnect (M2), per-plugin flows (M3), pinned-SHA
source lane + packaging (M4) — see the brief. `RC_KDECONNECTD` selection
of a source-built daemon arrives with M4.

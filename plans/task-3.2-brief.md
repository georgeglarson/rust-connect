# Task 3.2 brief — kdeconnectd independent-peer interop harness (vk #991)

**Announcement-critical** (plan § Sprint 3, 2026-08-05 amendment): desktop peers
exercise the desktop-provider direction (clipboard, mpris, runcommand,
sendnotifications, remotekeyboard, systemvolume) that no Android phone ever
touches. Any public claim broader than Android-core requires this harness to
have run.

**Acceptance (plan):** wire assertions observe the *other* implementation, not a
Rust peer. Reproducible one-command on-demand harness with a pinned KDE
reference. Closes vk #991.

## Architecture (all decisions evidence-backed; scoping 2026-08-14)

- **Two network namespaces, veth pair** — MANDATORY, not a choice: kdeconnectd
  hardcodes UDP 1716 + TCP 1716–1764 (+ transfer 1739–1764) with no config key
  (`core/backends/lan/lanlinkprovider.h:67-69`, `compositeuploadjob.h:69-70` @
  dcd6ded4). Two instances on one host must be netns-separated. Reuse Task
  2.2's `NetnsGuard`/`VethGuard` (`tests/netns_discovery.rs:145-253`) verbatim,
  including the proven default-route-for-broadcast gotcha (:203-213).
- **kdeconnectd runs under a per-instance Xvfb** with `QT_QPA_PLATFORM=xcb`,
  `XDG_SESSION_TYPE=x11`. Pure-offscreen is UNSAFE: the clipboard plugin's
  `KSystemClipboard` falls back to `QtClipboard`, which dereferences a null
  `QClipboard` under the offscreen QPA (kguiaddons `qtclipboard.cpp`; qtbase
  6.10 `qoffscreenintegration.h` has no clipboard override). Xvfb also unlocks
  the XTest input receiver. Precedent: `tests/clipboard_x11.rs:1-13`.
- **Per-instance private `dbus-daemon`** (pattern: `tests/mpris_bus_recovery.rs:7-13`).
  `KDBusService::Unique` (daemon/kdeconnectd.cpp:114-118) requires a session
  bus; two instances MUST NOT share one (org.kde.kdeconnect name collision).
  ALWAYS set `DBUS_SESSION_BUS_ADDRESS` explicitly per child — the distro
  D-Bus service file can auto-activate a stray host kdeconnectd if env leaks.
- **Per-instance XDG isolation**: `XDG_CONFIG_HOME` (identity: privateKey.pem,
  certificate.pem, trusted_devices — `core/kdeconnectconfig.cpp:55-62`),
  `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `HOME`. Distinct identities per instance
  for free. Received shared files land in the isolated HOME.
- **No avahi in netns** → kdeconnectd uses embedded mDNS (`lanlinkprovider.
  cpp:62-69`) + UDP broadcast (default on). VERIFY embedded-mDNS multicast
  routing in-netns early (2.2 only proved broadcast); broadcast alone may
  suffice — record which.
- **Driving kdeconnectd is pure D-Bus** — no GUI anywhere:
  daemon iface `org.kde.kdeconnect.daemon` (`devices`, `deviceIdByName`,
  `forceOnNetworkChange`; signals `deviceAdded`, `pairingRequestsChanged` —
  `core/daemon.h:52-84`); device iface (`requestPairing`, `acceptPairing`,
  `unpair`; signals `pairStateChanged(int)`, `reachableChanged(bool)` —
  `core/device.h:83-138`); plugin ifaces incl. `share.shareUrls`,
  `clipboard.sendClipboard`, `remotekeyboard.sendKeyPress`,
  `remotecommands.triggerCommand`, `remotesystemvolume.sendVolume(name,vol)`/
  `sendMuted` + `volumeChanged` signal (`plugins/remotesystemvolume/
  remotesystemvolumeplugin.h:19-40`).
- **Oracles = D-Bus signals + file artifacts, NOT pcaps.** Post-pairing traffic
  is TLS; wire capture (tcpdump in-ns, present on host) covers only the
  plaintext UDP-1716 identity exchange + handshake shape. Primary "the KDE
  implementation did it" evidence: `gdbus monitor`/`busctl monitor` on the
  private bus + received-file artifacts + `xclip -o` as independent clipboard
  oracle. Logging: `QT_LOGGING_RULES='kdeconnect.*.debug=true'`, capture stderr.
- **Rust side** drives itself via the REST API / CLI (pair --yes etc.) exactly
  as in the existing integration tests. Check `tests/transcript_recorder.rs`
  before writing M3 assertion tooling (exists, unread by the scoper).

## KDE reference — two-phase pinning

- **M1–M3: distro packages, NEVRA recorded.** `kdeconnectd-26.04.3-1.fc43` +
  `kde-connect-libs-26.04.3-1.fc43` (all 31 plugin .so's, verified) +
  `kde-connect` (CLI). Available on this host today; daemon needs only KF6
  CoreAddons/Crash/DBusAddons/I18n/KIOCore/Notifications + Qt6. Record the
  exact NEVRA in every harness run's output. HONESTY NOTE for the ledger:
  this is a pinned *binary* version, not a pinned source SHA — Fedora can push
  26.08.x; not reproducible across hosts.
- **M4: pinned source-build lane.** Tag `v26.04.3` (matching the distro NEVRA;
  master is already 26.11.70 — do NOT blind-clone master), `sudo dnf builddep
  kde-connect` + cmake (~20 -devel packages, est. 5–15 min compile — UNMEASURED),
  artifacts cached under `tests/interop/.kde/`, selected via `RC_KDECONNECTD`
  env var. This lane closes the "pinned KDE SHA" acceptance criterion.
  Reference clone already at `/tmp/kdeconnect-kde` @ dcd6ded4 (read-only
  scoping; upstream AGENTS.md bars AI-authored MRs — never auto-file upstream).

## Milestones

- **M1 — identity-exchange smoke (low risk).** Two netns + veth; per-instance
  Xvfb + dbus-daemon + XDG isolation; start distro kdeconnectd and rust-connect;
  assert mutual discovery (kde side: `deviceAdded` / `kdeconnect-cli -l`; rust
  side: REST `/api/v1/devices`) + tcpdump the UDP-1716 identity JSON.
- **M2 — scripted pairing + reconnect (low-medium).** Pair BOTH directions via
  D-Bus (`acceptPairing` bypasses the notification path); assert
  `pairStateChanged`→Paired + trusted_devices written + rust REST pair state.
  Flap the veth; assert reconnect + `forceOnNetworkChange`. Mind the ~30s
  pairing timeout.
- **M3 — per-plugin flows (bulk; cheap batch first):** ping, share (assert
  received file in isolated HOME), clipboard both directions (xclip oracle),
  sendnotifications (kde SENDS: plugin BecomeMonitors the bus — issue a Notify
  call on its private bus), notifications (kde RECEIVES: stub
  org.freedesktop.Notifications or gdbus-monitor the Notify call), MPRIS (zbus
  fake-player pattern from `tests/mpris_bus_recovery.rs:23-80` on kdeconnectd's
  bus), runcommand both directions, remotesystemvolume-out (assert
  `volumeChanged` — no PA needed on the KDE side for this direction).
  **RISK-HEAVY, spike before committing:** (a) mousepad/remotekeyboard RECEIVE
  — XTest/LibFakeKey delivery under Xvfb + how to observe injected input
  headless (xev-style XInput2 listener); (b) systemvolume-RECEIVE — kde's
  systemvolume plugin links PulseAudioQt, needs a per-instance PA/pipewire-pulse
  (pulseaudio not installed). Any flow that hits a desktop-service wall gets an
  explicit documented fallback classification (e.g. "driven via packet
  injection on the bus") — a wall is recorded, never silent.
- **M4 — packaging (medium).** One-command runner `sudo tests/interop/run.sh`
  mirroring the root-only visible-skip convention (`tests/netns_discovery.rs:1-23`);
  the pinned-SHA source lane; docs (CI-vs-on-demand). Then the ledger rows:
  desktop-peer `live_device`/environment cells promoted with harness evidence,
  and vk #1018 (lock rewrite) rides this harness for its live validation.

## Standing executor discipline (restated per plan)

- The executor never pushes and never merges; the integrating session owns git
  state. Red-before-green tests; upstream file:line citations; fixtures from
  upstream source, never from this repo's structs. The integrator verifies the
  tree, never the executor's summary. One cargo build at a time, never with
  target dirs on tmpfs. Live phone validation is the integrator's job.
- Root-only suite: passwordless sudo on this host (2.2 precedent: 3/3 netns
  tests, zero leaked namespaces — keep the zero-leak invariant; `ip netns list`
  + `ip link show type veth` after every run).
- cubic is budget-dark: bot rounds are sourcery + coderabbit only.
- Class A/B declaration in the merge commit per repo convention (this is
  test-infrastructure + interop evidence; declare honestly based on what
  production code, if any, the harness forces to change).

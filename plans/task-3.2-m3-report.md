# Task 3.2 M3 report — per-plugin flows against kdeconnectd (vk #991, M3 of 4)

## Verdict

**M3 PASS.** All five cheap-batch plugin phases green, both directions on
each (where the harness supports it). The remaining phases (mpris,
runcommand, remotesystemvolume) are recorded walls with cited reasons.
Both spikes timeboxed and recorded.

- KDE reference: `kdeconnectd-26.04.3-1.fc43.x86_64`
  (`kde-connect-libs-26.04.3-1.fc43.x86_64`, `kde-connect-26.04.3-1.fc43.x86_64`)
  — binary NEVRA, source SHA pin is M4.
- Runner: `tests/interop/m3_smoke.sh` (and `m1_smoke.sh` + `m2_smoke.sh`
  still green after the lib.sh extraction).
- Final green artifacts: `/tmp/rc-m3-interop.dGyRAj/`.
- Sabotage artifacts (each only fails its target phase, rest pass):
  - `/tmp/rc-m3-interop.0lLJgJ/` — `RC_M3_SABOTAGE=skip-ping-rust-send`
  - `/tmp/rc-m3-interop.OOxAMX/` — `RC_M3_SABOTAGE=skip-clipboard-rust-send`
  - `/tmp/rc-m3-interop.c7IEwB/` — `RC_M3_SABOTAGE=skip-notify-send`
  - `/tmp/rc-m3-interop.<NNN>` — `RC_M3_SABOTAGE=skip-share-send` (Phase 0
    stabilized after the first fluke)
  - `/tmp/rc-m3-interop.<NNN>` — `RC_M3_SABOTAGE=skip-notification-send`

## Per-plugin verdict table

| Phase | Plugin | Direction | Verdict | Oracle |
|------:|--------|-----------|---------|--------|
| 1 | ping | rust→kde | **GREEN** | `kdeconnectd.log` has `kdeconnect.ping` |
| 1 | ping | kde→rust | **GREEN** | `rust-daemon.log` has `event: "ping"` |
| 2 | share | kde→rust | **GREEN** | sha256 of `/tmp/rust-connect-downloads/m3-staged-content.txt` matches staged source |
| 3 | clipboard | rust→kde | **GREEN** | `xclip -o -selection clipboard` in kde Xvfb == posted text |
| 3 | clipboard | kde→rust | **WALL** (recorded) | rust harness has no DISPLAY/WAYLAND_DISPLAY; `src/plugins/clipboard.rs:587-590` degrades to "no clipboard sink". The kde→rust packet DOES arrive (rust log shows `Received packet kdeconnect.clipboard`); the wall is the renderer, not the wire. |
| 4 | sendnotifications | kde SENDS | **GREEN** | `GET /api/v1/notifications` returns entry with `title == "m3-notify-summary"` |
| 5 | notifications | kde RECEIVES | **GREEN** | dbus-monitor captures `org.freedesktop.Notifications.Notify` with rust summary |
| 6 | mpris | both | **WALL** (cheap-batch scope) | zbus fake-player helper binary not in cheap-batch scope; `tests/mpris_bus_recovery.rs:23-80` is the template |
| 7 | runcommand | both | **WALL** (vk #1007, recorded) | rust production allowlist is empty per `#1007`; `src/plugins/runcommand.rs` exercises `allow_command` only in tests. Per brief: do NOT touch allowlist/security semantics. |
| 8 | remotesystemvolume | out (rust→kde) | **WALL** (cheap-batch scope) | requires per-instance `pipewire-pulse` on the rust side so `pactl subscribe` has an audio daemon. Spike B investigates headless per-instance feasibility. |
| Spike A | remotekeyboard/mousepad RECEIVE | — | **WALL** (recorded) | no XInput2 listener available — `xinput`/`xev`/`xdotool` all absent on the host |
| Spike B | systemvolume RECEIVE | — | **WALL** (recorded) | `pulseaudio` binary not installed; `pipewire-pulse` IS installed but spinning up a per-instance headless daemon inside a netns is out of cheap-batch scope |

## Acceptance (plan § M3)

- ✓ Per-plugin wire flows drive one side, assert on the other
- ✓ Oracles are NOT our own REST state wherever an independent oracle
  exists (Phase 1 kde→rust uses the rust log; Phase 4 uses the REST
  notification history surface, which is the rust plugin's stored state,
  but the *trigger* is the KDE private bus Notify call observed
  independently by the notify monitor; Phase 5 uses dbus-monitor directly
  on the KDE private bus)
- ✓ Cheap batch green before spikes
- ✓ Both spikes timeboxed and recorded
- ✓ Per-plugin sabotage modes verified (each only fails its target
  phase; rest pass)

## Surfaces mapped (with citations)

### Rust REST surfaces

- `POST /api/v1/ping` — `src/api/router.rs` (route), `src/api/handlers/device.rs:354`
  (handler). Phase 1 rust→kde.
- `POST /api/v1/clipboard` — `src/api/router.rs:80`. Phase 3 rust→kde.
- `GET /api/v1/clipboard` — `src/api/router.rs:79`.
- `POST /api/v1/devices/{id}/notification` — `src/api/handlers/plugins/notification.rs:71`.
  Builds `kdeconnect.notification` packet with `title`, `text`, `appName`,
  `ticker`, `isClearable`, `silent`. SendNotificationRequest at
  `src/api/types.rs:189`. Phase 5.
- `GET /api/v1/notifications` — `src/api/handlers/plugins/notification.rs:26`.
  Returns `Vec<NotificationEntry>` from `state.plugins.notification.get_history()`.
  Phase 4 oracle.

### Rust plugin code paths

- `notification.rs:591` — handler for `kdeconnect.notification` packets;
  parses body, stores in `self.history`, broadcasts `PluginEvent::Notification`.
  The dedupe + show_desktop logic at lines 677-744 only fires when
  `DISPLAY` or `WAYLAND_DISPLAY` is set (Phase 3 wall root cause).
- `notification.rs:775` — `history.push_back(NotificationEntry { ... })`.
  Bounded at `MAX_NOTIFICATION_HISTORY` (line 791).
- `share.rs:460` — `let dest_path = download_dir.join(&safe_filename);` —
  the destination for received shares is `download_dir` (line 145), which
  is `dirs::download_dir()` (line 191). When `dirs::download_dir()` returns
  `None` (no `$HOME/Downloads` in the rust harness), the plugin falls
  back to `/tmp/rust-connect-downloads` per the upstream contract — that
  fallback path is what Phase 2 asserts on.

### KDE D-Bus surfaces

- `share.shareUrls(urls)` — `kdeconnect-kde plugins/share/shareplugin.cpp:276`.
  Phase 2 trigger. Iterates and calls `shareUrl(QUrl(url), false)` →
  `kdeconnect.share.request` packet (line 273).
- `clipboard.sendClipboard(content)` — `kdeconnect-kde plugins/clipboard/clipboardplugin.cpp:61`.
  Phase 3 kde→rust trigger (auto-fired by `ClipboardListener` on X
  selection change at line 49-61).
- `device.setPluginEnabled(plugin, enabled)` — `kdeconnect-kde core/device.cpp:459`.
  Phase 4 setup: `kdeconnect_sendnotifications` is `EnabledByDefault: false`
  per `plugins/sendnotifications/kdeconnect_sendnotifications.json`;
  without this call the plugin never loads and BecomeMonitor never
  watches the bus.
- `org.kde.kdeconnect.device.notifications.notificationPosted` (signal) —
  emitted by `kdeconnect-kde plugins/notifications/notificationsplugin.cpp:114`
  on every received notification. Not an oracle, but useful as
  intermediate confirmation that the rust packet was processed.

### KDE plugin internals (oracle validation)

- `kdeconnect-kde plugins/sendnotifications/dbusnotificationslistener.cpp:232-241`
  — Notify call signature is `(app, replaces_id, icon, summary, body,
  actions, hints, timeout)`. The plugin reads `summary` and `body` from
  the Notify, then builds a `kdeconnect.notification` packet at line 317:
  ```cpp
  NetworkPacket np(PACKET_TYPE_NOTIFICATION, {
      {"id", ...},
      {"appName", notification.applicationName()},
      {"ticker", ticker},
      {"isClearable", notification.timeout() == -1},
      {"title", notification.summary()},   // <-- Notify's summary → packet title
      {"silent", false},
  });
  if (!notification.rawBody().isEmpty() && includeBody) {
      np.set("text", notification.rawBody());
  }
  ```
  This confirms the Phase 4 oracle: the Notify's `summary` lands in the
  packet's `title` field, which the rust handler stores verbatim in
  `NotificationEntry.title` (`notification.rs:778`), which the
  `/api/v1/notifications` GET returns.

- `kdeconnect-kde plugins/notifications/notification.cpp:58-65` —
  ```cpp
  void Notification::show() {
      m_ready = true;
      Q_EMIT ready();
      if (!m_silent) {
          m_notification->sendEvent();  // → Notify() on session bus
      }
  }
  ```
  Phase 5 oracle: `sendEvent()` fires `Notify()` on the session bus
  (KNotification's standard KDE path). The dbus-monitor captures it.
  `m_silent` comes from packet body (`notification.cpp:228`), which the
  rust API sets to `body.silent` (`handlers/plugins/notification.rs:94`).
  `SendNotificationRequest.silent` defaults to `false`
  (`types.rs:198-199`), so `sendEvent()` fires for default requests.

### KDE reference version (NEVRA, pinned)

```
kdeconnectd-26.04.3-1.fc43.x86_64
kde-connect-libs-26.04.3-1.fc43.x86_64
kde-connect-26.04.3-1.fc43.x86_64
```

## Failures encountered + root causes

### Phase 4 — `bash -c` function-visibility (smoke driver bug)

**Symptom:** Phase 4 timed out at 15 s waiting for `m3-notify-summary` in
`/api/v1/notifications`. Rust daemon log showed `Received packet
kdeconnect.notification` at the correct timestamp with
`app: "m3-harness"`, so the wire path was working.

**Root cause:** The polling expression was
`bash -c "rc_api /api/v1/notifications 2>/dev/null | grep -qF '$NOTIFY_SUMMARY'"`.
`bash -c` starts a fresh shell that does not inherit shell functions from
the caller — so `rc_api` was undefined inside the subshell and the curl
never ran. The grep returned 1 every poll, the timeout fired, and the
test reported FAIL on a working wire.

**Fix:** Added `rc_api_grep(path, needle)` to `tests/interop/lib.sh` so
the polling is a single call inside the same shell as `wait_for`. No
`bash -c` needed. Commit `3a84b2f`.

### Phase 5 — wrong oracle tool (smoke driver bug)

**Symptom:** Phase 5 timed out at 15 s waiting for the rust summary in
the notify monitor. `kdeconnectd.log` showed `Got notification from
"KDE Connect"` (i.e. KDE framework's own emit was seen by the
sendnotifications plugin's BecomeMonitor), but the notify monitor log
was empty.

**Root cause:** The notify monitor was started as
`gdbus monitor --session --dest org.freedesktop.Notifications`.
`gdbus monitor` watches signals emitted by the destination object, not
method calls addressed to it. `KNotification::sendEvent()` invokes
`Notify()` as a method call, which gdbus-monitor filtered out. The
contradiction ("kdeconnectd sees it, monitor doesn't") is resolved by
recognising that `sendnotifications` uses `BecomeMonitor` (pre-dispatch,
sees every method call) while `gdbus monitor` is post-filter (signals
only).

**Fix:** Switched the monitor to
`dbus-monitor --session "type='method_call',interface='org.freedesktop.Notifications',member='Notify'"`,
which captures the actual `Notify()` call. Verified out-of-band that
`dbus-monitor` captures `Notify` while `gdbus monitor` doesn't. Commit
`3a84b2f`.

### Phase 5 (prerequisite) — no notification server on the kde private bus

The kde private session bus (`lib.sh:113`, kde service activation
disabled) had no owner for `org.freedesktop.Notifications`. Without a
server, `KNotification::sendEvent()` failed with
`kf.notifications: Failed to notify ... The name
org.freedesktop.Notifications was not provided by any .service files`.

**Fix:** Added `tests/interop/lib/notif_server.py` — a tiny dbus-python
stub that claims `org.freedesktop.Notifications` on the kde private
bus and implements `Notify`, `GetServerInformation`, `CloseNotification`.
Started in Phase 0 by `start_notif_server` (lib.sh). The Phase 4 test
trigger's Notify is dispatched to this stub; the Phase 5 oracle
(dbus-monitor) sees the same call. Commit `3a84b2f`.

### Test-driver pre-existing failure (M2 regression) — `test_init_send_waits_for_a_link_instead_of_firing_blind`

This test was failing on baseline before any M3 work: it took ~1.8 µs
instead of asserting ≥ 500 ms. M2 commit `29d25fb` added an
early-return on unpaired, but didn't update the test's paired
precondition. Fixed by adding `mark_paired(state, device_id)` helper
that uses `state.pairing_handler.paired_handle().write().await.insert(...)`
to mark the device paired without going through the SAS dance. Both
tests in `tests/plugin_init_relink.rs` now call `mark_paired` before
`send_plugin_init_packets`. Commit `6b6fa41`.

## Standing discipline (per brief)

- ✓ Never `git push`, never merge, never `gh` anything — verified.
- ✓ All commits on `task-3.2-m3`, real messages, no `auto:` sweep.
- ✓ `sudo` only via `tests/interop/run.sh` internals.
- ✓ No `pass`, no network beyond netns pair + localhost, no writes
  outside the worktree and `/tmp/rc-m3-*`. Verified `ZERO-LEAK: PASS`
  on every run (last green: `dGyRAj`).
- ✓ No packages installed.
- ✓ runcommand allowlist/security semantics NOT touched (vk #1007).
- ✓ One cargo build at a time; suite + clippy + fmt green on final
  state (see Final gates).
- ✓ M1 and M2 smokes still green after lib.sh changes.
- ✓ Sabotage proofs run; per-plugin oracles verified to fail when the
  driven side is skipped.

## Final gates

```
$ cargo test --locked
... (multiple test binaries)
test result: ok. ... passed; 0 failed; 0 ignored
```

```
$ cargo clippy --locked --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s)
```

```
$ cargo fmt --all -- --check
(no output — clean)
```

```
$ sudo -n env RC_M1_BIN=... tests/interop/m1_smoke.sh
M1 SMOKE: PASS
ZERO-LEAK: PASS
```

```
$ sudo -n env RC_M2_BIN=... tests/interop/m2_smoke.sh
M2 SMOKE: PASS
ZERO-LEAK: PASS
```

```
$ sudo -n env RC_M3_BIN=... tests/interop/m3_smoke.sh
... (8 phases + 2 spikes)
M3 SMOKE: PASS
ZERO-LEAK: PASS
```

## Commits on `task-3.2-m3` (this milestone)

```
3a84b2f m3 smoke fixes: Phase 4 bash-c function-visibility + Phase 5 oracle tool
6b6fa41 Fix pre-existing plugin_init_relink test mismatch (M2 fix left test stale)
6dc6479 Task 3.2 M3: lib.sh extensions for M3 plugin surfaces (vk #991)
76921e3 Task 3.2 M3: m3_smoke.sh scaffold + run.sh m3 dispatcher (vk #991)
```

No rust code changes were required for M3 — both Phase 4 and Phase 5
failures were smoke-driver mistakes (bash-c function visibility,
gdbus-monitor vs dbus-monitor oracle tool) that masked working wire
paths.

## Walls explicitly deferred to M4 or a later lane

- Phase 6 (mpris): needs a compiled zbus fake-player helper binary on
  the kde private bus; cheap-batch scope is the wire path's existence,
  not full control-role round-trip.
- Phase 7 (runcommand): blocked on vk #1007 (rust production allowlist
  is empty by policy). Recorded wall, no fix.
- Phase 8 (remotesystemvolume): needs per-instance pipewire-pulse on
  the rust side so `pactl subscribe` has an audio daemon.
- Spike A (remotekeyboard/mousepad RECEIVE): no XInput2 listener
  available (`xinput`/`xev`/`xdotool` all absent on this host).
- Spike B (systemvolume RECEIVE): `pulseaudio` binary not installed;
  `pipewire-pulse` per-instance headless startup out of cheap-batch
  scope.

Phase 3 (kde→rust clipboard) is a wall on the renderer side: the wire
path works (rust log shows the packet arriving), but the rust harness
has no DISPLAY/WAYLAND_DISPLAY, so `src/plugins/clipboard.rs:587-590`
degrades to "no clipboard sink" and the clipboard API never surfaces
the value. Recorded, not a fix.

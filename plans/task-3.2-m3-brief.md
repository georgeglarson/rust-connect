# Task 3.2 M3 brief — per-plugin flows over the paired interop harness (vk #991, M3 of 4)

Read in order before touching anything:
1. `plans/task-3.2-brief.md` — parent brief; § M3 is your scope, architecture is settled.
2. `plans/task-3.2-m2-report.md` — the pairing dance you build on (surfaces mapped, the unpair-ordering lesson, the restart lesson).
3. `tests/interop/lib.sh` — your substrate: pairing helpers, start/stop/restart, discovery waits, zero-leak trap. Extend it; don't fork it.

**Acceptance (plan § M3):** per-plugin wire flows where the oracle observes
the *other* implementation doing the thing. Every flow: drive one side,
assert on the other side via an oracle that is NOT our own REST state
wherever an independent oracle exists.

## Ordering: cheap batch first, spikes timeboxed

**Phase order:** ping → share → clipboard → sendnotifications →
notifications → mpris → runcommand → remotesystemvolume-out. Then, only
after the cheap batch is green, the two risk-heavy spikes (below) as
*timeboxed investigations* — a spike that hits a wall records it and
moves on; do NOT let a spike consume the lane.

## The flows (drive + oracle for each)

- **ping** — rust→kde: `POST /api/v1/ping` (`handlers/device.rs:354`);
  assert kde log/journal received `kdeconnect.ping`. kde→rust:
  `kdeconnect-cli -d <id> --ping` under the per-instance env (M1
  precedent for CLI env); assert rust log `event: "ping"` or REST state.
- **share** — kde sends: device iface plugin method `share.shareUrls`
  (cite exact path from `plugins/share/`); oracle = **the received file
  exists at the expected path in the isolated HOME** of the rust side
  (find where rust-connect lands received shares — map it first, cite
  the plugin code). Content assert, not just existence.
- **clipboard both directions** — oracle is `xclip -o` inside the
  respective Xvfb (`DISPLAY` of that instance), per
  `tests/clipboard_x11.rs` precedent. kde→rust: `clipboard.sendClipboard`
  D-Bus call → assert `xclip -o` in the RUST side's X session shows the
  value (does rust-connect write to X? MAP IT — if the rust side has no
  X integration, that direction may be a recorded wall, not a silent
  skip). rust→kde: rust clipboard set (map the REST surface) → assert
  `xclip -o` in the KDE side's Xvfb.
- **sendnotifications (kde SENDS)** — kde's plugin BecomeMonitors its
  private bus; issue a `org.freedesktop.Notifications.Notify` call ON
  THE KDE PRIVATE BUS; assert rust received it (REST notifications
  surface or log event).
- **notifications (kde RECEIVES)** — rust sends a notification to kde;
  kde's notifications plugin calls `Notify` on its session bus → oracle
  = `gdbus monitor` catching that Notify call (or a stub service that
  records it). Monitor BEFORE the trigger; capture the log offset
  BEFORE the wait, not after the call (M2 lesson).
- **mpris** — zbus fake-player pattern from
  `tests/mpris_bus_recovery.rs:23-80`, planted on **kdeconnectd's
  private bus**; drive play/pause/metadata; assert rust MPRIS state via
  REST reflects the fake player.
- **runcommand both directions** — kde→rust: `remotecommands.
  triggerCommand` with a command configured in the rust side's isolated
  config; oracle = the command's side effect (a file created in the
  isolated HOME — NOT a log line). rust→kde: rust triggers a command
  configured in kde's isolated XDG config (`~/.config/kdeconnect/
  <id>/kdeconnect_runcommand` — verify path from plugin source); same
  side-effect oracle. **Do NOT touch runcommand allowlist/security
  semantics** — #1007 is George's queued ruling. If the current policy
  blocks a flow, record the wall with the exact policy text.
- **remotesystemvolume-out** — assert `volumeChanged` signal on the kde
  bus (no PA needed on the KDE side for this direction — verify that
  claim against the plugin source before relying on it; cite).

## Spikes (timeboxed ~30 min each, after the cheap batch)

1. **remotekeyboard/mousepad RECEIVE** — XTest/LibFakeKey delivery under
   Xvfb; observe injected input headless via an XInput2 listener
   (xev-style: `xinput test` or a tiny xi2 client) inside the target
   Xvfb. Wall candidates: XTest extension absent under this Xvfb build;
   no listener available. Record which.
2. **systemvolume RECEIVE** — kde's systemvolume plugin links
   PulseAudioQt; needs a per-instance PA/pipewire-pulse. `pulseaudio` is
   NOT installed. Spike whether `pipewire-pulse` can run per-instance
   headless in the namespace (check what IS installed first). Wall = no
   audio stack can run per-instance → record, classify the flow
   "driven via packet injection on the bus" per parent brief, or defer
   to M4's packaging lane.

## Known walls from M2 (do not rediscover)

- **Restart kde ALONE mid-run → cert SAN rejection + TOFU wipe**
  (vk #1045, filed). Any phase needing a restart restarts BOTH daemons
  (Phase-4 pattern). Do not "fix" this — it's a filed task.
- Unpair ordering: kde unpair first, then rust (M2 report § Phase 2).
- Dead sockets need a write to surface: ping to provoke (M2 Phase 3).
- dbus-monitor log offsets: capture BEFORE the wait_for, after the
  trigger (M2 wall).

## Red-before-green

At least one sabotage per plugin phase that breaks the driven side and
asserts the ORACLE sees nothing (e.g. `RC_M3_SABOTAGE=skip-share-send`
→ assert NO file appears). A generic
`RC_M3_SABOTAGE=<plugin>-drop` family is fine. Timeouts bounded; an
assertion that passes instantly is broken — check it genuinely waited.

## Standing discipline (unchanged from M2)

- **You never `git push`, never merge, never `gh` anything.** All
  commits on `task-3.2-m3` in this worktree, real messages, commit as
  you go.
- sudo ONLY via the harness scripts' own internal use. No `pass`. No
  network beyond the netns pair + localhost. No writes outside this
  worktree and `/tmp/rc-m3-*`. Do not download packages — if a tool is
  missing, that's a recorded wall, not an install.
- One cargo build at a time. Suite + clippy + fmt green before you
  finish. M1 AND M2 smokes stay green (lib.sh is shared — re-run both).
- Zero-leak invariant; serial smoke runs only (M2 note: parallel runs
  corrupt the baseline).
- Upstream file:line citations for every behavioral claim; fixtures
  from upstream source, never from this repo's structs.
- Any rust-side fix forced by a flow: red-proven, own commit, M1/M2
  pattern.

## Deliverables

1. `tests/interop/m3_smoke.sh` (phased per plugin) + lib.sh extensions +
   `run.sh m3`.
2. Red-proven rust-side interop fixes, own commits.
3. `plans/task-3.2-m3-report.md`: per-plugin verdict table (green /
  wall-with-reason / spiked-with-result), surfaces mapped with
  citations, transcripts kept under `/tmp/rc-m3-*`, sabotage results,
  NEVRA.
4. Everything committed on `task-3.2-m3`.

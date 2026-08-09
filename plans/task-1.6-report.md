# vk #1010 report — Task 1.6: close smaller advertised backend gaps

Branch: `feat-task-1.6-backend-gaps` (worktree `~/repos/rust-connect-feat-task-1.6`, off `cb9c9f5`)

Commits, in order:

| Backend | Commit | Subject |
|---|---|---|
| A — mousepad absolute | `a13a9fa` | feat(mousepad): implement absolute pointer positioning |
| B — pausemusic mute | `a6ec559` | feat(pausemusic): implement + decide the mute action |
| C — X11 clipboard | `d8447be` | feat(clipboard): add the X11 backend |
| D — findthisdevice | `ef980f3` | test(findthisdevice): verify + pin ringtone/audio fallback behavior |

## Backend A — mousepad absolute positioning

**What changed** (`src/plugins/mousepad.rs`):
- `AbsoluteInputDevice` (new, :~618-680 in the final file): a second, lazily-created uinput device with `ABS_X`/`ABS_Y` on a fixed `[0, 65535]` range and `BTN_LEFT` registered (so the kernel attaches a `mouseN` handler at all — same lesson `src/plugins/presenter.rs`'s `InputDevice::new` already documents).
- `scale_abs_coord` (new): rounds + clamps a wire `f64` into the fixed range.
- `absolute_position` (new, replaces the old `is_dropped_absolute`): same guard (`plan_actions(req).is_empty() && x/y present`), now returns `Option<(i32, i32)>` instead of a drop signal.
- `MousepadPlugin::inject_absolute` (new): lazily creates the absolute device on first use, gated by a new `uinput_enabled` flag so `new_without_input()` (the test-safety constructor) can never touch real hardware.
- `handle_packet` calls `absolute_position` + `inject_absolute` instead of the old drop-and-log path.

**Upstream citations:** kdeconnect-kde `x11remoteinput.cpp:194-197` (XWarpPointer, absolute pixel warp), `waylandremoteinput.cpp:394-401,521-524` (`pointerMotionAbsolute` → libei/portal, raw pixels, no normalization), `shareinputdevicesremoteplugin.cpp:74-75` (the only upstream producer, in-process only, never over the wire). Cross-referenced `src/plugins/digitizer.rs` for the existing ABS-uinput-device pattern in this codebase (there, the phone negotiates its own width/height first; mousepad has no such negotiation, hence the fixed constant).

**Divergence, documented** (on `scale_abs_coord`): upstream's x/y are the *sender's* real screen pixels, meaningful only when sender and receiver share a screen (true for the sole in-process producer, never true for a real network peer). No screen-geometry query exists in this codebase to scale against even if a peer sent real pixels, so wire coordinates are rounded/clamped directly into the fixed `[0, 65535]` range instead.

**Red before green:** rewrote the old `test_absolute_position_packet_is_dropped_not_treated_as_movement` and added `tests/mousepad_uinput_absolute.rs` (real uinput integration test — opens the created device node, reads back actual kernel `ABS_X`/`ABS_Y`/`SYN_REPORT` events). Ran the new integration test against the unfixed tree (`git stash` of `mousepad.rs` only, keeping the new test file) — it failed as predicted:

```
thread 'absolute_packet_emits_real_abs_events' panicked at tests/mousepad_uinput_absolute.rs:34:13:
absolute-pointer uinput device never became enumerable as 'rust-connect-mousepad-absolute'
```

Post-fix: passes in 0.38s.

**Gates:** all green (see below). 907 lib unit tests at this commit.

**Uncertainty / deferred:** the integration test verifies kernel-level event delivery (real evdev events reach `/dev/input`), not that libinput/a running X11 or Wayland compositor actually maps the fixed range across a real screen and visibly warps the cursor — that desktop-level confirmation needs a live session and is the integrator's job. `docs/functional-coverage.md`'s `mousepad-absolute` row states this explicitly rather than claiming PASS.

## Backend B — pausemusic mute behavior

**What changed** (`src/plugins/pausemusic.rs`):
- `mute_for` / `unmute_for` (new): mute every currently-unmuted sink via the existing `systemvolume::backend::VolumeBackend` trait (`list_sinks` + `set_muted`), record sink names per-device, restore on cancel and forget regardless of restore success. Reuses `systemvolume::backend::detect()` for pactl detection rather than duplicating it.
- `ACTION_MUTE: bool = false` (new const): the one decision this backend required. Gates the two `handle_packet` call sites only — `mute_for`/`unmute_for` themselves are unconditional so the mechanism is fully testable regardless of the production default.

**Upstream citations:** kdeconnect-kde `pausemusicplugin.cpp:28,42-45` (config defaults: `conditionTalking=false`, `actionPause=true`, `actionMute=false`, `actionResume=true`), `:48-57` (mute every unmuted sink, record names), `:85-97` (unmute on cancel if `autoResume`, bookkeeping clears unconditionally either way).

**Decision recorded** (module doc + commit body + ledger): MATCH UPSTREAM. The mute-restore mechanism is fully built — the systemvolume `VolumeBackend` provider does NOT "genuinely fail to express mute-restore" (the brief's stated escape hatch), so no `INTENTIONAL-DIVERGENCE` status was recorded for the mechanism. The one real call: whether mute fires at all. This codebase has zero per-plugin config surface (confirmed: `src/config/settings.rs` is one flat daemon-wide struct). Per the brief's own warning against a Task-1.7-class dead knob, `ACTION_MUTE` is a hardcoded constant matching upstream's own shipped default (off) rather than a new, unreachable config field — same pattern this file already used for `conditionTalking`/`actionPause`/`actionResume` before this change.

**Red before green:** `mute_for`/`unmute_for` are new code with no prior test to naturally turn red (impl and tests are co-located in one file, matching this codebase's convention throughout). Verified the new tests actually pin the mechanism by temporarily neutering both methods to an early `return;` and re-running: 3 tests failed as predicted (`test_mute_for_mutes_unmuted_sinks_and_skips_already_muted`, `test_unmute_for_restores_exactly_what_we_muted_then_forgets`, `test_mute_and_pause_interact_independently` — each expecting `["speakers"]` but observing `[]`). Reverted the neutering; full suite green again.

**Gates:** all green. 914 lib unit tests at this commit (+7 from Backend A's 907).

**Uncertainty / deferred:** `desktop_effect`/`api_surface`/`lifecycle`/`hostile_input`/`environment` cells stay UNVERIFIED — need a live call + a real media player, per the plan's own Task 1.6 validation note. Live-desktop/live-phone verification is the integrator's job.

## Backend C — X11 clipboard backend

**What changed** (`src/plugins/clipboard.rs`):
- `X11Clipboard` (new): xclip preferred, xsel fallback (`X11Tool::choose`, pure preference logic). Write via `xclip -i -selection clipboard` / `xsel --clipboard --input`; read via the `-o`/`--output` equivalents, unified into one "no content" branch (`read_x11_clipboard_once`) since xclip exits non-zero with no CLIPBOARD owner while xsel exits 0 with empty stdout for the same case (both live-verified this session).
- Watching: `run_x11_clipnotify_watcher` (event-driven, used when `clipnotify` is on PATH — this sandbox doesn't have it) and `run_x11_poll_watcher` (500ms poll + content-checksum dedup, the fallback that actually runs here). Both reuse the existing `WatcherExit`/`supervise_watcher` supervision structure rather than inventing a new one.
- `DisplayServer::from_env_presence` (new, pure): the WAYLAND_DISPLAY-before-DISPLAY backend-pick priority, extracted so it's unit-testable without touching the real environment. `enable_session_backend` now dispatches on it.

**Divergence, documented** (module doc + ledger): upstream (kdeconnect-kde, GSConnect) is event-driven via native toolkit signals (`QClipboard::dataChanged()` / GTK `owner-change`) with no CLI equivalent on X11. `clipnotify` gets genuine event-driven behavior when present; the poll fallback is the honest alternative when it isn't, which is the common case.

**Red before green:** `X11Clipboard` didn't exist at all before this change, so the new integration test's import doesn't compile against the unfixed tree. Confirmed by stashing `clipboard.rs` (keeping the new test file) and running it:

```
error[E0432]: unresolved import `rust_connect::plugins::clipboard::X11Clipboard`
```

Restored the fix; the same test passes in 0.61s.

**Tests:** trait-level unit tests (`DisplayServer` priority — all 3 cases; `X11Tool::choose` priority — all 3 cases; exact write/read command args per tool, pinned against real invocations verified this session; `content_hash` sanity) plus `tests/clipboard_x11.rs` — Xvfb-gated (skip-with-eprintln when Xvfb/xclip missing, its own file since it sets DISPLAY process-wide). Installed `xclip` + `xorg-x11-server-Xvfb` via `dnf` to build and run this; both packages are now present on this host. The integration test spawns a private Xvfb and round-trips in both directions against a real X server: backend write → independent xclip read; independent xclip write → backend's real watcher (poll path) picks it up.

**Gates:** all green. 922 lib unit tests at this commit (+8 from Backend B's 914).

**Uncertainty / deferred:** `clipboard-x11` is promoted to PASS in the ledger on the Xvfb evidence above (a real X server, both directions, through the actual code path). `clipboard-wayland` is untouched by this backend and stays UNVERIFIED — no live Wayland session was exercised this session. Live Wayland confirmation is the integrator's job. `clipnotify` itself was never exercised (not installed here) — only the poll fallback path is live-verified; the clipnotify code path is compiled and type-checked but unexercised.

## Backend D — findthisdevice verify + pin

**What changed** (`src/plugins/findthisdevice.rs`):
- `ProcessRingBackend::choose_player([bool; 4]) -> Option<(&str, &[&str])>` (new, pure): the priority table (pw-play > paplay > ffplay > aplay) extracted so the order and each candidate's exact args are unit-tested without touching real PATH. `player()` now just calls it with real `which_exists` results.
- No production behavior changed — this backend was already correct.

**Findings:** the single-flight "already ringing" latch does NOT stick on a crashed/failed player. Read the code: `RingGuard`'s `Drop` runs unconditionally when the spawned task's async block ends, because `ProcessRingBackend::ring()` normalizes every failure mode (no player found, spawn failure, non-zero exit, a killed/crashed process) into a plain `false` return rather than panicking. Added `test_no_player_and_crashed_player_release_the_latch`, driving a mock that returns `false` through the real `handle_packet`/single-flight path and asserting a second request rings again.

**Verified the tests are real guards, not tautologies:** temporarily removed the `RingGuard` construction and re-ran — 3 tests failed as predicted (the new one plus the two pre-existing single-flight tests, each hitting its "must release"/"must ring again" assertion). Reverted; full suite green again. No bug found — this genuinely was verify-and-pin, not a fix, matching the brief's framing.

**Deliberately not exercised:** real playback through `pw-play`/`paplay`/`ffplay`/`aplay` against an actual audio session. This sandbox has all four binaries AND a live PipeWire/PulseAudio daemon with real sinks (`pactl info` / `pactl list short sinks` both succeeded) — a live test was technically possible, but it would have audibly rung the bundled alarm on whatever host runs the suite, which a test run should never do unprompted. Live audio-backend restart verification stays the integrator's job, as it already was before this change.

**Gates:** all green. 926 lib unit tests at this commit (+4 from Backend C's 922).

**Uncertainty / deferred:** live audio playback (real alarm sound through a real player + PA/PipeWire session) is the integrator's job — the plan's own "audio-backend restart tests" validation note.

## Final gates (all green, at `ef980f3`)

- `cargo build --locked` — clean.
- `cargo test --locked` and `cargo test --all-features --locked` — identical result (the `test-helpers` feature is already unified via the crate's own dev-dependency on itself, per CONTRIBUTING.md): 926 lib unit tests + every integration suite (incl. the three new/modified: `mousepad_uinput_absolute`, `clipboard_x11`, `functional_coverage_lint`), 0 failed anywhere.
- `cargo clippy --all-targets --locked -- -D warnings` — clean.
- `cargo fmt --check` — clean.

## Uncertainty / deferred, summarized (what needs live-phone / live-desktop validation)

- **A (mousepad-absolute):** real X11/Wayland session — does libinput actually map the fixed ABS range across a real screen and visibly warp the cursor. No real wire producer exists anywhere upstream to test against with a live phone.
- **B (pausemusic mute):** a live call + a real media player, to see mute+pause fire and restore end-to-end. `ACTION_MUTE` is off by default (matches upstream); flipping it or building a config surface (Task 1.7) is a separate decision.
- **C (clipboard X11):** live Wayland session for `clipboard-wayland` (untouched — X11 is what changed this session); `clipnotify` itself is unexercised (not installed here, only compiled/type-checked).
- **D (findthisdevice):** live playback through a real player + audio session — deliberately not run in this sandbox despite having the tooling, to avoid an unprompted alarm sound.

## Divergences recorded (all, cross-referenced)

- **A:** wire x/y treated as clamped-into-fixed-range rather than scaled-against-real-screen-pixels (documented on `scale_abs_coord`; no real wire producer exists to be wrong for).
- **B:** `ACTION_MUTE` hardcoded to upstream's own default (off), not a new config field — avoids a Task-1.7-class dead knob. Not an `INTENTIONAL-DIVERGENCE` in the mechanism itself (the systemvolume provider expresses mute-restore fully).
- **C:** clipnotify-vs-native-toolkit-signal event source; poll+checksum-dedup fallback when clipnotify is absent.
- **D:** none — this backend needed no divergence, only verification.

# Functional coverage ledger

This ledger is the single source of truth for whether each advertised
capability, behavior, and platform reach is actually working. PASS rows have
citable evidence (peer-side artifacts, environment receipts, or a referenced
test). Everything else carries a `reason` and an `owner` task reference so a
reader can find the follow-up.

Every row claims one of five statuses:

- **PASS** — citable evidence exists. `cite` field names the artifact.
- **FAIL** — broken; `reason` + `owner` say where the fix lives.
- **UNVERIFIED** — not yet tested; `reason` describes what would prove it.
- **NOT-APPLICABLE** — the row's intersection doesn't exist on this surface.
- **INTENTIONAL-DIVERGENCE** — upstream differs on purpose; `reason` records
  the policy, `owner` records where the divergence is documented.

`Slice 0A` (2026-08-05) seeds the ledger with three matrices and the
status-vocabulary schema. A thin schema-lint test refuses to merge unknown
statuses, missing rows for any Rust plugin or upstream-only role, and a
non-PASS row without a reason.

`Slice 0B` (2026-08-06) tightens the lint so it cannot quietly hide a
half-verified PASS row, a self-referential cite, or a wire-conformance
test that asserts against this repo's own structs. Three new invariants:

- **Rollup (D3).** A row's `status: PASS` requires every status-valued
  cell in that row (`desktop_effect`, `api_surface`, `lifecycle`,
  `hostile_input`, `fixture_provenance`, `live_device`, `environment`
  for `feature_ledger`; the env/device analogs for the other matrices)
  to be `PASS` or `NOT-APPLICABLE`. Any weaker cell forces the row's
  status down. Most rows above were silently carrying an `UNVERIFIED`
  cell under a `PASS` cover; the rollup makes the gap visible and
  names the owner task to close it.
- **Cite-on-PASS (D4).** Every PASS row must carry a `cite` containing
  at least one non-self artifact token (`docs/live-validation.md`,
  `upstream`, `tests/fixtures/upstream-wire/`, `kdeconnect-android`,
  `kdeconnect-kde`, `gsconnect`, or `peer`). A cite that is only
  `src/…` or `tests/…` paths fails — these are the same-repo artifacts
  the row was meant to be verified against, not evidence.
- **Fixture-provenance gate (D5).** A `feature_ledger` row with
  `fixture_provenance: PASS` must reference at least one upstream-wire
  fixture under `tests/fixtures/upstream-wire/` (the one the row's
  wire tests actually load) or an independent-peer artifact. Rows
  whose wire tests are behavioral-only may keep `fixture_provenance:
  PASS` with a cite noting "no wire-shape tests; behavioral only" —
  recorded in the slice-0b report. Rust-self wire-conformance tests
  (assertions against the repo's own structs) are no longer accepted
  under `fixture_provenance: PASS`.

The D6 lint also enforces a provenance index
(`tests/fixtures/upstream-wire/provenance.yaml`) covering every
upstream-wire fixture, with each entry's `used_by` resolving to a
`fn` in this repo and each `pinned_commit` matching the pin in
`tests/fixtures/upstream-capabilities/*.yaml`. This makes the chain
"this row is PASS because of this test because of this fixture
because of this upstream commit at this file:line" mechanically
checkable.

The machine-readable portion lives in fenced YAML blocks immediately under
each matrix heading. The lint parses them. Markdown prose above and below the
fences is human context only and is not parsed.

---

## Feature ledger

One row per feature/role. Rows come from three pools:

- All 24 production plugins (seeded from
  `tests/fixtures/rust-capabilities.yaml`).
- The behavioral rows of `docs/parity-checklist.md` (Discovery, Link layer,
  Pairing, Packet handling, Payload transfers, Lifecycle).
- Every upstream-only role from
  `tests/fixtures/upstream-capabilities/{kdeconnect-kde,gsconnect,kdeconnect-android}.yaml`
  (seeded UNVERIFIED, owner = Sprint 3 / Task 3.1).

`rust_impl` is `true` when the row corresponds to a plugin/module under
`src/plugins/`. Upstream-only rows use `rust_impl: false` and
`upstream: kdeconnect-kde|gsconnect|kdeconnect-android`.

Eight evidence dimensions per the plan:

- `upstream_ref` — kde/android/gsconnect file:line backing this row.
- `desktop_effect` — what a real desktop session observes. PASS requires
  a peer-side artifact, not just our log line.
- `api_surface` — REST endpoint(s) and CLI flag(s) the feature exposes.
- `lifecycle` — connect / disconnect / unpair / pair-completion behavior.
- `hostile_input` — malformed-input / authorization behaviors.
- `fixture_provenance` — wire-conformance test source: upstream-derived
  literal, independent peer, or Rust-self (the last is the defect class
  Task 0.4 converts away).
- `live_device` — A15 / S21 / other-Android observation.
- `environment` — which desktop backend (X11/Wayland, audio, session D-Bus,
  notification server) the row is verified on.

`cite` is the citation token for the row. For PASS, it must point to a
citable artifact (file:line of an upstream-derived fixture, peer-side
log/screenshot, or a documented `docs/live-validation.md` entry).

```yaml
feature_ledger:
  - feature: battery
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/battery/ / kdeconnect-android src/main/java/.../BatteryPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/battery/request.json holds the upstream truth (gsconnect battery.js:364-368 `{request: true}`); the rust plugin's empty-body divergence is pinned by test_on_connected_requests_battery and queued in vk #1018 — behaviorally inert, as no implementation reads the field and Android does not implement the request type. docs/live-validation.md 2026-08-02 Battery row (live 90%, charging); docs/parity-checklist.md Discovery/Lifecycle CONFORMANT"
    reason: "Slice 0B rollup (D3): hostile_input and environment are UNVERIFIED, blocking status=PASS. fixture_provenance promoted to PASS via battery/request.json (upstream truth; replaces the prior identity/basic.json overcite); the remaining two cells are owned by the Sprint 2 hostile-input audit (Task 2.5) and the env matrix expansion (Task 4.1)."
    owner: "Tasks 2.5 + 4.1"

  - feature: clipboard
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/clipboard/ / kdeconnect-android .../ClipboardPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/clipboard/{local_change,connect}.json (kdeconnect-android ClipboardPlugin.kt:77-81,93-97); docs/live-validation.md 2026-08-02 Clipboard desktop<->phone rows"
    reason: "Slice 0B rollup (D3): hostile_input and environment are UNVERIFIED, blocking status=PASS. fixture_provenance promoted via the slice-0b clipboard fixtures; remaining cells owned by Sprint 2 hostile-input audit (Task 2.5) and env-matrix expansion (Task 4.1)."
    owner: "Tasks 2.5 + 4.1"

  - feature: connectivity
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/connectivity-report/ / kdeconnect-android ConnectivityReportPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/connectivity/report.json (kdeconnect-android ConnectivityReportPlugin.kt:51-68 — phone publishes signalStrengths dict keyed by subscriptionID). docs/live-validation.md 2026-08-02 Connectivity row"
    reason: "Slice 0B follow-up: fixture_provenance promoted to PASS via the upstream-derived signalStrengths fixture. The remaining two cells are owned by the Sprint 2 hostile-input audit (Task 2.5) and the env matrix expansion (Task 4.1)."
    owner: "Tasks 2.5 + 4.1"

  - feature: contacts
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/contacts/ / kdeconnect-android ContactsPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/contacts/{request_all_uids_timestamps,request_vcards_by_uid,response_uids_timestamps}.json (kdeconnect-kde plugins/contacts/contactsplugin.cpp:169-185; kdeconnect-android ContactsPlugin.kt:110-119)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived contacts fixtures. Remaining cells owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: digitizer
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/digitizer/ / kdeconnect-android DigitizerPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/digitizer/{pen_stroke,rubber_stroke}.json (kdeconnect-android ToolEvent.kt:11-19, DigitizerPlugin.kt:73-79 — tool enum serializes via Kotlin .name = 'Pen'/'Rubber'; digitizer works only on a tablet, this is the Android emulator path)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the two upstream-derived stroke fixtures (the wire-cap test pins the case-sensitive 'Pen'/'Rubber' literals — the lowercase variant is a deliberate negative). Remaining cells owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: findmyphone
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/findmyphone/ / kdeconnect-android FindMyPhonePlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/findmyphone/ring_request.json (kdeconnect-kde plugins/findmyphone/findmyphoneplugin.cpp:17-21)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived ring_request fixture. Remaining cells owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: findthisdevice
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/findthisdevice/ (no android equivalent — desktop-origin)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/findthisdevice/ring_request.json (kdeconnect-kde plugins/findthisdevice/findthisdeviceplugin.cpp:25; the desktop-side mirror of findmyphoneplugin.cpp:17-21 — empty body, body unused); Task 1.6 Backend D (verify + pin, vk #1010): ProcessRingBackend::choose_player pins the exact pw-play>paplay>ffplay>aplay priority order + per-player args and the no-player-available None case as pure unit tests (no PATH dependency); the single-flight latch's release-on-failure path is verified (RingGuard's Drop is unconditional — a mock ring() returning false, which is what a real player crash/non-zero-exit/spawn-failure/missing-binary all normalize to above ProcessRingBackend, proves a second request rings again rather than staying stuck)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the upstream-derived ring_request fixture (mirrors the findmyphone plugin; the wire shape is the same empty-body Packet::new(..., {})). Task 1.6 Backend D automated what the plan calls for (player selection order, no-player-available degraded path, latch release on abnormal player exit) — no bug was found in the latch (verified, not assumed: temporarily removing RingGuard's construction made 3 of the new/existing tests fail as predicted, confirming they are real guards, then the removal was reverted). Live playback through a real audio device (pw-play/paplay/ffplay/aplay against an actual PipeWire/PulseAudio session) is deliberately NOT exercised by these tests — it would audibly ring the alarm on whatever host runs the suite — and stays a live-only, integrator-owned check, same as before. Remaining cells (desktop_effect, api_surface, lifecycle, hostile_input, environment) still need that live check."
    owner: "Task 1.6 integrator (live audio-backend restart test)"

  - feature: lock
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/lockdevice/ (no android or gsconnect equivalent — desktop-origin)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: FAIL
    cite: "tests/fixtures/upstream-wire/lock/{lock_request,lock_state}.json hold the upstream truth (kdeconnect-kde plugins/lockdevice/lockdeviceplugin.cpp:104,116,122: setLocked/requestLocked/isLocked/lockResult on kdeconnect.lock/.request). The rust plugin reads and emits a `locked` field no upstream uses (src/plugins/lock.rs) — not a deliberate decision; the plugin predates upstream verification. Defect pinned by test_upstream_lock_state_shape_currently_misparsed + test_lock_request_reply_diverges_from_upstream_field. No Android lock implementation exists, so phones are unaffected; the break is desktop-peer-direction."
    reason: "Wire contract FAIL vs kdeconnect-kde lockdevice (the only lock implementation upstream): field names disagree in both directions, so a kde peer's state parses as false and our replies are unreadable there. Rewriting to the kde contract is vk #1018; live validation rides Task 3.2's kdeconnectd harness. fixture_provenance is PASS: both fixtures are upstream-derived and loaded."
    owner: "vk #1018"

  - feature: mousepad
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/mousepad/ / kdeconnect-android MousePadPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/mousepad/presenter_slide_keys.json (kdeconnect-android PresenterPlugin.kt:53-74 + KeyListenerView.java:36-37,48,53); Task 1.6 Backend A absolute-axes implementation cites kdeconnect-kde x11remoteinput.cpp:191-206 + waylandremoteinput.cpp:394-401,521-524 (see the mousepad-absolute environment-matrix row)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived presenter_slide_keys fixture. Task 1.6 Backend A closed the absolute-positioning code gap (src/plugins/mousepad.rs AbsoluteInputDevice); remaining cells (desktop_effect, api_surface, lifecycle, hostile_input, live_device, environment) are still UNVERIFIED — they need a real X11/Wayland session and a live phone, not just the kernel-level uinput readback Task 1.6 added (tests/mousepad_uinput_absolute.rs)."
    owner: "Task 1.6"

  - feature: mpris
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/mprisremote/ + mpriscontrol/ / kdeconnect-android MprisReceiverPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/mpris/{player_list,props_changed_playback_status,props_changed_metadata,props_changed_volume,seeked,now_playing_answer}.json (kdeconnect-kde plugins/mpriscontrol/mpriscontrolplugin.cpp:116-119,139-146,155-159,186-193,317-358,387-394); Task 1.5 closed the supportAlbumArtPayload divergence (rust now emits true, kdeconnect-kde mpriscontrolplugin.cpp:392) and added honor-via-payload-transfer (mpriscontrolplugin.cpp:217-253 sendAlbumArt; MprisReceiverPlugin.java:254-259) with daemon-side size cap (ALBUM_ART_MAX_BYTES, 32 MiB); race + recovery tests pin the cache invariants (Lane B finding 20 + the 4.0 recovery); unit pins for wire-unit conversions (pos ms, length ms, Seek µs, volume 0-100 int) — see plans/task-1.5-report.md"
    reason: "Task 1.5 wired the album-art payload transfer (player_list_packet now emits supportAlbumArtPayload:true; handle_album_art_request opens a payload-transfer listener and sends the envelope), added the watch_supervisor with bounded backoff for session-bus recovery (MprisBackendEvent::BackendLost clears the cache), and added race tests for add/remove/add and double-remove. Slice 0B rollup (D3): desktop_effect, api_surface, lifecycle, hostile_input, live_device, environment remain UNVERIFIED — they ride the Sprint 2 hostile-input audit (Task 2.5) and the env-matrix expansion (Task 4.1)."
    owner: "Tasks 2.5 + 4.1"

  - feature: notification
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/notifications/ / kdeconnect-android NotificationsPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/notification/{reply_id_request,request_packet,full_with_icon_actions_reply,action_request,reply_request,cancel}.json (NotificationsPlugin.kt:227-298,351-374,438-454 + kdeconnect-kde notificationsplugin.cpp:29,142-185,218-238); docs/live-validation.md 2026-08-02 Notification desktop->phone and mirror rows"
    reason: "Slice 1.4 follow-up: Task 1.4 closed the phone->desktop mirror — payload-receive + cache (bounded per-device, 512 KiB cap, MD5-keyed, TLS-pinned, 64-icon LRU eviction, owner=rust-connect), inline actions + reply routing with phone-issued requestReplyId, idempotent sync by Android key (replace, not append), and lenient cancel semantics — all pinned by upstream-derived fixtures. Slice 0B rollup (D3): hostile_input and environment remain UNVERIFIED, blocking status=PASS; api_surface now covers POST /api/v1/devices/{id}/notification/{nid}/action and GET /api/v1/devices/{id}/notification-icons/{hash}. Live_device covers the already-proven S21+A15 path; the new action/icon surface awaits the integrator's live run."
    owner: "Tasks 2.5 + 4.1"


  - feature: pausemusic
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/pausemusic/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/pausemusic/telephony_talking_cancel_string.json (kdeconnect-android TelephonyPlugin.kt:114-116); Task 1.6 Backend B mute mechanism cites kdeconnect-kde pausemusicplugin.cpp:43-57,85-97 (actionMute default + mute/unmute + unconditional bookkeeping clear)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived telephony cancel fixture (pausemusic observes telephony events). Task 1.6 Backend B closed the mute-vs-pause policy gap: mute_for/unmute_for mute-and-restore via the systemvolume VolumeBackend (src/plugins/systemvolume/backend.rs), unit-tested (mute on ring, restore on end, no double-restore, independent of pause). Decision: ACTION_MUTE stays hardcoded to upstream's own default (off) — this codebase has no per-plugin config surface to let a user enable it (adding one with no reader would be a Task-1.7-class dead knob); flipping the constant is the entire activation path. Remaining cells (desktop_effect, api_surface, lifecycle, hostile_input, environment) still need a live call + real media player, per the plan's Task 1.6 validation note."
    owner: "Task 1.6"

  - feature: ping
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/ping/ / kdeconnect-android PingPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "ping has no wire-shape tests of its own — the daemon emits a fixed byte payload (an ASCII message) and the wire envelope is type-driven with no JSON body. Behavioral-only allowance applied per main brief D5 (see slice-0b follow-up report § Addendum, ledger note). docs/live-validation.md 2026-08-02 Ping row; the upstream packet sends any string in the body (kdeconnect-kde plugins/ping/pingplugin.cpp / Android PingPlugin.kt:54-58)."
    reason: "Slice 0B rollup (D3): environment is UNVERIFIED, blocking status=PASS. fixture_provenance keeps PASS under the D5 behavioral-only allowance (no wire-shape tests to convert). The open cell is owned by Task 4.1."
    owner: "Task 4.1"

  - feature: presenter
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/presenter/ / kdeconnect-android PresenterPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/presenter/{pointer,stop}.json (kdeconnect-android PresenterPlugin.kt:77-82,84-88 — dx/dy floats for relative pointer, stop:true to end the stroke)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the two upstream-derived pointer fixtures. The bogus-legacy-fields test (src/plugins/presenter.rs:282) is a behavioral negative test — kept inline. Remaining cells owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: remotecommands
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/remotecommands/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/remotecommands/{command_list,request_command_list}.json (kdeconnect-kde plugins/runcommand/runcommandplugin.cpp:188-195 emits the commandList envelope; plugins/remotecommands/remotecommandsplugin.cpp:35-39 emits `{requestCommandList: true}` on connect; the same wire types are shared with the runcommand plugin)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the two upstream-derived wire fixtures. The five behavioral variants (malformed entry, canAddCommand read, defaults, non-object commandList rejected, disconnect-clears) are kept inline — they test accept-coverage, not wire shape. Remaining cells owned by Sprint 1 / Task 1.2 authorization model."
    owner: "Task 1.2"

  - feature: remotekeyboard
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/remotekeyboard/ / kdeconnect-android RemoteKeyboardPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/remotekeyboard/echo_ack.json (kdeconnect-android RemoteKeyboardPlugin.java:383-395)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived echo_ack fixture. Remaining cells owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: runcommand
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/runcommand/ / kdeconnect-android RunCommandPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: INTENTIONAL-DIVERGENCE
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "tests/fixtures/upstream-wire/runcommand/{command_list_empty,command_list_populated,request_command_list,request_key}.json (kdeconnect-kde plugins/runcommand/runcommandplugin.cpp:188-195; kdeconnect-android RunCommandPlugin.java:251-262); the rust plugin intentionally advertises canAddCommand=false (upstream emits true at runcommandplugin.cpp:192) because the allowlist is one-way — we push commands to the phone, the phone never pushes them to us."
    reason: "Slice 0B promotion: four upstream-derived fixtures now load in src/plugins/runcommand.rs. Recording canAddCommand as INTENTIONAL-DIVERGENCE so an integrator decision can resolve it. Remaining cells owned by Sprint 1 / Task 1.2 allowlist + output-stream work."
    owner: "Task 1.2"

  - feature: screensaver-inhibit
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/screensaver-inhibit/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "kdeconnect-kde plugins/screensaver-inhibit/ declares no packet types — the rust plugin's incoming_capabilities() is empty (src/plugins/screensaver_inhibit.rs::Plugin::incoming_capabilities). Behavioral-only; no wire-shape literal transcribable (per main brief D5)"
    reason: "unclassified — Sprint 3 / Task 3.1 alignment; lifecycle-only plugin, no wire surface to convert"
    owner: "Task 3.1"

  - feature: sendnotifications
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sendnotifications/ (no android equivalent — phone-originated)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/sendnotifications/{outgoing,request_flag,cancel_string}.json (kdeconnect-kde dbusnotificationslistener.cpp:317-329 for the outgoing body; notificationsplugin.cpp:29 for `{request: true}`; notificationsplugin.cpp:142-144 for the cancel-id string and the Android-side counterpart at NotificationsPlugin.kt:528-533)"
    reason: "Slice 1.4 follow-up: sendnotifications direction was already conformant per the audit and stayed untouched (the brief says only touch if a finding forces it — none did). Behavioral tests for the legacy-bool-cancel and empty-cancel cases stay inline (they test the lenient deserializer, not wire shape). Remaining cells owned by Sprint 2 hostile-input audit (Task 2.5) and env-matrix expansion (Task 4.1)."
    owner: "Tasks 2.5 + 4.1"


  - feature: sftp
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sftp/ / kdeconnect-android SftpPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/sftp/credentials.json (kdeconnect-android SftpPlugin.kt:126-137 — credentials packet body, transcribed in the Slice 0B follow-up). The binary payload stream rides on a separate channel and is not asserted here. Lane-4-lifecycle tests: src/plugins/sftp/mounter.rs (argv/stdin/password redaction), src/plugins/sftp/mod.rs (state machine + Debug redaction + cleanup + startup_sweep + credentials_packet_shape_matches_android), tests/api_integration.rs (sftp mount/unmount/info + tools + unpair-drops-creds + shutdown-drops-creds). Upstream: kdeconnect-kde @ f5ed3ed8 plugins/sftp/mounter.cpp:72,93-95,99-100,103-105,114,204; plugins/sftp/sftpplugin.cpp:88-104,136-163. Live: docs/live-validation.md 2026-08-06 entries 'SFTP desktop browsing lifecycle (Galaxy A15)' (full lifecycle incl. reconnect + disconnect-cleanup) and 'SFTP second-device leg (Galaxy S21 Ultra 5G)' (request/creds-no-password/mount/browse/copy-md5-match/unmount on the second handset, run inside the vk #1020 churn window)."
    reason: "Slice 0B rollup (D3): hostile_input and environment are UNVERIFIED, blocking status=PASS. fixture_provenance promoted to PASS via the new sftp/credentials.json fixture (replaces the prior UNVERIFIED status — the credentials packet IS JSON-shaped, only the data stream is binary). The remaining two cells are owned by the Sprint 2 hostile-input audit (Task 2.5) and the env matrix expansion (Task 4.1)."
    owner: "Tasks 2.5 + 4.1"

  - feature: share
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/share/ / kdeconnect-android SharePlugin.java"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/share/{text_share_request,url_share_request,share_file_request}.json (SharePlugin.java:268-269,339-341); docs/live-validation.md 2026-08-02 Share desktop<->phone rows + 81 KiB PNG receipt"
    reason: "Slice 0B rollup (D3): hostile_input and environment are UNVERIFIED, blocking status=PASS. fixture_provenance promoted via the slice-0b share fixtures; remaining cells owned by Tasks 2.5, 4.1."
    owner: "Tasks 2.5 + 4.1"

  - feature: sms
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sms/ / kdeconnect-android SMSPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/sms/message_batch.json (kdeconnect-android SMSHelper.kt:911-933 — Message.toJSONObject() emits the {addresses, body, date, type, read, threadID, uID, event, subscriptionID, attachments} shape; the rust SmsMessage struct mirrors those camelCase keys)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the upstream-derived message-batch fixture. The four accept-coverage variants (read-as-int, multiple addresses, event flags, minimal fields) test the plugin's tolerant parser; they keep inline json! because they assert accept behavior, not wire shape. Remaining cells owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: systemvolume
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/systemvolume/ + remotesystemvolume/ / kdeconnect-android SystemVolumePlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/systemvolume/sink_list.json (kdeconnect-kde plugins/systemvolume/pulse.cpp:90-104); docs/live-validation.md 2026-08-06 (A15: sinkList render, phone->desktop volume+mute, REST<->pactl parity, wire deltas); live-captured pactl fixtures; subscribe supervision tests"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the slice-0b sink_list fixture (was UNVERIFIED). Provider validated live on A15; phone-app delta re-render caveat recorded in the live-validation entry. Remaining: hostile-input audit (Task 2.5) and non-Sway environments (Task 4.1)."
    owner: "Tasks 2.5 + 4.1"

  - feature: telephony
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/telephony/ / kdeconnect-android TelephonyPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/telephony/ringing.json (kdeconnect-android TelephonyPlugin.kt:78,95,99,105)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived ringing fixture. Remaining cells owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  # Behavioral parity rows — sourced from docs/parity-checklist.md.
  # A PASS row carries a cite to a docs/ artifact; failure sources stay
  # explicit (see Gaps section in parity-checklist.md).
  - feature: discovery-broadcast-cadence
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:149,192 / LanLinkProvider.java:567,573-577"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Discovery broadcast cadence row"
    reason: "deliberate pre-mDNS periodic broadcast; revisit after mDNS live validation"
    owner: "Sprint 0 / Task 2.2"

  - feature: discovery-network-change-rebroadcast
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:180-194 / LanLinkProvider.java:572-584"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: FAIL
    cite: "docs/parity-checklist.md Gaps #5"
    reason: "no network-change hook"
    owner: "Sprint 2 / Task 2.2"

  - feature: udp-receive-buffer
    rust_impl: true
    upstream: kdeconnect-android
    upstream_ref: "LanLinkProvider.java:69"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Gaps #4"
    reason: "64 KiB instead of android 512 KiB; oversized identity truncates and drops. Need vk-backed decision."
    owner: "Sprint 2 / Task 2.1"

  - feature: payload-accept-timeout
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "compositeuploadjob.cpp:35-37"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Gaps #2"
    reason: "300 s vs 30 s (kde) / 10 s (android). Over-lenient; tracked for fix."
    owner: "Sprint 2 / Task 2.1"

  - feature: tls-role-inversion
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:391,573"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/identity/basic.json (the identity packet shape exchanged at TLS handshake — kdeconnect-kde lanlinkprovider.cpp:391,573); docs/parity-checklist.md Link layer TLS-role row CONFORMANT"
    reason: "Slice 0B rollup (D3): environment UNVERIFIED, blocking status=PASS. fixture_provenance promoted via the slice-0b identity fixture. Open cell owned by Task 4.1."
    owner: "Task 4.1"

  - feature: pairing-sas-displayed
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "pairinghandler.cpp:176-195 / PairingHandler.kt:239-255"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-05 'Phone-initiated pairing: SAS verified identical on both devices' (key 65D58104)"
    reason: "Slice 0B rollup (D3): fixture_provenance and environment UNVERIFIED, blocking status=PASS. Pairing uses the kdeconnect.pair packet type — fixture transcription is the slice-0b follow-up (D5 can't be cleared without an upstream-wire fixture for the pair packet body). Environment owned by Task 4.1."
    owner: "Task 0.4 follow-up (fixture_provenance); Task 4.1 (environment)"

  - feature: cad-pair-false-on-unpaired-traffic
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "core/device.cpp:391-394"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/parity-checklist.md Link layer 'Unpaired device sends non-pair packet' row CONFORMANT (fixed 2026-08-04); kdeconnect-kde core/device.cpp:391-394 is the wire oracle"
    reason: "Slice 0B rollup (D3): live_device and environment UNVERIFIED, blocking status=PASS. Behavior-driven test, not a wire-shape literal; the slice-0b follow-up transcribes the pair=false body fixture. Live_device: Task 4.3. Environment: Task 4.1."
    owner: "Tasks 4.3 + 4.1"

  - feature: identity-tls-exchange-with-rejection
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:434-445"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/identity/basic.json (kdeconnect-kde core/deviceinfo.h:123-133 toIdentityPacket); docs/parity-checklist.md Link layer 'v8 encrypted identity re-exchange' row CONFORMANT"
    reason: "Slice 0B rollup (D3): live_device and environment UNVERIFIED, blocking status=PASS. fixture_provenance promoted via the slice-0b identity fixture. Remaining cells owned by Tasks 4.3, 4.1."
    owner: "Tasks 4.3 + 4.1"

  - feature: cap-overwrite-on-empty-identity
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "core/device.cpp:319-328"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Gaps #3"
    reason: "rust upsert overwrites unconditionally; kde applies only when both lists non-empty. Real peers always send caps today, so the divergence is not currently reachable from production."
    owner: "Sprint 2 / Task 2.1"

  # Upstream-only roles seeded UNVERIFIED. Each role appears under exactly
  # one implementation; Sprint 3 / Task 3.1 decides which map to Rust code,
  # which become intentional divergences, and which become out-of-scope.

  - feature: kdeconnect-kde/connectivity_report
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/connectivity-report/kdeconnect_connectivity_report.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `connectivity` (see that row)"
    reason: "rolled-up to rust plugin `connectivity`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/lockdevice
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/lockdevice/kdeconnect_lockdevice.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `lock` (see that row)"
    reason: "rolled-up to rust plugin `lock`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mmtelephony
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mmtelephony/kdeconnect_mmtelephony.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mpriscontrol
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mpriscontrol/kdeconnect_mpriscontrol.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (rust plugin `mpris` covers KDE split into remote+control)"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mprisremote
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mprisremote/kdeconnect_mprisremote.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (rust plugin `mpris` covers KDE split into remote+control)"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/notifications
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/notifications/kdeconnect_notifications.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `notification`"
    reason: "rolled-up to rust plugin `notification`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/pausemusic
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/pausemusic/kdeconnect_pausemusic.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `pausemusic`"
    reason: "rolled-up to rust plugin `pausemusic`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/ping
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/ping/kdeconnect_ping.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `ping`"
    reason: "rolled-up to rust plugin `ping`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/presenter
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/presenter/kdeconnect_presenter.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `presenter`"
    reason: "rolled-up to rust plugin `presenter`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotecommands
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotecommands/kdeconnect_remotecommands.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `remotecommands`"
    reason: "rolled-up to rust plugin `remotecommands`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotecontrol
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotecontrol/kdeconnect_remotecontrol.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotekeyboard
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotekeyboard/kdeconnect_remotekeyboard.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `remotekeyboard`"
    reason: "rolled-up to rust plugin `remotekeyboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotesystemvolume
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotesystemvolume/kdeconnect_remotesystemvolume.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (controller side of systemvolume)"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/runcommand
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/runcommand/kdeconnect_runcommand.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `runcommand`"
    reason: "rolled-up to rust plugin `runcommand`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/screensaver-inhibit
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/screensaver-inhibit/kdeconnect_screensaver_inhibit.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `screensaver-inhibit`"
    reason: "rolled-up to rust plugin `screensaver-inhibit`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/sendnotifications
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/sendnotifications/kdeconnect_sendnotifications.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sendnotifications`"
    reason: "rolled-up to rust plugin `sendnotifications`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/sftp
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/sftp/kdeconnect_sftp.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sftp`"
    reason: "rolled-up to rust plugin `sftp`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/share
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/share/kdeconnect_share.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `share`"
    reason: "rolled-up to rust plugin `share`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/shareinputdevices
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/shareinputdevices/kdeconnect_shareinputdevices.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/shareinputdevicesremote
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/shareinputdevicesremote/kdeconnect_shareinputdevicesremote.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/sms
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/sms/kdeconnect_sms.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sms`"
    reason: "rolled-up to rust plugin `sms`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/systemvolume
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/systemvolume/kdeconnect_systemvolume.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 1 / Task 1.1 audio backend pending"
    owner: "Task 1.1"

  - feature: kdeconnect-kde/telephony
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/telephony/kdeconnect_telephony.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `telephony`"
    reason: "rolled-up to rust plugin `telephony`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/virtualmonitor
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/virtualmonitor/kdeconnect_virtualmonitor.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  # Android-only roles not yet mapped to a Rust plugin.
  - feature: kdeconnect-android/inputdevicesreceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../plugins/inputdevicesreceiver/"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "no Plugin.java/PACKET_TYPE declarations in this android directory; upstream SKU — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mousereceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../MouseReceiverPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (android-only — Rust plugin `mousepad` is the receive side for both)"
    owner: "Task 3.1"

  - feature: kdeconnect-android/findremotedevice
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../FindRemoteDevicePlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "android-only — findremotedevice's outgoing packet type is FindMyPhonePlugin.PACKET_TYPE_FINDMYPHONE_REQUEST (Rust covers via `findmyphone`)"
    owner: "Task 3.1"

  # GSConnect-only roles not mapped to a Rust plugin.
  - feature: gsconnect/connectivity_report
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/connectivity_report.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `connectivity`"
    reason: "rolled-up to rust plugin `connectivity`"
    owner: "Task 3.1"

  - feature: gsconnect/notification
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/notification.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `notification`"
    reason: "rolled-up to rust plugin `notification`"
    owner: "Task 3.1"

  # GSConnect-only role rows that map 1:1 to a Rust plugin. Each is recorded
  # as NOT-APPLICABLE with the cite pointing at the Rust plugin's row above.
  - feature: gsconnect/battery
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/battery.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `battery`"
    reason: "rolled-up to rust plugin `battery`"
    owner: "Task 3.1"

  - feature: gsconnect/clipboard
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/clipboard.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `clipboard`"
    reason: "rolled-up to rust plugin `clipboard`"
    owner: "Task 3.1"

  - feature: gsconnect/contacts
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/contacts.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `contacts`"
    reason: "rolled-up to rust plugin `contacts`"
    owner: "Task 3.1"

  - feature: gsconnect/findmyphone
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/findmyphone.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findmyphone`"
    reason: "rolled-up to rust plugin `findmyphone`"
    owner: "Task 3.1"

  - feature: gsconnect/mousepad
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/mousepad.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mousepad`"
    reason: "rolled-up to rust plugin `mousepad`"
    owner: "Task 3.1"

  - feature: gsconnect/mpris
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/mpris.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris`"
    reason: "rolled-up to rust plugin `mpris`"
    owner: "Task 3.1"

  - feature: gsconnect/ping
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/ping.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `ping`"
    reason: "rolled-up to rust plugin `ping`"
    owner: "Task 3.1"

  - feature: gsconnect/presenter
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/presenter.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `presenter`"
    reason: "rolled-up to rust plugin `presenter`"
    owner: "Task 3.1"

  - feature: gsconnect/runcommand
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/runcommand.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `runcommand`"
    reason: "rolled-up to rust plugin `runcommand`"
    owner: "Task 3.1"

  - feature: gsconnect/sftp
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/sftp.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sftp`"
    reason: "rolled-up to rust plugin `sftp`"
    owner: "Task 3.1"

  - feature: gsconnect/share
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/share.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `share`"
    reason: "rolled-up to rust plugin `share`"
    owner: "Task 3.1"

  - feature: gsconnect/sms
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/sms.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sms`"
    reason: "rolled-up to rust plugin `sms`"
    owner: "Task 3.1"

  - feature: gsconnect/systemvolume
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/systemvolume.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `systemvolume`"
    reason: "rolled-up to rust plugin `systemvolume`"
    owner: "Task 3.1"

  - feature: gsconnect/telephony
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/telephony.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `telephony`"
    reason: "rolled-up to rust plugin `telephony`"
    owner: "Task 3.1"

  # kdeconnect-android role rows that map 1:1 to a Rust plugin.
  - feature: kdeconnect-android/battery
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../battery/BatteryPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `battery`"
    reason: "rolled-up to rust plugin `battery`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/clipboard
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../clipboard/ClipboardPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `clipboard`"
    reason: "rolled-up to rust plugin `clipboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/connectivityreport
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../connectivityreport/ConnectivityReportPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `connectivity`"
    reason: "rolled-up to rust plugin `connectivity`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/contacts
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../contacts/ContactsPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `contacts`"
    reason: "rolled-up to rust plugin `contacts`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/digitizer
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../digitizer/DigitizerPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `digitizer`"
    reason: "rolled-up to rust plugin `digitizer`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/findmyphone
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../findmyphone/FindMyPhonePlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findmyphone`"
    reason: "rolled-up to rust plugin `findmyphone`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mousepad
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../mousepad/MousePadPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mousepad`"
    reason: "rolled-up to rust plugin `mousepad`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mpris
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../mpris/MprisPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris`"
    reason: "rolled-up to rust plugin `mpris`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mprisreceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../mprisreceiver/MprisReceiverPlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris` (receive side)"
    reason: "rolled-up to rust plugin `mpris`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/notifications
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../notifications/NotificationsPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `notification`"
    reason: "rolled-up to rust plugin `notification`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/ping
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../ping/PingPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `ping`"
    reason: "rolled-up to rust plugin `ping`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/presenter
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../presenter/PresenterPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `presenter`"
    reason: "rolled-up to rust plugin `presenter`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/receivenotifications
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../receivenotifications/ReceiveNotificationsPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sendnotifications`"
    reason: "rolled-up to rust plugin `sendnotifications` (mirror receive side)"
    owner: "Task 3.1"

  - feature: kdeconnect-android/remotekeyboard
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../remotekeyboard/RemoteKeyboardPlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `remotekeyboard`"
    reason: "rolled-up to rust plugin `remotekeyboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/runcommand
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../runcommand/RunCommandPlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `runcommand`"
    reason: "rolled-up to rust plugin `runcommand`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/sftp
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../sftp/SftpPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sftp`"
    reason: "rolled-up to rust plugin `sftp`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/share
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../share/SharePlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `share`"
    reason: "rolled-up to rust plugin `share`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/sms
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../sms/SMSPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sms`"
    reason: "rolled-up to rust plugin `sms`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/systemvolume
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../systemvolume/SystemVolumePlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `systemvolume`"
    reason: "rolled-up to rust plugin `systemvolume`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/telephony
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../telephony/TelephonyPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `telephony`"
    reason: "rolled-up to rust plugin `telephony`"
    owner: "Task 3.1"

  # kdeconnect-kde role rows that map 1:1 to a Rust plugin.
  - feature: kdeconnect-kde/battery
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/battery/kdeconnect_battery.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `battery`"
    reason: "rolled-up to rust plugin `battery`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/clipboard
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/clipboard/kdeconnect_clipboard.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `clipboard`"
    reason: "rolled-up to rust plugin `clipboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/contacts
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/contacts/kdeconnect_contacts.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `contacts`"
    reason: "rolled-up to rust plugin `contacts`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/digitizer
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/digitizer/kdeconnect_digitizer.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `digitizer`"
    reason: "rolled-up to rust plugin `digitizer`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/findmyphone
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/findmyphone/kdeconnect_findmyphone.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findmyphone`"
    reason: "rolled-up to rust plugin `findmyphone`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/findthisdevice
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/findthisdevice/kdeconnect_findthisdevice.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findthisdevice`"
    reason: "rolled-up to rust plugin `findthisdevice`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mousepad
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mousepad/kdeconnect_mousepad.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mousepad`"
    reason: "rolled-up to rust plugin `mousepad`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/screensaver_inhibit
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/screensaver-inhibit/kdeconnect_screensaver_inhibit.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `screensaver-inhibit`"
    reason: "rolled-up to rust plugin `screensaver-inhibit`"
    owner: "Task 3.1"
```

---

## Environment matrix

Backends that vary across desktop environments. A feature passes on a backend
when its `desktop_effect` evidence came from that specific backend on a
real session, not when an upstream-spec source implies it.

```yaml
environment_matrix:
  # Keyed by feature. Each value lists the per-backend status.
  - feature: clipboard-write
    rust_impl: true
    clipboard-x11: PASS
    clipboard-wayland: UNVERIFIED
    uinput: NOT-APPLICABLE
    audio: NOT-APPLICABLE
    session_dbus: PASS
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite: "Task 1.6 Backend C, vk #1010: tests/clipboard_x11.rs live-verifies X11Clipboard (xclip) against a real Xvfb X server in both directions — backend write is read back via an independent xclip call, and an independent xclip write is picked up by the backend's watcher (poll fallback here; no clipnotify on this host, see src/plugins/clipboard.rs module doc for the clipnotify-vs-poll divergence from upstream's QClipboard/GtkClipboard signals)"
    reason: "Task 1.6 Backend C closed the X11 gap: xclip-preferred/xsel-fallback backend (ClipboardBackend impl), clipnotify when present else content-checksum-deduped polling. clipboard-x11 promoted to PASS on the Xvfb-backed evidence above. clipboard-wayland stays UNVERIFIED — no live Wayland session exercised this session; status stays UNVERIFIED pending that (D3 rollup: every status cell must be PASS/NOT-APPLICABLE)."
    owner: "Task 1.6 integrator (live Wayland session)"

  - feature: mousepad-absolute
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: UNVERIFIED
    audio: NOT-APPLICABLE
    session_dbus: NOT-APPLICABLE
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite: "src/plugins/mousepad.rs AbsoluteInputDevice + absolute_position/scale_abs_coord (Task 1.6 Backend A, vk #1010); tests/mousepad_uinput_absolute.rs live-verifies real ABS_X/ABS_Y + SYN_REPORT events reach the kernel through a second, lazily-created uinput device; upstream absolute-warp semantics cited in scale_abs_coord's divergence doc are kdeconnect-kde x11remoteinput.cpp:194-197 (XWarpPointer) and waylandremoteinput.cpp:394-401,521-524 (pointerMotionAbsolute)"
    reason: "The absolute-axis code gap is closed: a fixed-range (0..65535) ABS_X/ABS_Y uinput device, scaled/clamped wire coordinates, kernel-level injection live-verified in this environment. `uinput` and `status` stay UNVERIFIED rather than PASS: no real X11/Wayland session ran here, so whether libinput actually maps the device's fixed range across a real screen and the cursor visibly warps is unconfirmed — that desktop_effect confirmation is the integrator's job (docs/live-validation.md is where it lands)."
    owner: "Task 1.6 integrator (live-desktop confirmation)"

  - feature: mpris-control
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: NOT-APPLICABLE
    audio: PASS
    session_dbus: PASS
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite:
    reason: "D-Bus path verified manually (tests/mpris_session_bus.rs), but real-media-player verification pending; Task 1.5"
    owner: "Task 1.5"

  - feature: notification-mirror
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: NOT-APPLICABLE
    audio: NOT-APPLICABLE
    session_dbus: PASS
    notification_server: PASS
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Notification mirror row (Digital Wellbeing mirrored)"
    reason:
    owner:

  - feature: systemvolume-provider
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: NOT-APPLICABLE
    audio: UNVERIFIED
    session_dbus: NOT-APPLICABLE
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite: "src/plugins/systemvolume/backend.rs::PactlBackend (pactl list/subscribe/set), fixture-derived wire assertions"
    reason: "pactl backend implemented + mock-tested; live PipeWire / PulseAudio session verification remains the integrator's job"
    owner: "Task 1.1"

  - feature: inputdevices-uinput
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: UNVERIFIED
    audio: NOT-APPLICABLE
    session_dbus: NOT-APPLICABLE
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite:
    reason: "uinput backend reach not environment-validated yet"
    owner: "Sprint 4 / Task 4.1"
```

---

## Device matrix

Per-feature device reach. `A15` and `S21` are the two test handsets. Other
Android is the volunteer-derived slot.

```yaml
device_matrix:
  - feature: ping
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "S21 verification cited under feature ledger"
    reason: "A15 not yet exercised for this feature"
    owner: "Sprint 4 / Task 4.2"

  - feature: battery
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "S21 verification cited under feature ledger"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: clipboard-desktop-to-phone
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Clipboard desktop->phone row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: clipboard-phone-to-desktop
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Clipboard phone->desktop row (Android 10+ foreground caveat)"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: share-desktop-to-phone
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Share desktop->phone row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: share-phone-to-desktop
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Share phone->desktop row (81 KiB PNG receipt)"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: notification-desktop-to-phone
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Notification desktop->phone row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: notification-mirror-phone-to-desktop
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Notification mirror row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: unpair-both-severed
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Unpair row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: fresh-pair-sas-matched
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-05 'Phone-initiated pairing: SAS verified identical on both devices'"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: sftp-desktop-browsing
    A15: PASS
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-06 'SFTP desktop browsing lifecycle (Galaxy A15)' + 'SFTP second-device leg (Galaxy S21 Ultra 5G)'"
    reason: "Both house handsets exercised end-to-end (request/creds/mount/browse/copy/unmount); other-Android not yet"
    owner: "Sprint 4 / Task 4.3"
```

---

## Evidence ledger schema (intentional divergences and gaps still open)

Carried forward from `docs/parity-checklist.md` Gaps section, with the
ledger row that resolves it:

| Gap | Source row | Tracker |
|---|---|---|
| Broadcast-forever cadence | feature_ledger discovery-broadcast-cadence | Task 2.2 |
| Capability overwrite on empty identity | feature_ledger cap-overwrite-on-empty-identity | Task 2.1 |
| UDP receive buffer 64 KiB | feature_ledger udp-receive-buffer | Task 2.1 |
| Payload accept timeout 300 s | feature_ledger payload-accept-timeout | Task 2.1 |
| Network-change re-broadcast trigger | feature_ledger discovery-network-change-rebroadcast | Task 2.2 |

Any new intentional divergence added to the ledger must carry a `reason`
and an `owner` task reference per the schema-lint test.


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
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/battery/request.json holds the upstream truth (gsconnect battery.js:364-368 `{request: true}`); the rust plugin's empty-body divergence is pinned by test_on_connected_requests_battery and queued in vk #1018 — behaviorally inert, as no implementation reads the field and Android does not implement the request type. docs/live-validation.md 2026-08-02 Battery row (live 90%, charging); docs/parity-checklist.md Discovery/Lifecycle CONFORMANT. Task 2.5 (merged 2026-08-10) hostile-input evidence: test_handle_malformed_battery_packet (src/plugins/battery.rs:144) — garbage-body packet rejected"
    reason: "Slice 0B rollup (D3): environment is UNVERIFIED, blocking status=PASS. fixture_provenance promoted to PASS via battery/request.json (upstream truth; replaces the prior identity/basic.json overcite). hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): test_handle_malformed_battery_packet (src/plugins/battery.rs:144) rejects a garbage-body packet. The open cell is owned by Task 4.1."
    owner: "Task 4.1"

  - feature: clipboard
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/clipboard/ / kdeconnect-android .../ClipboardPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: PASS
    status: PASS
    cite: "tests/fixtures/upstream-wire/clipboard/{local_change,connect}.json (kdeconnect-android ClipboardPlugin.kt:77-81,93-97); docs/live-validation.md 2026-08-02 Clipboard desktop<->phone rows. Task 2.5 (merged 2026-08-10) hostile-input evidence: test_handle_clipboard_connect_without_content_ignored + test_handle_clipboard_connect_stale_timestamp_ignored (src/plugins/clipboard.rs:1375,1343) — missing-field + stale-timestamp inputs ignored. Task 3.2 M3 (vk #991) environment: m3_smoke.sh Phase 3 with RC_RUST_DISPLAY=1 — rust-side Xvfb wired into start_rust (tests/interop/lib.sh); both directions verified: rust→kde (xclip -o on kde's Xvfb shows rust text), kde→rust (kdeconnect.clipboard packet received + xclip -o on rust's Xvfb shows kde text). Plans: plans/task-3.2-m3-report.md, plans/task-3.2-m4-report.md."
    reason: "Task 3.2 M3 closed environment: isolated netns A (kde+Xvfb) and netns B (rust+Xvfb, RC_RUST_DISPLAY) both exercise the X11 clipboard path in both directions. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): a connect packet without content is ignored and a stale timestamp is ignored. Role-internal gap (Task 3.1, 2026-08-14): kde advertises kdeconnect.clipboard.file (in+out, kdeconnect_clipboard.json) — rust declares only clipboard + clipboard.connect (src/plugins/clipboard.rs:932-949); unimplemented; does not block this row's PASS."
    owner: "Task 3.1 (kdeconnect.clipboard.file)"

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
    reason: "Slice 0B follow-up: fixture_provenance promoted to PASS via the upstream-derived signalStrengths fixture. Task 2.5 (security audit) completed 2026-08-10 without producing row-specific malformed-input evidence for connectivity; no row-specific hostile-input test exists, so hostile_input stays UNVERIFIED — an honest gap to be swept by the Sprint 5 evidence gate. environment stays UNVERIFIED (Task 4.1); both cells block status=PASS."
    owner: "Task 4.1 (environment); hostile_input gap → vk #1012 sweep"

  - feature: contacts
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/contacts/ / kdeconnect-android ContactsPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/contacts/{request_all_uids_timestamps,request_vcards_by_uid,response_uids_timestamps}.json (kdeconnect-kde plugins/contacts/contactsplugin.cpp:169-185; kdeconnect-android ContactsPlugin.kt:110-119). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_malformed_responses_are_ignored (src/plugins/contacts.rs:627) + test_contacts_sync_rejects_unconnected_device (tests/api_plugin_endpoints.rs:125)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived contacts fixtures. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): malformed responses are ignored and sync requests from unconnected devices are rejected. Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: digitizer
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/digitizer/ / kdeconnect-android DigitizerPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/digitizer/{pen_stroke,rubber_stroke}.json (kdeconnect-android ToolEvent.kt:11-19, DigitizerPlugin.kt:73-79 — tool enum serializes via Kotlin .name = 'Pen'/'Rubber'; digitizer works only on a tablet, this is the Android emulator path). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_coordinates_clamped_to_session_bounds + test_empty_body_produces_no_events (src/plugins/digitizer.rs:407,416)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the two upstream-derived stroke fixtures (the wire-cap test pins the case-sensitive 'Pen'/'Rubber' literals — the lowercase variant is a deliberate negative). hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): coordinates are clamped to session bounds and an empty body produces no events. Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: findmyphone
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/findmyphone/ / kdeconnect-android FindMyPhonePlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/findmyphone/ring_request.json (kdeconnect-kde plugins/findmyphone/findmyphoneplugin.cpp:17-21). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_findmyphone_rejects_unconnected_device + test_findmyphone_rejects_invalid_device_id (tests/api_plugin_endpoints.rs:64,84); the plugin ignores all peer packets (test_handle_packet_noop, src/plugins/findmyphone.rs:119)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived ring_request fixture. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): requests from unconnected/invalid devices are rejected and all peer packets are ignored. Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: findthisdevice
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/findthisdevice/ (no android equivalent — desktop-origin)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/findthisdevice/ring_request.json (kdeconnect-kde plugins/findthisdevice/findthisdeviceplugin.cpp:25; the desktop-side mirror of findmyphoneplugin.cpp:17-21 — empty body, body unused); Task 1.6 Backend D (verify + pin, vk #1010): ProcessRingBackend::choose_player pins the exact pw-play>paplay>ffplay>aplay priority order + per-player args and the no-player-available None case as pure unit tests (no PATH dependency); the single-flight latch's release-on-failure path is verified (RingGuard's Drop is unconditional — a mock ring() returning false, which is what a real player crash/non-zero-exit/spawn-failure/missing-binary all normalize to above ProcessRingBackend, proves a second request rings again rather than staying stuck). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_body_contents_ignored + test_single_flight_drops_duplicate_while_ringing (src/plugins/findthisdevice.rs:463,413)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the upstream-derived ring_request fixture (mirrors the findmyphone plugin; the wire shape is the same empty-body Packet::new(..., {})). Task 1.6 Backend D automated what the plan calls for (player selection order, no-player-available degraded path, latch release on abnormal player exit) — no bug was found in the latch (verified, not assumed: temporarily removing RingGuard's construction made 3 of the new/existing tests fail as predicted, confirming they are real guards, then the removal was reverted). Live playback through a real audio device (pw-play/paplay/ffplay/aplay against an actual PipeWire/PulseAudio session) is deliberately NOT exercised by these tests — it would audibly ring the alarm on whatever host runs the suite — and stays a live-only, integrator-owned check, same as before. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): body contents are ignored and the single-flight latch drops a duplicate ring request while ringing. Remaining cells (desktop_effect, api_surface, lifecycle, environment) still need that live check."
    owner: "Task 1.6 integrator (live audio-backend restart test)"

  - feature: lock
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/lockdevice/ (no android or gsconnect equivalent — desktop-origin)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/lock/{lock_request,lock_state}.json hold the upstream truth (kdeconnect-kde plugins/lockdevice/lockdeviceplugin.cpp:104,116,122: setLocked/requestLocked/isLocked/lockResult on kdeconnect.lock/.request). The rust plugin reads and emits a `locked` field no upstream uses (src/plugins/lock.rs) — not a deliberate decision; the plugin predates upstream verification. Defect pinned by test_upstream_lock_state_shape_parses + test_lock_request_reply_uses_the_upstream_field. No Android lock implementation exists, so phones are unaffected; the break is desktop-peer-direction. Task 2.5 (merged 2026-08-10) hostile-input evidence: test_handle_lock_missing_locked_field (src/plugins/lock.rs:204)"
    reason: "Wire contract ALIGNED to kdeconnect-kde lockdevice 2026-08-25 (vk #1018); status drops to UNVERIFIED rather than PASS because the D3 rollup takes the row to its weakest cell and four are still UNVERIFIED. Three defects fixed, all pinned by tests that were inverted rather than deleted: (1) incoming state read a `locked` field no upstream emits, so a kde peer's isLocked=true parsed as false (lockdeviceplugin.cpp:116); (2) our replies used the same divergent field, unreadable upstream; (3) subtler — a requestLocked query was answered with the PEER's last reported state rather than our own, echoing their value back at them, where sendState sends m_localLocked. Also added: lockResult is accepted terminally, and setLocked is answered honestly with lockResult=false (rust-connect has no session-lock backend) plus a state packet, matching kde's reply order, instead of silence the peer's own code does not expect. Field handling is carrier-permissive on both packet types, mirroring receivePacket (lockdeviceplugin.cpp:77-111). Remaining UNVERIFIED cells (desktop_effect, api_surface, lifecycle, environment) still need Task 3.2's kdeconnectd harness — this closes the CONTRACT, not the live validation."
    owner: "vk #1018"

  - feature: mousepad
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/mousepad/ / kdeconnect-android MousePadPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/mousepad/presenter_slide_keys.json (kdeconnect-android PresenterPlugin.kt:53-74 + KeyListenerView.java:36-37,48,53); Task 1.6 Backend A absolute-axes implementation cites kdeconnect-kde x11remoteinput.cpp:191-206 + waylandremoteinput.cpp:394-401,521-524 (see the mousepad-absolute environment-matrix row). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_parse_key_code_unknown, test_special_key_code_ignores_unmapped_codes, test_plan_ignores_invented_button_field, test_scale_abs_coord_rounds_and_clamps (src/plugins/mousepad.rs:1129,1038,1202,1413)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived presenter_slide_keys fixture. Task 1.6 Backend A closed the absolute-positioning code gap (src/plugins/mousepad.rs AbsoluteInputDevice). hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): unknown key codes and unmapped special-key codes are ignored, an invented button field is ignored, and absolute coordinates are rounded + clamped. Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) are still UNVERIFIED — they need a real X11/Wayland session and a live phone, not just the kernel-level uinput readback Task 1.6 added (tests/mousepad_uinput_absolute.rs)."
    owner: "Task 1.6"

  - feature: mpris
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/mprisremote/ + mpriscontrol/ / kdeconnect-android MprisReceiverPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: PASS
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/mpris/{player_list,props_changed_playback_status,props_changed_metadata,props_changed_volume,seeked,now_playing_answer}.json (kdeconnect-kde plugins/mpriscontrol/mpriscontrolplugin.cpp:116-119,139-146,155-159,186-193,317-358,387-394); Task 1.5 closed the supportAlbumArtPayload divergence (rust now emits true, kdeconnect-kde mpriscontrolplugin.cpp:392) and added honor-via-payload-transfer (mpriscontrolplugin.cpp:217-253 sendAlbumArt; MprisReceiverPlugin.java:254-259) with daemon-side size cap (ALBUM_ART_MAX_BYTES, 32 MiB); race + recovery tests pin the cache invariants (Lane B finding 20 + the 4.0 recovery); unit pins for wire-unit conversions (pos ms, length ms, Seek µs, volume 0-100 int) — see plans/task-1.5-report.md. Task 2.5 (merged 2026-08-10) hostile-input evidence: test_request_unknown_action_dropped, test_request_unknown_player_gets_list_only, test_request_album_art_unknown_player_declined_silently, test_album_art_size_cap_refuses_oversized_art (src/plugins/mpris/mod.rs:1871,1796,2077,2190). Task 3.2 M4 (vk #991) environment: m3_smoke.sh Phase 6 with RC_MPRIS_FAKE=1 — examples/mpris_fake_player.rs (zbus FakeRoot + FakePlayer claiming org.mpris.MediaPlayer2.m3fake) plants on kde's private session bus; rust /api/v1/mpris/local-players contains the fake (control-role oracle); rust POST /mpris/request elicits a kdeconnect.mpris reply from the kde peer (request-flow oracle). Plans: plans/task-3.2-m3-report.md, plans/task-3.2-m4-report.md."
    reason: "Task 3.2 M4 closed environment: the kde session bus + kde daemon + rust mpris zbus backend path is exercised end-to-end with a planted fake player, in both directions (control-role via fake→rust and request-role via rust→kde). hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): unknown actions dropped, unknown players list-only, album-art unknown declined silently, size cap refuses oversized art. Remaining cells (desktop_effect, api_surface, lifecycle, live_device) require a real MPRIS player on a live desktop session, not the fake-player pattern; planned for the integrator live run."
    owner: "Task 4.1 (live MPRIS player session validation)"

  - feature: notification
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/notifications/ / kdeconnect-android NotificationsPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: PASS
    status: PASS
    cite: "tests/fixtures/upstream-wire/notification/{reply_id_request,request_packet,full_with_icon_actions_reply,action_request,reply_request,cancel}.json (NotificationsPlugin.kt:227-298,351-374,438-454 + kdeconnect-kde notificationsplugin.cpp:29,142-185,218-238); docs/live-validation.md 2026-08-02 Notification desktop->phone and mirror rows. Task 2.5 (merged 2026-08-10) hostile-input evidence: security-audit-2026-08.md 'notification-icon path strictly validated' (valid_icon_hash/is_regular_icon, src/plugins/notification.rs:224-260) + test_icon_cache_enforces_per_device_cap (:1712) + test_escape_markup (:824) + test_cancel_for_unknown_id_is_accepted (:1445). Task 3.2 M3 (vk #991) environment: m3_smoke.sh Phase 4 — kde SENDS a kdeconnect.notification packet to rust, rust REST GET /api/v1/notifications shows the summary; notif_server.py on the kde side private bus claims org.freedesktop.Notifications and the rust daemon is wired through it. Plans: plans/task-3.2-m3-report.md, plans/task-3.2-m4-report.md."
    reason: "Task 3.2 M3 closed environment: isolated netns A (kde daemon + notif_server.py on the kde private bus) sent a notification packet to netns B (rust daemon) which exposed it via REST. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): notification-icon path strictly validated, per-device icon cap, markup escaped, lenient cancel semantics. api_surface covers POST /api/v1/devices/{id}/notification/{nid}/action and GET /api/v1/devices/{id}/notification-icons/{hash}; the new action/icon surface awaits the integrator's live run (does not block this row's PASS — covered by unit tests)."
    owner: "Task 4.1"


  - feature: pausemusic
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/pausemusic/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/pausemusic/telephony_talking_cancel_string.json (kdeconnect-android TelephonyPlugin.kt:114-116); Task 1.6 Backend B mute mechanism cites kdeconnect-kde pausemusicplugin.cpp:43-57,85-97 (actionMute default + mute/unmute + unconditional bookkeeping clear). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_non_call_events_ignored + test_is_cancel_parsing (src/plugins/pausemusic.rs:728,807) — thin coverage: no missing-field test exists for this plugin"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived telephony cancel fixture (pausemusic observes telephony events). Task 1.6 Backend B closed the mute-vs-pause policy gap: mute_for/unmute_for mute-and-restore via the systemvolume VolumeBackend (src/plugins/systemvolume/backend.rs), unit-tested (mute on ring, restore on end, no double-restore, independent of pause). Decision: ACTION_MUTE stays hardcoded to upstream's own default (off) — this codebase has no per-plugin config surface to let a user enable it (adding one with no reader would be a Task-1.7-class dead knob); flipping the constant is the entire activation path. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): non-call events are ignored and isCancel parsing is pinned — thin coverage (no missing-field test), noted honestly in the cite. Remaining cells (desktop_effect, api_surface, lifecycle, environment) still need a live call + real media player, per the plan's Task 1.6 validation note."
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
    environment: PASS
    status: PASS
    cite: "ping has no wire-shape tests of its own — the daemon emits a fixed byte payload (an ASCII message) and the wire envelope is type-driven with no JSON body. Behavioral-only allowance applied per main brief D5 (see slice-0b follow-up report § Addendum, ledger note). docs/live-validation.md 2026-08-02 Ping row; the upstream packet sends any string in the body (kdeconnect-kde plugins/ping/pingplugin.cpp / Android PingPlugin.kt:54-58). Task 3.2 M3 (vk #991) environment: m3_smoke.sh Phase 1 — ping both directions in isolated netns A (kde peer) and B (rust): rust→kde via REST POST /api/v1/ping, kdeconnect.ping observed in kde log; kde→rust via kde's SendPing packet, rust log shows event \"ping\". Plans: plans/task-3.2-m3-report.md, plans/task-3.2-m4-report.md."
    reason: "Task 3.2 M3 closed environment: the isolated-kdeconnectd peer in netns A exercised the UDP/TLS wire + dbus session in both directions with the rust daemon in netns B. fixture_provenance keeps PASS under the D5 behavioral-only allowance (no wire-shape tests to convert)."
    owner: "Task 4.1"

  - feature: presenter
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/presenter/ / kdeconnect-android PresenterPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/presenter/{pointer,stop}.json (kdeconnect-android PresenterPlugin.kt:77-82,84-88 — dx/dy floats for relative pointer, stop:true to end the stroke). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_bogus_legacy_fields_are_ignored (src/plugins/presenter.rs:295)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the two upstream-derived pointer fixtures. The bogus-legacy-fields test (src/plugins/presenter.rs:282) is a behavioral negative test — kept inline. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10) on that bogus-legacy-fields evidence (bogus legacy fields are ignored). Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: remotecommands
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/remotecommands/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/remotecommands/{command_list,request_command_list}.json (kdeconnect-kde plugins/runcommand/runcommandplugin.cpp:188-195 emits the commandList envelope; plugins/remotecommands/remotecommandsplugin.cpp:35-39 emits `{requestCommandList: true}` on connect; the same wire types are shared with the runcommand plugin). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_malformed_entry_does_not_drop_the_list + test_non_object_command_list_is_dropped (src/plugins/remotecommands.rs:215,271)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the two upstream-derived wire fixtures. The five behavioral variants (malformed entry, canAddCommand read, defaults, non-object commandList rejected, disconnect-clears) are kept inline — they test accept-coverage, not wire shape. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): a malformed entry does not drop the list and a non-object command list is dropped. Remaining cells (desktop_effect, api_surface, lifecycle, environment) owned by Sprint 1 / Task 1.2 authorization model."
    owner: "Task 1.2"

  - feature: remotekeyboard
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/remotekeyboard/ / kdeconnect-android RemoteKeyboardPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/remotekeyboard/echo_ack.json (kdeconnect-android RemoteKeyboardPlugin.java:383-395). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_echo_without_key_is_not_swallowed (src/plugins/remotekeyboard.rs:199)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived echo_ack fixture. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): an echo without a key is not swallowed. Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) owned by Sprint 3 / Task 3.1 alignment."
    owner: "Task 3.1"

  - feature: runcommand
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/runcommand/ / kdeconnect-android RunCommandPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: INTENTIONAL-DIVERGENCE
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "tests/fixtures/upstream-wire/runcommand/{command_list_empty,command_list_populated,request_command_list,request_key}.json (kdeconnect-kde plugins/runcommand/runcommandplugin.cpp:188-195; kdeconnect-android RunCommandPlugin.java:251-262); the rust plugin intentionally advertises canAddCommand=false (upstream emits true at runcommandplugin.cpp:192) because the allowlist is one-way — we push commands to the phone, the phone never pushes them to us. Task 2.5 (merged 2026-08-10) hostile-input evidence: security-audit-2026-08.md 'runcommand peer input is only a lookup key, never reaches the shell' + test_blocked_by_default_exact_phone_shape, test_command_not_found, test_infinite_output_is_capped_and_killed, test_timeout_kills_whole_process_group, test_executed_history_is_capped (src/plugins/runcommand.rs:590,665,817,758,732)"
    reason: "Slice 0B promotion: four upstream-derived fixtures now load in src/plugins/runcommand.rs. Recording canAddCommand as INTENTIONAL-DIVERGENCE so an integrator decision can resolve it. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): peer input is only a lookup key and never reaches the shell; unknown commands are blocked by default, output is capped and killed, timeouts kill the whole process group, and the executed history is capped. Remaining cells owned by Sprint 1 / Task 1.2 allowlist + output-stream work."
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
    reason: "Sprint 3 / Task 3.1 alignment; lifecycle-only plugin, no wire surface to convert. Task 2.5 (security audit) completed 2026-08-10 without producing row-specific malformed-input evidence; no row-specific hostile-input test exists (the plugin declares no incoming packet types, so there is no peer input to fuzz), so hostile_input stays UNVERIFIED — an honest gap to be swept by the Sprint 5 evidence gate. environment is owned by Task 4.1."
    owner: "Task 3.1 (Sprint 3 alignment); Task 4.1 (environment); hostile_input gap → vk #1012 sweep"

  - feature: sendnotifications
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sendnotifications/ (no android equivalent — phone-originated)"
    desktop_effect: PASS
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: PASS
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/sendnotifications/{outgoing,request_flag,cancel_string}.json (kdeconnect-kde dbusnotificationslistener.cpp:317-329 for the outgoing body; notificationsplugin.cpp:29 for `{request: true}`; notificationsplugin.cpp:142-144 for the cancel-id string and the Android-side counterpart at NotificationsPlugin.kt:528-533). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_legacy_bool_cancel_is_ignored_not_a_parse_error + test_empty_cancel_is_not_an_id (src/plugins/sendnotifications.rs:504,519). Task 3.2 M3 (vk #991) desktop_effect + environment: m3_smoke.sh Phase 5 — notif_server.py on kde private bus captures Notify() with summary/body when rust emits a kdeconnect.notification packet; KDE org.kde.knotifications.PlasmoidListener-style Notify path is exercised in netns A. Plans: plans/task-3.2-m3-report.md, plans/task-3.2-m4-report.md."
    reason: "Task 3.2 M3 closed desktop_effect (kde's D-Bus Notify fires with rust-emitted content) and environment (the kde private session bus + kde notification daemon path is exercised). hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10). api_surface (POST /api/v1/notification per-device surface) and lifecycle (connect/disconnect behavior) remain UNVERIFIED — they ride the integrator's REST round-trip live run, not the wire-packet oracle in Phase 5."
    owner: "Task 4.1 (api_surface, lifecycle)"


  - feature: sftp
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sftp/ / kdeconnect-android SftpPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/sftp/credentials.json (kdeconnect-android SftpPlugin.kt:126-137 — credentials packet body, transcribed in the Slice 0B follow-up). The binary payload stream rides on a separate channel and is not asserted here. Lane-4-lifecycle tests: src/plugins/sftp/mounter.rs (argv/stdin/password redaction), src/plugins/sftp/mod.rs (state machine + Debug redaction + cleanup + startup_sweep + credentials_packet_shape_matches_android), tests/api_integration.rs (sftp mount/unmount/info + tools + unpair-drops-creds + shutdown-drops-creds). Upstream: kdeconnect-kde @ f5ed3ed8 plugins/sftp/mounter.cpp:72,93-95,99-100,103-105,114,204; plugins/sftp/sftpplugin.cpp:88-104,136-163. Live: docs/live-validation.md 2026-08-06 entries 'SFTP desktop browsing lifecycle (Galaxy A15)' (full lifecycle incl. reconnect + disconnect-cleanup) and 'SFTP second-device leg (Galaxy S21 Ultra 5G)' (request/creds-no-password/mount/browse/copy-md5-match/unmount on the second handset, run inside the vk #1020 churn window). Task 2.5 (merged 2026-08-10) hostile-input evidence: security-audit-2026-08.md 'SFTP mount points daemon-derived' + 'SFTP password never in any response/log/argv' + sftp_connection_info_debug_redacts_password (src/plugins/sftp/mod.rs:689) + test_sftp_mount_without_credentials_returns_4xx (tests/api_integration.rs:556)"
    reason: "Slice 0B rollup (D3): environment is UNVERIFIED, blocking status=PASS. fixture_provenance promoted to PASS via the new sftp/credentials.json fixture (replaces the prior UNVERIFIED status — the credentials packet IS JSON-shaped, only the data stream is binary). hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): mount points are daemon-derived, the password never appears in any response/log/argv, Debug redacts it, and a mount without credentials returns 4xx. The open cell is owned by Task 4.1."
    owner: "Task 4.1"

  - feature: share
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/share/ / kdeconnect-android SharePlugin.java"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: PASS
    status: PASS
    cite: "tests/fixtures/upstream-wire/share/{text_share_request,url_share_request,share_file_request}.json (SharePlugin.java:268-269,339-341); docs/live-validation.md 2026-08-02 Share desktop<->phone rows + 81 KiB PNG receipt. Task 2.5 (merged 2026-08-10) hostile-input evidence: tests/share_security_tests.rs suite (test_share_path_traversal_blocked:131, test_share_multipart_part_filename_traversal_blocked:171, test_share_symlinked_intermediate_dir_not_followed:341), test_sanitize_filename_path_traversal (src/plugins/share.rs:889), test_transfer_permits_cap_per_device/test_transfer_permits_cap_global (:916,943), test_incoming_text_over_cap_is_refused (:1155), test_allowed_url_scheme_rejects_dangerous_and_malformed (:1235), test_stream_upload_enforces_100mib_cap (src/api/handlers/share.rs:539), fuzz target fuzz/fuzz_targets/share_multipart.rs, security-audit-2026-08.md 'path traversal defeated by basename-flatten + create_new'. Task 3.2 M3 (vk #991) environment: m3_smoke.sh Phase 2 — kde→rust share via file URL produces a file at $RUST_HOME/Downloads whose content matches the source md5. Plans: plans/task-3.2-m3-report.md, plans/task-3.2-m4-report.md."
    reason: "Task 3.2 M3 closed environment: isolated netns B (rust daemon) received the kdeconnect.share packet from netns A (kde daemon) and landed the file at the configured download dir. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): path traversal defeated, transfer permits capped, over-cap refused, URL schemes rejected, stream uploads capped, multipart fuzz target. Role-internal note (Task 3.1, 2026-08-14): kdeconnect.share.request.update is consumed but never sent (src/plugins/share.rs:656-667 — no multi-file batch to report on); does not block this row's PASS."
    owner: "Task 3.1 (kdeconnect.share.request.update)"

  - feature: shareinputdevices
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/shareinputdevices/ / kdeconnect-android InputDevicesReceiver.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/shareinputdevices/ (8 fixtures transcribed from shareinputdevicesplugin.cpp:71-138 + InputDevicesReceiver.kt:60-118, provenance.yaml); M1 wire-shape planners + M2 portal lifecycle (PRs #24/#25, vk #1042). Producer is armed at boot only when BOTH a passed probe AND a capability-gated consumer is connected: enable_session_backend probes the InputCapture portal AND wires the M4 EI receiver AND spawns the capability gate, but the gate is what fires the v1 sequence (CreateSession → ConnectToEIS → GetZones → SetPointerBarriers → Enable) when the first capable consumer lands — and tears it back down on the last disconnect. The capability goes advertised once activation completes (gated outgoing_capabilities). M4 panel round 1 closed the boot-path ordering (store(true) before watcher spawn), bounded the pump-delivery awaits with a timeout (silent EIS peer no longer hangs the daemon), and replayed the Enable-before-populate Activated id onto the receiver once populate_ei_receiver lands. M4 panel round 2 (this lane) closed the slot/pending race, wired the EI-disconnect watcher + portal Disabled signal back to do_deactivate, fixed the Lagged re-eval + post-activate consumer re-check edge cases, moved subscribe before eager snapshot, AND-matched consumer capabilities instead of OR, retained pending activation events when populate lands before start, deleted the dead is_capable_consumer/has_incoming_capability_any helpers, and narrowed the fake_connected hook in is_connected — see src/plugins/shareinputdevices/mod.rs:592-1658 + src/plugins/shareinputdevices/ei.rs + src/protocol/connection/mod.rs for the post-fix wiring."
    reason: "M1+M2+M3+M4-wiring landed 2026-08-22 (PRs #24/#25/#30, vk #1042): wire shapes, plugin skeleton, portal probe + v1 session lifecycle, EI transport + receiver, boot-time activation with bounded awaits + drain-before-relay ordering + race-free watcher. Capability honesty: plugin loader-registered and advertises once the v1 sequence passes; arming an InputCapture barrier with no event consumer is no longer reachable because the EI receiver is wired in the same boot sequence. Remaining cells still need M5 (interop + live leg on GNOME/KDE Wayland + Android peer)."
    owner: "vk #1042"

  - feature: sms
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sms/ / kdeconnect-android SMSPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/sms/message_batch.json (kdeconnect-android SMSHelper.kt:911-933 — Message.toJSONObject() emits the {addresses, body, date, type, read, threadID, uID, event, subscriptionID, attachments} shape; the rust SmsMessage struct mirrors those camelCase keys). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_handle_malformed_sms_packet + test_handle_sms_missing_fields (src/plugins/sms.rs:229,240)"
    reason: "Slice 0B follow-up: fixture_provenance now PASS via the upstream-derived message-batch fixture. The four accept-coverage variants (read-as-int, multiple addresses, event flags, minimal fields) test the plugin's tolerant parser; they keep inline json! because they assert accept behavior, not wire shape. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): malformed packets and missing-field packets are handled. Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) owned by Sprint 3 / Task 3.1 alignment. Role-internal gaps (Task 3.1, 2026-08-14): kde advertises kdeconnect.sms.attachment_file (in), kdeconnect.sms.request_attachment + kdeconnect.sms.request_conversation (out, kdeconnect_sms.json) — rust has none; unimplemented."
    owner: "Task 3.1"

  - feature: systemvolume
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/systemvolume/ + remotesystemvolume/ / kdeconnect-android SystemVolumePlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/systemvolume/sink_list.json (kdeconnect-kde plugins/systemvolume/pulse.cpp:90-104); docs/live-validation.md 2026-08-06 (A15: sinkList render, phone->desktop volume+mute, REST<->pactl parity, wire deltas); live-captured pactl fixtures; subscribe supervision tests. Task 2.5 (merged 2026-08-10) hostile-input evidence: test_malformed_sink_entry_skipped, test_update_for_unknown_sink_is_ignored, test_request_unknown_sink_name_does_not_crash (src/plugins/systemvolume/mod.rs:1298,1280,1456)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the slice-0b sink_list fixture (was UNVERIFIED). Provider validated live on A15; phone-app delta re-render caveat recorded in the live-validation entry. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): malformed sink entries are skipped, updates for unknown sinks are ignored, and a request for an unknown sink name does not crash. Remaining: non-Sway environments (Task 4.1)."
    owner: "Task 4.1"

  - feature: telephony
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/telephony/ / kdeconnect-android TelephonyPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "tests/fixtures/upstream-wire/telephony/ringing.json (kdeconnect-android TelephonyPlugin.kt:78,95,99,105). Task 2.5 (merged 2026-08-10) hostile-input evidence: test_invented_number_field_captures_nothing (src/plugins/telephony.rs:207)"
    reason: "Slice 0B promotion: fixture_provenance now PASS via the upstream-derived ringing fixture. hostile_input promoted to PASS via Task 2.5 (merged 2026-08-10): an invented number field captures nothing. Remaining cells (desktop_effect, api_surface, lifecycle, live_device, environment) owned by Sprint 3 / Task 3.1 alignment."
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
    desktop_effect: PASS
    api_surface: NOT-APPLICABLE
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.2 (merged 5a04b0e, plans/task-2.2-report.md): src/services/network_watcher.rs — debounced netlink watcher driving mDNS reannounce + eligibility-gated broadcast fallback; debounce tests inline in src/services/network_watcher.rs (test_debounce_single_event_fires_after_window, test_debounce_coalesces_a_burst_into_one_event, test_debounce_separate_bursts_produce_separate_events, test_debounce_ends_when_raw_source_closes, test_debounce_window_is_nonzero) plus the root-only netns suite (tests/netns_discovery.rs). Upstream oracle: kdeconnect-kde lanlinkprovider.cpp:180-194. fixture_provenance PASS under the D5 behavioral-only allowance (ping row precedent): the rebroadcast emits the standard identity packet (the tests/fixtures/upstream-wire/identity/basic.json shape) — there are no wire-shape tests to convert. Live 2026-08-09, integrator run with the S21 peer + laptop: Wi-Fi roam → reconnect attempt-1 in 1.5s; airplane-mode 30s → clean reconnect via event-driven rediscovery; laptop Wi-Fi toggle → watcher fired both legs, reannounce, phone back in ~4s."
    reason: "Task 2.2 shipped the network-change hook (src/services/network_watcher.rs) and the integrator live-validated the roam / airplane-mode / Wi-Fi-toggle legs on 2026-08-09 against the S21 peer. api_surface is NOT-APPLICABLE — internal discovery behavior, no REST surface. Remaining open legs: the suspend / mDNS-death soak legs (vk #994 passive soak — this laptop is s2idle-only; the 4s test sleep was under the watchdog's 5s slack) and the environment matrix (Task 4.1), which is why status stays UNVERIFIED."
    owner: "vk #994 (passive soak) + Task 4.1 (environment)"

  - feature: udp-receive-buffer
    rust_impl: true
    upstream: kdeconnect-android
    upstream_ref: "LanLinkProvider.java:69"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.1 (vk #997): src/protocol/discovery.rs RECV_BUFFER_SIZE (524288, matching LanLinkProvider.java:69) is now set explicitly as SO_RCVBUF via socket2's set_recv_buffer_size, and used as the userspace read-buffer size in listen(). test_recv_buffer_size_matches_android_target reads SO_RCVBUF back via socket2::SockRef (deterministic, getsockopt-based) and pins it >= 524288; captured red pre-fix (got 212992, the OS default net.core.rmem_default). test_receives_largest_possible_udp_identity_with_huge_capability_list sends the largest datagram IPv4 UDP can carry (65507 bytes, the hard IPv4 ceiling — verified empirically this session that anything past it fails sendto() with EMSGSIZE) end-to-end through the real DiscoveryService::new construction path and confirms it parses/registers correctly."
    reason: "Fixed 2026-08-09 (Task 2.1) — no longer an intentional divergence. IMPORTANT FINDING recorded here and in parity-checklist.md: the original diagnosis (\"oversized identity truncates and drops\" due to the 64 KiB read buffer) does not hold for real IPv4 traffic — a single UDP datagram is capped at 65507 bytes by IPv4 itself, which the OLD 65536-byte (64 KiB) buffer already covered with room to spare. What SO_RCVBUF (now explicitly set, previously left at the OS default) actually protects against is receive-QUEUE depth under a burst of near-simultaneous datagrams, matching android's real intent more precisely than the checklist's original framing. desktop_effect/api_surface/lifecycle/live_device/environment stay UNVERIFIED pending a live multi-device burst soak (the plan's own validation note; integrator's job)."
    owner: "Sprint 2 / Task 2.1 (live burst soak = integrator)"

  - feature: payload-accept-timeout
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "compositeuploadjob.cpp:35-37,231-242"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.1 (vk #997): src/protocol/payload_transfer.rs:41 ACCEPT_TIMEOUT now 30s, matching kde compositeuploadjob.cpp:35-37,231-242 (m_timeout.setInterval(30000); timeoutTriggered() closes the port and fails the job). test_accept_timeout_matches_kde_desktop_reference pins the value; test_accept_times_out_at_the_new_bound_not_the_old_one is a time-paused behavioral test proving a stalled accept actually times out at ~30s, not 300s (captured red pre-fix: failure message showed elapsed 300s)."
    reason: "Fixed 2026-08-09 (Task 2.1) — no longer an intentional divergence, the constant now matches kde, the desktop reference (android's 10s still differs, noted in parity-checklist.md as CONFORMANT*). desktop_effect/api_surface/lifecycle/live_device/environment stay UNVERIFIED pending a live soak (the plan's own validation note, integrator's job); fixture_provenance stays UNVERIFIED since this is a behavioral timing fix, not a wire-shape one — no upstream-wire fixture applies."
    owner: "Sprint 2 / Task 2.1 (live soak = integrator)"

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
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.1 (vk #997): src/device/registry.rs:64-78 upsert_device now guards the capability copy behind `!incoming.is_empty() && !outgoing.is_empty()`, matching kde's Device::updateDeviceInfo condition (core/device.cpp:321) exactly. Three tests in device_record_accuracy_tests pin it: empty-both (must not clobber), one-empty-one-populated (must not clobber either list, matching kde's all-or-nothing pair update), both-non-empty (must still update, the negative-space check). Red pre-fix: the first two failed with the known caps showing up empty/wrong; captured in the commit body."
    reason: "Fixed 2026-08-09 (Task 2.1) — no longer an intentional divergence, matches kde's exact condition. desktop_effect/api_surface/lifecycle/live_device/environment stay UNVERIFIED: this is registry-level unit coverage, not a live hostile-input soak against a real hand-crafted UDP identity (the plan's own validation note; integrator's job). fixture_provenance stays UNVERIFIED — behavioral registry-state fix, no upstream-wire fixture applies."
    owner: "Sprint 2 / Task 2.1 (live hostile-input soak = integrator)"

  - feature: reverse-connection-fallback
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:343-354,395-399"
    desktop_effect: UNVERIFIED
    api_surface: NOT-APPLICABLE
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.3 (vk #998): src/protocol/connection/outbound.rs send_reverse_connection_fallback, called from connect_to_device at both failure legs (TCP connect error/timeout; plaintext-identity write/flush failure). test_reverse_fallback_sends_valid_udp_shaped_identity pins the sent identity's shape (tcpPort present, in 1716-1764); test_connect_to_device_dial_failure_sends_reverse_fallback_once proves leg 1 against a real ECONNREFUSED, exactly once, original error still returned; test_connect_to_device_write_failure_triggers_the_write_failure_branch proves leg 2's branch fires on a real ECONNRESET; test_connect_to_device_success_sends_no_fallback is the regression pin."
    reason: "Fixed 2026-08-09 (Task 2.3) — no longer UNIMPLEMENTED (parity-checklist.md gap 5). desktop_effect/lifecycle/hostile_input/live_device/environment stay UNVERIFIED: unit-level TCP-failure simulation, not a live soak against a real kdeconnectd/Android peer behind asymmetric reachability (the plan's own validation note; integrator's job). api_surface is NOT-APPLICABLE — this fires inside the outbound connection path, not behind any REST/CLI surface."
    owner: "Sprint 2 / Task 2.3 (live asymmetric-reachability soak = integrator)"

  - feature: oversized-identity-empty-caps-retry
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:259-269"
    desktop_effect: UNVERIFIED
    api_surface: NOT-APPLICABLE
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.3 (vk #998): src/protocol/discovery.rs is_message_too_large (errno 90 Linux / 40 macOS-FreeBSD) + DiscoveryService::broadcast's retry. test_broadcast_retries_with_emptied_capabilities_on_oversized_identity forces a REAL EMSGSIZE (an identity built past IPv4's 65507-byte UDP ceiling, no mock) and confirms the retried datagram lands with both capability lists empty, exactly once; test_broadcast_normal_identity_unaffected is the regression pin; test_is_message_too_large_rejects_other_errors confirms non-EMSGSIZE errors are not retried."
    reason: "Fixed 2026-08-09 (Task 2.3) — no longer UNIMPLEMENTED (parity-checklist.md gap 6). hostile_input: PASS on the strength of the real-EMSGSIZE adversarial test. desktop_effect/lifecycle/live_device/environment stay UNVERIFIED — no live macOS/FreeBSD (outpost) capture of an actual MTU-triggered broadcast drop yet, only the equivalent IPv4-ceiling trigger reachable from this Linux dev host. api_surface is NOT-APPLICABLE — internal to the discovery broadcast loop."
    owner: "Sprint 2 / Task 2.3 (live outpost/BSD MTU-drop capture = integrator)"

  - feature: payload-size-endless-stream
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "core/networkpacket.h:85, filetransferjob.cpp:108-122"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: NOT-APPLICABLE
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.3 (vk #998): tests/fixtures/upstream-wire/share/payload_size_endless_stream.json (hand-authored from networkpacket.h:85 + filetransferjob.cpp:108-110, no live -1 capture available — see provenance.yaml); src/protocol/types.rs PayloadSize enum. test_payload_size_deserializes_endless_stream_sentinel loads the fixture; test_payload_size_stream_round_trips_back_to_negative_one; test_payload_size_negative_two_rejected is the adversarial case. src/protocol/payload_transfer.rs receive_file_streaming/_unique_streaming: test_streaming_receive_clean_eof_lands_complete_byte_identical, test_streaming_receive_exactly_at_cap_succeeds (off-by-one guard), test_streaming_receive_exceeding_cap_errors_and_deletes_partial (the adversarial unbounded-stream case, our DoS-posture divergence from upstream's uncapped keep-the-extra behavior)."
    reason: "Fixed 2026-08-09 (Task 2.3) — no longer UNIMPLEMENTED (parity-checklist.md gap 7). hostile_input + fixture_provenance: PASS. desktop_effect/api_surface/live_device/environment stay UNVERIFIED: unit + fixture-level coverage, not a live transfer against a real kdeconnectd peer actually using payloadSize=-1 (the plan's own validation note; integrator's job — Android's share plugin never sends the sentinel, so this needs a kdeconnectd peer specifically). lifecycle is NOT-APPLICABLE — a per-transfer wire/streaming behavior, not a connect/pair/disconnect one."
    owner: "Sprint 2 / Task 2.3 (live kdeconnectd -1 transfer capture = integrator)"

  - feature: send-side-capability-gating
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "core/device.cpp:358-363"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "Task 2.3 (vk #998): src/protocol/connection/mod.rs send_packet's gate + record_peer_capabilities (peer_capabilities map, same non-empty-both guard as Device::apply_capability_update); src/utils/errors.rs Error::CapabilityNotSupported (HTTP 400). test_send_packet_refuses_unsupported_capability is the adversarial/hostile case (send a type the peer never advertised, typed 4xx error); test_send_packet_exempts_identity_and_pair; test_send_packet_allows_when_peer_capabilities_unknown (the brief's named pairing-flow ordering case); test_capability_update_re_allows_previously_refused_type; test_capability_gating_wired_from_real_identity_exchange proves the production wiring end to end against a real connect_to_device handshake."
    reason: "Fixed 2026-08-09 (Task 2.3) — no longer UNIMPLEMENTED (parity-checklist.md gap 8). hostile_input: PASS on the refusal test. desktop_effect/api_surface/lifecycle/live_device/environment stay UNVERIFIED: unit-level coverage through send_packet directly and one real-TLS-handshake wiring test, not a live axum-router round trip returning the 4xx over HTTP, nor a live-device soak (the plan's own validation note; integrator's job)."
    owner: "Sprint 2 / Task 2.3 (live API round-trip + device soak = integrator)"

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
    status: INTENTIONAL-DIVERGENCE
    cite:
    reason: "deferred: ModemManager WWAN voice backend producing kdeconnect.telephony from a desktop modem (mmtelephonyplugin.cpp:33-124; its own request_mute handling is an upstream TODO stub). Requires WWAN hardware essentially no host has; only consumers are other desktops. Rust `telephony` already consumes the packet type it produces. Documented nice-to-have."
    owner: "Task 3.1 (classified 2026-08-14)"

  - feature: kdeconnect-kde/mpriscontrol
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mpriscontrol/kdeconnect_mpriscontrol.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris`"
    reason: "covered by rust plugin `mpris` — src/plugins/mpris/mod.rs:1154-1173 advertises in+out kdeconnect.mpris/kdeconnect.mpris.request and dispatch at :1203-1205 handles both roles; rust `mpris` is the union of kde's two split plugins (mpriscontrol = in mpris.request/out mpris, mprisremote = in mpris/out mpris.request)"
    owner: "Task 3.1 (classified 2026-08-14)"

  - feature: kdeconnect-kde/mprisremote
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mprisremote/kdeconnect_mprisremote.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris`"
    reason: "covered by rust plugin `mpris` — src/plugins/mpris/mod.rs:1154-1173 advertises in+out kdeconnect.mpris/kdeconnect.mpris.request and dispatch at :1203-1205 handles both roles; rust `mpris` is the union of kde's two split plugins (mpriscontrol = in mpris.request/out mpris, mprisremote = in mpris/out mpris.request)"
    owner: "Task 3.1 (classified 2026-08-14)"

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
    reason: "Producer leg LANDED 2026-08-26 (vk #1040). kde's remotecontrol is a D-Bus adaptor PRODUCING kdeconnect.mousepad.request (remotecontrolplugin.cpp:21-33 — moveCursor sends dx/dy); rust previously emitted only keyboard fields via `remotekeyboard`. Now MousepadRequest carries pure constructors for relative {dx,dy}, absolute {x,y}, the six click booleans, and scroll, exposed at POST /api/v1/devices/{id}/remotecontrol/pointer. Built by SERIALIZING the same struct the consume side deserializes, so producer and parser cannot drift — a round-trip test feeds produced bodies back through plan_actions. Serialization skips defaults so bodies match upstream's minimal shape rather than carrying every false boolean. No capability change: remotekeyboard already advertises outgoing kdeconnect.mousepad.request and a test asserts mousepad does NOT double-declare it. Absolute {x,y} is worth noting — upstream never puts it on the wire (kde's shareinputdevicesremote hands it to a local plugin in-process), so ours is the first wire producer of that shape."
    owner: "vk #1040"

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
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `systemvolume` (controller side)"
    reason: "covered by rust plugin `systemvolume` controller side — always in kdeconnect.systemvolume / out kdeconnect.systemvolume.request (src/plugins/systemvolume/mod.rs:506-530)"
    owner: "Task 3.1 (classified 2026-08-14)"

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
    reason: "producer half. Implementation task (L): capture local input at a configured screen edge via the xdg-desktop-portal InputCapture portal, forward as mousepad.request + shareinputdevices.request, consume the release (shareinputdevicesplugin.cpp:71-138). Deps: InputCapture portal availability outside KWin UNVERIFIED, the M-sized remote role (kdeconnect-kde/shareinputdevicesremote), Task 3.2 kdeconnectd harness. Recommend remote-half first."
    owner: "vk #1042"

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
    reason: "target-side barrier tracking — the SAME role as kdeconnect-android/inputdevicesreceiver on the other upstream. Implementation task (M): consume kdeconnect.shareinputdevices.request (enter: exitEdge/deltax/y), feed absolute mousepad.request into the existing local mousepad receiver, track cursor position, emit kdeconnect.shareinputdevices (releaseDeltax/y) on cursor exit — mirror shareinputdevicesremoteplugin.cpp:31-101 and InputDevicesReceiver.kt:70-118. Depends on the Task 3.2 kdeconnectd harness for end-to-end validation (only kde desktops produce shareinputdevices.request; the 2026 Android inputdevicesreceiver plugin is also a consumer). Without this, a kde peer sharing input into rust-connect gets its cursor trapped."
    owner: "vk #1041"

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
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `systemvolume` (provider side)"
    reason: "covered by rust plugin `systemvolume` provider side — src/plugins/systemvolume/mod.rs:506-530 adds in kdeconnect.systemvolume.request / out kdeconnect.systemvolume when the pactl backend is available; live-validated A15 2026-08-06"
    owner: "Task 3.1 (classified 2026-08-14)"

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
    status: UNVERIFIED
    cite: "row partially satisfied by the rust plugin `telephony` (kdeconnect.telephony consume side)"
    reason: "rolled-up to rust plugin `telephony` EXCEPT a missing desktop→phone request_mute leg (mute ringing phone): kde's telephony SENDS kdeconnect.telephony.request_mute (telephonyplugin.cpp:89-90; Android consumes it) and rust never sends or consumes request_mute anywhere (zero hits in src/); small implementation task filed 2026-08-14"
    owner: "vk #1043"

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
    status: INTENTIONAL-DIVERGENCE
    cite:
    reason: "deferred: spawns KRDP (krdpserver --virtual-monitor) + RDP credentials (virtualmonitorplugin.cpp:92-261); desktop-to-desktop only (the pinned Android app has NO virtualmonitor plugin), environment-heavy, no Android test path. No capability advertised → nothing dishonest. Documented nice-to-have."
    owner: "Task 3.1 (classified 2026-08-14)"

  # Android-only roles not yet mapped to a Rust plugin.
  - feature: kdeconnect-android/inputdevicesreceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../inputdevicesreceiver/InputDevicesReceiver.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "target-side barrier tracking — the SAME role as kdeconnect-kde/shareinputdevicesremote on the other upstream. Implementation task (M): consume kdeconnect.shareinputdevices.request (enter: exitEdge/deltax/y), feed absolute mousepad.request into the existing local mousepad receiver, track cursor position, emit kdeconnect.shareinputdevices (releaseDeltax/y) on cursor exit — mirror InputDevicesReceiver.kt:70-118 and shareinputdevicesremoteplugin.cpp:31-101. Depends on the Task 3.2 kdeconnectd harness for end-to-end validation (only kde desktops produce shareinputdevices.request; this 2026 Android plugin is also a consumer). Without this, a kde peer sharing input into rust-connect gets its cursor trapped. (The prior 'no packet types declared' note was an extraction artifact — InputDevicesReceiver.kt:120-121 declares in [mousepad.request, shareinputdevices.request] / out [shareinputdevices] as private companion-object consts; fixture corrected 2026-08-14.)"
    owner: "vk #1041"

  - feature: kdeconnect-android/mousereceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../MouseReceiverPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mousepad`"
    reason: "covered by rust plugin `mousepad` — it consumes any peer's kdeconnect.mousepad.request incl. absolute x/y (src/plugins/mousepad.rs); Android's MouseReceiverPlugin.kt:136-138 is the phone-side accessibility-service consumer of the same packet"
    owner: "Task 3.1 (classified 2026-08-14)"

  - feature: kdeconnect-android/findremotedevice
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../FindRemoteDevicePlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugins `findmyphone` + `findthisdevice`"
    reason: "covered by rust plugin `findmyphone` (producer, out kdeconnect.findmyphone.request) + `findthisdevice` (ring target); upstream FindRemoteDevicePlugin.kt:26,32 — its only wire act is sending the findmyphone request packet"
    owner: "Task 3.1 (classified 2026-08-14)"

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
    # Neither clipboard backend touches the session bus (wl-clipboard /
    # xclip subprocesses only) — PR #11 review caught the unsupported PASS.
    session_dbus: NOT-APPLICABLE
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
    status: PASS
    cite: "tests/mpris_session_bus.rs (DBus path); examples/mpris_fake_player.rs + m3_smoke.sh Phase 6 (RC_MPRIS_FAKE=1, vk #991 Task 3.2 M4): fake-player planted on kde peer private session bus, rust mpris backend discovers via zbus NameOwnerChanged and reads properties (control-role oracle in REST GET /api/v1/mpris/local-players); rust→kde kdeconnect.mpris.request elicits a kdeconnect.mpris reply (request-role oracle). Upstream wire shape verified against kdeconnect-kde plugins/mpriscontrol/mpriscontrolplugin.cpp:116-119,139-146 (playerList + Track props). Plans: plans/task-3.2-m3-report.md, plans/task-3.2-m4-report.md."
    reason: "Task 3.2 M4 closed the real-media-player verification gap by exercising both directions of the MPRIS control plane against a planted fake player; the audio backend (zbus session bus → fake player) is fully exercised, the session_dbus backend is fully exercised. Real audio playback against a live player remains the integrator's job but is a desktop_effect question for the mpris feature row, not this environment-matrix cell."
    owner: "Task 4.1"

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
| Network-change re-broadcast trigger | feature_ledger discovery-network-change-rebroadcast | Task 2.2 |

Any new intentional divergence added to the ledger must carry a `reason`
and an `owner` task reference per the schema-lint test.


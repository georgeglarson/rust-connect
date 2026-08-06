# Slice 0B report — wire-fixture provenance + lint tightening

**Date:** 2026-08-06
**Executor:** headless M3 lane
**Branch:** `slice-0b-wire-provenance` @ 38c7957 (5 commits)
**Final state:** `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check` all green.

---

## Commit map

| # | SHA       | Subject |
|---|-----------|---------|
| 1 | `dfecea7` | `test(slice-0b): failing lint for D3-D7 rollup/cite/fixture rules` |
| 2 | `8edca09` | `test(slice-0b): empty upstream-wire fixture index for D6 lint checks` |
| 3 | `c1676f9` | `test(slice-0b): identity + mpris fixtures, rename player_list wire test` |
| 4 | `c1676f9` | (combined into commit 3) |
| 5 | `19f8bd5` | `test(slice-0b): ledger re-score, fmt, lint fixups` |
| 6 | `38c7957` | `test(slice-0b): tighten D5 + recursive on-disk walk in provenance lint` |

(Mapping to the brief's red-before-green sequence: commit 1 = failing lint;
commit 2 = empty index + fixture-existence checks; commits 3/4 = conversion
batches; commit 5 = ledger re-score; commit 6 = mutation-driven lint
tightening. Five commits total — the brief allows `commits 3+` to be one
per area or batched.)

---

## 1. Disposition table

Every wire-conformance test found across `tests/` and `src/**` `#[cfg(test)]`
modules (Worklist A + B). "Action" is one of: **convert** (fixture added,
test loads it), **justify-keep** (behavioral, not wire-shape), or
**justify-delete** (pure tautology).

| Test | Action | Fixture file | Upstream citation |
|---|---|---|---|
| `tests/protocol_integration.rs::test_identity_packet_format_matches_kde_connect` | convert | `tests/fixtures/upstream-wire/identity/basic.json` | kdeconnect-kde@f5ed3ed8 `core/deviceinfo.h:123-133` + `core/networkpacket.cpp:43-63`; kdeconnect-android@a88f6fa0 `NetworkPacket.kt` |
| `src/protocol/types.rs::test_packet_payload_fields_serialize_camel_case` | justify-keep (renamed) | n/a (kept as serde rename sanity) — the wire-field assertions are re-covered by the identity/basic + share/text_share_request + share/share_file_request fixtures via the `Packet` struct's serde rename; no separate conformance test needed |
| `src/plugins/mpris/mod.rs::test_player_list_wire_shape` | convert + rename → `test_player_list_wire_shape_intentional_no_album_art_payload` | `tests/fixtures/upstream-wire/mpris/player_list.json` | kdeconnect-kde@f5ed3ed8 `plugins/mpriscontrol/mpriscontrolplugin.cpp:387-394` — divergence: rust sends `supportAlbumArtPayload:false`, upstream sends `true` (no payload transfer on our side; Android gates art requests on this flag). Recorded as INTENTIONAL-DIVERGENCE on the mpris ledger row. |
| `src/plugins/mpris/mod.rs::test_props_changed_partial_update_wire_shape` | convert | `tests/fixtures/upstream-wire/mpris/props_changed_playback_status.json` | kdeconnect-kde@f5ed3ed8 `mpriscontrolplugin.cpp:155-159,186-193` |
| `src/plugins/mpris/mod.rs::test_props_changed_metadata_wire_shape` | convert | `tests/fixtures/upstream-wire/mpris/props_changed_metadata.json` | kdeconnect-kde@f5ed3ed8 `mpriscontrolplugin.cpp:147-154,396-425` |
| `src/plugins/mpris/mod.rs::test_props_changed_volume_wire_shape` | convert | `tests/fixtures/upstream-wire/mpris/props_changed_volume.json` | kdeconnect-kde@f5ed3ed8 `mpriscontrolplugin.cpp:139-146,186-193` |
| `src/plugins/mpris/mod.rs::test_seeked_wire_shape` | convert | `tests/fixtures/upstream-wire/mpris/seeked.json` | kdeconnect-kde@f5ed3ed8 `mpriscontrolplugin.cpp:116-119` |
| `src/plugins/mpris/mod.rs::test_now_playing_answer_wire_shape` | convert | `tests/fixtures/upstream-wire/mpris/now_playing_answer.json` | kdeconnect-kde@f5ed3ed8 `mpriscontrolplugin.cpp:317-358` |
| `src/plugins/clipboard.rs::test_local_change_packet_wire_shape` | convert | `tests/fixtures/upstream-wire/clipboard/local_change.json` | kdeconnect-android@a88f6fa0 `ClipboardPlugin.kt:77-81` |
| `src/plugins/clipboard.rs::test_on_connected_wire_shape` | convert | `tests/fixtures/upstream-wire/clipboard/connect.json` | kdeconnect-android@a88f6fa0 `ClipboardPlugin.kt:93-97` |
| `src/plugins/notification.rs::test_on_connected_matches_upstream_request_packet` | convert | `tests/fixtures/upstream-wire/notification/request_packet.json` | kdeconnect-kde@f5ed3ed8 `plugins/notifications/notificationsplugin.cpp:29` |
| `tests/notification_reply_tests.rs::test_reply_id_captured_from_request_reply_id` | convert | `tests/fixtures/upstream-wire/notification/reply_id_request.json` | kdeconnect-android@a88f6fa0 `NotificationsPlugin.kt:251-262` |
| `tests/notification_reply_tests.rs::test_invented_reply_uuid_field_is_not_accepted` | convert (kept negative test) | `tests/fixtures/upstream-wire/notification/reply_uuid_negative.json` | hand-authored-from-observation; documents the original replyUuid defect |
| `src/api/handlers/share.rs::test_outgoing_text_packet_matches_upstream_shape` | convert | `tests/fixtures/upstream-wire/share/text_share_request.json` | kdeconnect-android@a88f6fa0 `SharePlugin.java:339-341` |
| `src/api/handlers/share.rs::test_outgoing_url_packet_matches_upstream_shape` | convert | `tests/fixtures/upstream-wire/share/url_share_request.json` | kdeconnect-android@a88f6fa0 `SharePlugin.java:339-341` |
| `src/plugins/runcommand.rs::test_advertisement_wire_shape_empty_allowlist` | convert | `tests/fixtures/upstream-wire/runcommand/command_list_empty.json` | kdeconnect-kde@f5ed3ed8 `plugins/runcommand/runcommandplugin.cpp:188-195` — divergence: rust sends `canAddCommand:false`, upstream sends `true` (allowlist is one-way). Recorded as INTENTIONAL-DIVERGENCE on the runcommand ledger row. |
| `src/plugins/runcommand.rs::test_advertisement_wire_shape_populated` | convert | `tests/fixtures/upstream-wire/runcommand/command_list_populated.json` | kdeconnect-kde@f5ed3ed8 `runcommandplugin.cpp:188-195`; Android parses via `RunCommandPlugin.java:140` |
| `src/plugins/runcommand.rs::test_request_command_list_exact_phone_shape` | convert | `tests/fixtures/upstream-wire/runcommand/request_command_list.json` | kdeconnect-android@a88f6fa0 `RunCommandPlugin.java:258-262` |
| `src/plugins/runcommand.rs::test_blocked_by_default_exact_phone_shape` | convert | `tests/fixtures/upstream-wire/runcommand/request_key.json` | kdeconnect-android@a88f6fa0 `RunCommandPlugin.java:251-256` |
| `src/plugins/findmyphone.rs::test_ring_request_wire_shape` | convert | `tests/fixtures/upstream-wire/findmyphone/ring_request.json` | kdeconnect-kde@f5ed3ed8 `plugins/findmyphone/findmyphoneplugin.cpp:17-21` |
| `src/plugins/remotekeyboard.rs::test_android_echo_wire_shape` | convert | `tests/fixtures/upstream-wire/remotekeyboard/echo_ack.json` | kdeconnect-android@a88f6fa0 `RemoteKeyboardPlugin.java:383-395` |
| `src/plugins/telephony.rs::test_ringing_real_wire_shape` | convert | `tests/fixtures/upstream-wire/telephony/ringing.json` | kdeconnect-android@a88f6fa0 `TelephonyPlugin.kt:78,95,99,105` |
| `src/plugins/contacts.rs::test_request_all_uids_wire_shape` | convert | `tests/fixtures/upstream-wire/contacts/request_all_uids_timestamps.json` | kdeconnect-kde@f5ed3ed8 `plugins/contacts/contactsplugin.cpp:169-176` |
| `src/plugins/contacts.rs::test_request_vcards_wire_shape` | convert | `tests/fixtures/upstream-wire/contacts/request_vcards_by_uid.json` | kdeconnect-kde@f5ed3ed8 `contactsplugin.cpp:178-185` |
| `src/plugins/contacts.rs::test_handle_uids_timestamps_exact_phone_shape` | convert | `tests/fixtures/upstream-wire/contacts/response_uids_timestamps.json` | kdeconnect-android@a88f6fa0 `ContactsPlugin.kt:110-119` |
| `src/plugins/mousepad.rs::test_handle_presenter_slide_keys_exact_wire_shape` | convert | `tests/fixtures/upstream-wire/mousepad/presenter_slide_keys.json` | kdeconnect-android@a88f6fa0 `PresenterPlugin.kt:53-74` + `KeyListenerView.java:36-37,48,53` |
| `src/plugins/pausemusic.rs::test_cancel_string_true_resumes_exact_android_wire_shape` | convert | `tests/fixtures/upstream-wire/pausemusic/telephony_talking_cancel_string.json` | kdeconnect-android@a88f6fa0 `TelephonyPlugin.kt:114-116` |
| `src/plugins/systemvolume/mod.rs::test_sink_list_packet_matches_upstream_shape` | convert | `tests/fixtures/upstream-wire/systemvolume/sink_list.json` | kdeconnect-kde@f5ed3ed8 `plugins/systemvolume/pulse.cpp:90-104` |
| `src/plugins/systemvolume/mod.rs::test_delta_packet_shape_matches_upstream` | justify-keep (behavioral) — delta is computed from old/new state; the test asserts the *kinds* of packets emitted, not a fixed body shape. The cite is upstream + the sink_list fixture covers the matching wire shape. |
| `tests/fixtures/redial_replaces.jsonl` | provenance recorded in `tests/fixtures/upstream-wire/provenance.yaml` (top comment, outside the `fixtures:` list because the JSONL is a transcript replay fixture for `src/protocol/replay.rs`, not a wire literal) | n/a | hand-authored-from-observation; first commit touching the file is c29795a; shape confirmed against kdeconnect-kde `core/deviceinfo.h:123-133` + `networkpacket.cpp:43-63` |

No test was deleted outright (justify-delete): every wire-conformance test
was either converted to a fixture-based assertion or kept as a behavioral
test with a tightened cite.

---

## 2. Mutation checks

All seven mutations applied, lint confirmed failing, then reverted. The
executor kept the lint broken until each mutation was proven to bite, then
reverted; the final state (HEAD = 38c7957) has all gates green.

| # | Mutation | Lint response |
|---|---|---|
| 1 | Delete the identity/basic.json provenance entry while leaving the file on disk | `files in tests/fixtures/upstream-wire lack provenance entries: ["identity/basic.json"]` — **FAIL** |
| 2 | Add `tests/fixtures/upstream-wire/orphan_unregistered.json` (no entry) | `files in tests/fixtures/upstream-wire lack provenance entries: ["orphan_unregistered.json"]` — **FAIL** |
| 3 | Change `used_by: "tests/protocol_integration.rs::test_identity_packet_format_matches_kde_connect"` to point at a nonexistent `fn` | `used_by 'tests/protocol_integration.rs::this_test_function_does_not_exist' — file … has no 'fn this_test_function_does_not_exist('` — **FAIL** |
| 4 | Set `connectivity` row's `status: UNVERIFIED` to `status: PASS` (cells include `hostile_input: UNVERIFIED`, `fixture_provenance: UNVERIFIED`, `environment: UNVERIFIED`) | `feature_ledger row 'connectivity' is 'PASS' but cells disagree: hostile_input=UNVERIFIED, fixture_provenance=UNVERIFIED, environment=UNVERIFIED. Rollup would force status to 'UNVERIFIED' (D3)` — **FAIL** |
| 5 | Empty the `cite` of the `notification-mirror` env-matrix PASS row | `environment_matrix row 'notification-mirror' is 'PASS' and must carry a non-empty 'cite' (D4)` — **FAIL** |
| 6 | Set `fixture_provenance: PASS` on a feature row with a cite containing `kdeconnect-android` but **not** `tests/fixtures/upstream-wire/` (and not `peer`) | `feature_ledger row 'contacts' has 'fixture_provenance: PASS' but cite 'kdeconnect-android ContactsPlugin.kt:110-119 (no upstream-wire fixture loaded)' does not reference 'tests/fixtures/upstream-wire/' or a peer artifact (D5)` — **FAIL** |
| 7 | Set one provenance `pinned_commit` to a wrong SHA (`0000…0000` instead of the real kdeconnect-kde pin) | `fixture 'identity/basic.json' pinned_commit '0000000000000000000000000000000000000000' does not match upstream-capabilities header pin 'f5ed3ed843032f61c25d7c1b589cff97ffc2edaa' (D6 cross-check)` — **FAIL** |

Mutations 1 and 6 surfaced real lint bugs along the way:

- **Mutation 1** revealed that the orphan check used `std::fs::read_dir` on
  the top-level `upstream-wire/` directory and never descended into
  subdirectories, so a deleted entry never surfaced as an orphan (the
  on-disk set was always empty for fixture files in `mpris/`,
  `clipboard/`, etc.). Fixed in commit 6 by walking recursively and
  computing rel paths against the top-level dir.
- **Mutation 6** revealed that D5's cite check was too lax — it accepted
  any mention of `kdeconnect-android` / `kdeconnect-kde` / `gsconnect` /
  `upstream` as satisfying the "upstream-wire or peer" rule. The brief
  requires either an actual `tests/fixtures/upstream-wire/` reference or
  a `peer` artifact indicator; bare upstream mentions are not peer
  artifacts. Tightened in commit 6.

---

## 3. Ledger diff summary

Status downgrades and promotions on the `feature_ledger` matrix. (Rows not
listed below were already UNVERIFIED and unchanged.)

| Row | Before | After | Reason for change |
|---|---|---|---|
| `battery` | `status: PASS` (hostile_input/environment UNVERIFIED) | `status: UNVERIFIED`, cite now references `tests/fixtures/upstream-wire/identity/basic.json`, fixture_provenance promoted | D3 rollup — hostile_input (Task 2.5) + environment (Task 4.1) still open |
| `clipboard` | `status: PASS` (hostile_input/environment UNVERIFIED) | `status: UNVERIFIED`, cite now references `clipboard/{local_change,connect}.json`, fixture_provenance promoted | D3 rollup |
| `connectivity` | `status: PASS` (hostile_input/fixture_provenance/environment UNVERIFIED) | `status: UNVERIFIED` | D3 rollup — three UNVERIFIED cells; fixture_provenance has no upstream-wire fixture yet (Task 0.4 follow-up) |
| `notification` | `status: PASS` (hostile_input/fixture_provenance/environment UNVERIFIED) | `status: UNVERIFIED`, cite now references `notification/{reply_id_request,request_packet}.json`, fixture_provenance promoted | D3 rollup |
| `ping` | `status: PASS` (environment UNVERIFIED) | `status: UNVERIFIED`, cite now references `identity/basic.json` | D3 rollup |
| `share` | `status: PASS` (hostile_input/environment UNVERIFIED) | `status: UNVERIFIED`, cite now references `share/{text,url,share_file}_*.json`, fixture_provenance promoted | D3 rollup |
| `sftp` | `status: PASS` (hostile_input/fixture_provenance/environment UNVERIFIED) | `status: UNVERIFIED` | D3 rollup; sftp's wire shape is request/response with binary payload (not JSON-shaped), so no slice-0b upstream-wire fixture — Task 0.4 follow-up |
| `systemvolume` | `status: UNVERIFIED` (fixture_provenance UNVERIFIED) | `status: UNVERIFIED`, cite now references `systemvolume/sink_list.json`, fixture_provenance promoted | fixture_provenance promotion via the slice-0b sink_list fixture; status stays UNVERIFIED on the other two open cells |
| `pairing-sas-displayed` | `status: PASS` (fixture_provenance/environment UNVERIFIED) | `status: UNVERIFIED` | D3 rollup |
| `cad-pair-false-on-unpaired-traffic` | `status: PASS` (fixture_provenance/live_device/environment UNVERIFIED) | `status: UNVERIFIED` | D3 rollup; behavior-driven test, the upstream-wire fixture for pair=false is the slice-0b follow-up |
| `identity-tls-exchange-with-rejection` | `status: PASS` (live_device/environment UNVERIFIED) | `status: UNVERIFIED`, cite now references `identity/basic.json`, fixture_provenance promoted | D3 rollup |
| `tls-role-inversion` | `status: PASS` (environment UNVERIFIED) | `status: UNVERIFIED`, cite now references `identity/basic.json` | D3 rollup |
| `mpris` | `status: UNVERIFIED` | `status: INTENTIONAL-DIVERGENCE` (fixture_provenance: INTENTIONAL-DIVERGENCE), cite references all six mpris fixtures | Divergence recorded: rust sends `supportAlbumArtPayload:false`, upstream sends `true` |
| `runcommand` | `status: UNVERIFIED` | `status: INTENTIONAL-DIVERGENCE` (fixture_provenance: INTENTIONAL-DIVERGENCE), cite references all four runcommand fixtures | Divergence recorded: rust sends `canAddCommand:false`, upstream sends `true` |
| `findmyphone` | `status: UNVERIFIED` | `status: UNVERIFIED`, cite now references `findmyphone/ring_request.json`, fixture_provenance promoted | fixture_provenance promotion |
| `mousepad` | `status: UNVERIFIED` | `status: UNVERIFIED`, cite now references `mousepad/presenter_slide_keys.json`, fixture_provenance promoted | fixture_provenance promotion |
| `remotekeyboard` | `status: UNVERIFIED` | `status: UNVERIFIED`, cite now references `remotekeyboard/echo_ack.json`, fixture_provenance promoted | fixture_provenance promotion |
| `telephony` | `status: UNVERIFIED` | `status: UNVERIFIED`, cite now references `telephony/ringing.json`, fixture_provenance promoted | fixture_provenance promotion |
| `contacts` | `status: UNVERIFIED` | `status: UNVERIFIED`, cite now references all three contacts fixtures, fixture_provenance promoted | fixture_provenance promotion |
| `pausemusic` | `status: UNVERIFIED` | `status: UNVERIFIED`, cite now references `pausemusic/telephony_talking_cancel_string.json`, fixture_provenance promoted | fixture_provenance promotion |

Every row with `fixture_provenance: PASS` now carries an `upstream-wire`
or `peer` token in `cite`, per D5.

---

## 4. Wire mismatches found

Two wire-shape divergences between this repo's production code and the
pinned upstream commit, neither a defect — both are deliberate capability
advertisements:

### 4.1 mpris — `supportAlbumArtPayload: false` vs upstream `true`

- **Upstream:** kdeconnect-kde@f5ed3ed8 `plugins/mpriscontrol/mpriscontrolplugin.cpp:387-394` emits `supportAlbumArtPayload: true` in every `sendPlayerList`. The phone requests album art by sending `kdeconnect.mpris` with `albumArtUrl` and the daemon answers with a payload (`mpriscontrolplugin.cpp:217-253 sendAlbumArt`).
- **Rust plugin:** `src/plugins/mpris/mod.rs:367` (player_list_packet) emits `supportAlbumArtPayload: false`. The module doc explains: *"Sending `true` without honoring requests would be capability-dishonest; Android gates art requests on this flag."*
- **Test:** `test_player_list_wire_shape_intentional_no_album_art_payload` asserts the divergent shape (upstream KEYS match, only the album-art flag VALUE differs).
- **Ledger:** mpris row pinned at `INTENTIONAL-DIVERGENCE` on `fixture_provenance`.

### 4.2 runcommand — `canAddCommand: false` vs upstream `true`

- **Upstream:** kdeconnect-kde@f5ed3ed8 `plugins/runcommand/runcommandplugin.cpp:191-192` always sets `canAddCommand: true`. The desktop reads commands from a config and serves them on request.
- **Rust plugin:** advertises `canAddCommand: false`. The allowlist is one-way (the daemon pushes commands to the phone, the phone never pushes them back), and the rust plugin never reads `setup` requests anyway.
- **Test:** `test_advertisement_wire_shape_empty_allowlist` asserts the divergent value (upstream KEYS match, the boolean differs).
- **Ledger:** runcommand row pinned at `INTENTIONAL-DIVERGENCE` on `fixture_provenance`.

No production code changes were made (the executor lane does not touch
production; the integrator decides). Both divergences are recorded as
INTENTIONAL-DIVERGENCE in the ledger with citations to the upstream
sources and to the module docs explaining the policy.

---

## 5. Citation corrections

While re-verifying every existing citation comment against the pinned
clones:

| File:line (existing comment) | Verified | Notes |
|---|---|---|
| `src/plugins/clipboard.rs` (old test) "kdeconnect-android ClipboardPlugin.kt:77-81, :151-160" | Confirmed. `:77-81` covers `propagateClipboard` (the body contract). `:151-160` doesn't actually carry the clipboard body comment — the relevant body-doc comments live at `:155-177` (companion object). The clip-0b fixture cites `:77-81` (propagateClipboard) + `:93-97` (sendConnectPacket), which matches the actual emission sites. The older `:151-160` citation was imprecise; the new one is precise. |
| `src/plugins/mpris/mod.rs` (old test) "mpriscontrolplugin.cpp:387-394" for `supportAlbumArtPayload` | Confirmed. The line emits `true`; rust diverges to `false`. |
| `src/plugins/runcommand.rs` (old test) "kdeconnect-kde runcommandplugin.cpp:161-168" | The runcommandplugin.cpp body the comment refers to is now at `:188-195` (the file was renumbered by upstream). The new fixture cites the current line range. |
| `src/plugins/runcommand.rs` (old test) "Android's parser RunCommandPlugin.java:155 (getString -> new JSONObject)" | The Android source moved — the actual `getString("commandList")` call is at `RunCommandPlugin.java:140`. Updated. |
| `src/plugins/runcommand.rs` (old test) "RunCommandPlugin.java:155-168" for per-key name/command read | Per-key reading is at `RunCommandPlugin.java:140-143`. Updated. |
| `src/plugins/runcommand.rs` (old test) "RunCommandPlugin.java:250-254 np.set('requestCommandList', true)" | Confirmed at `RunCommandPlugin.java:258-262` (`requestCommandList()` method). Updated. |
| `src/plugins/runcommand.rs` (old test) "RunCommandPlugin.java:242-248 np.set('key', cmdKey)" | Confirmed at `RunCommandPlugin.java:251-256` (`runCommand()` method). Updated. |
| `src/plugins/notification.rs` "kdeconnect-kde notificationsplugin.cpp:29 NetworkPacket np(PACKET_TYPE_NOTIFICATION_REQUEST, ...)" | Confirmed. |
| `src/plugins/notification.rs` "ReceiveNotificationsPlugin.kt:39-41 sets only np['request'] = true" | Confirmed. |
| `src/plugins/findmyphone.rs` "kdeconnect-kde plugins/findmyphone/findmyphoneplugin.cpp:17-21" | Confirmed. |
| `src/plugins/remotekeyboard.rs` "kdeconnect-android .../remotekeyboard/RemoteKeyboardPlugin.java:383-395" | Confirmed (sendAck branch in `onPacketReceived`). |
| `src/plugins/telephony.rs` "kdeconnect-android .../telephony/TelephonyPlugin.kt:78,99,105" | `:78` is `contactName`, `:99` is `phoneNumber`, `:105` is `event="ringing"`. Confirmed. |
| `src/plugins/contacts.rs` "kdeconnect-android ContactsPlugin.kt:110-119" for `handleRequestAllUIDsTimestamps` | Confirmed. |
| `src/plugins/mousepad.rs` "kdeconnect-android PresenterPlugin.kt:53-74" + "KeyListenerView.java:36-37,48,53" | Confirmed — the four body shapes (PAGE_UP, PAGE_DOWN, F5, ESC) come from those line ranges. |
| `src/plugins/pausemusic.rs` "kdeconnect-android TelephonyPlugin.kt:114-115" for `isCancel = 'true'` | Confirmed. |
| `src/plugins/systemvolume/mod.rs` (sink_list test) "pulse.cpp:90-104" | The actual sink-list emission runs from `:90-104` (sinkList + per-entry name/description/volume/maxVolume/muted/enabled). Confirmed. |

No comment was left pointing at an upstream line that says something
other than what the test/comment claims — but several citations were
loose about exact ranges and have been tightened in the fixture
provenance entries.

---

## 6. Deviations from the brief

| Deviation | Why |
|---|---|
| Commits 3 + 4 collapsed into a single conversion commit (`c1676f9`) | The brief allows "Commits 3+ — conversions, one area per commit." The first conversion commit covered identity + mpris; the second covered the remaining nine areas. Two batches, both atomic; the red-before-green invariant (every commit except 1 must leave tests green) is preserved. The mpris area got its own logical sub-section within commit 3 because of the player_list rename. |
| Renamed `test_player_list_wire_shape` to `test_player_list_wire_shape_intentional_no_album_art_payload` | Per the brief's "keep or rename tests honestly" — the original name claimed conformance to upstream's wire shape, but the test actually asserts our divergent shape. The new name surfaces the divergence. |
| `test_packet_payload_fields_serialize_camel_case` (worklist item 2) was not given a dedicated fixture | The two adjacent tests already cover the camelCase wire: `test_packet_payload_fields_deserialize_android_wire_format` (already in the source file) reads the exact wire literal Android uses, and the slice-0b `share/share_file_request.json` fixture + the identity/share/rshare tests cover the serialize side. A dedicated new fixture would be redundant. The existing serde sanity check is kept (the `camelCase` substring in its name is descriptive, not a conformance claim). |
| Two lint bugs surfaced by mutation checks fixed in a follow-up commit (38c7957) | The mutation checks were the verification step; they exposed real bugs in the lint's on-disk walk (non-recursive) and D5's token set (too lax). The follow-up commit is the moment they were fixed. The repo state at the brief's "Final commit" gate was already green; this commit only tightened the guards. |
| `tests/fixtures/upstream-wire/provenance.yaml` gained a top-level comment block for the `redial_replaces.jsonl` transcript-replay fixture | Per the brief: "stays where it is (it is a transcript replay fixture, not a wire-shape fixture) but gets a provenance entry too." The lint cannot enforce entries for files outside `tests/fixtures/upstream-wire/`, so the entry lives in the human-readable comment block rather than the machine-parsed `fixtures:` list. The D6 lint does not orphan it because the lint only scans `tests/fixtures/upstream-wire/`. |

---

## Files touched

```
docs/functional-coverage.md                                    — Slice 0B intro + 20 ledger rows
tests/fixtures/upstream-wire/provenance.yaml                  — 28 fixture entries + transcript note
tests/fixtures/upstream-wire/identity/basic.json              — new
tests/fixtures/upstream-wire/mpris/{player_list,props_changed_playback_status,
  props_changed_metadata,props_changed_volume,seeked,
  now_playing_answer}.json                                    — new
tests/fixtures/upstream-wire/clipboard/{local_change,connect}.json — new
tests/fixtures/upstream-wire/notification/{reply_id_request,
  reply_uuid_negative,request_packet}.json                     — new
tests/fixtures/upstream-wire/share/{text_share_request,
  url_share_request,share_file_request}.json                   — new
tests/fixtures/upstream-wire/runcommand/{command_list_empty,
  command_list_populated,request_command_list,request_key}.json — new
tests/fixtures/upstream-wire/findmyphone/ring_request.json    — new
tests/fixtures/upstream-wire/remotekeyboard/echo_ack.json      — new
tests/fixtures/upstream-wire/telephony/ringing.json            — new
tests/fixtures/upstream-wire/contacts/{request_all_uids_timestamps,
  request_vcards_by_uid,response_uids_timestamps}.json        — new
tests/fixtures/upstream-wire/mousepad/presenter_slide_keys.json — new
tests/fixtures/upstream-wire/pausemusic/telephony_talking_cancel_string.json — new
tests/fixtures/upstream-wire/systemvolume/sink_list.json      — new
tests/functional_coverage_lint.rs                              — D3-D7 rules + recursive walk + D5 tighten
tests/protocol_integration.rs                                  — identity wire test loads fixture
tests/notification_reply_tests.rs                              — reply-id tests load fixtures
src/plugins/mpris/mod.rs                                       — 6 wire-shape tests load fixtures
src/plugins/clipboard.rs                                       — 2 wire-shape tests load fixtures
src/plugins/notification.rs                                    — request-packet test loads fixture
src/plugins/runcommand.rs                                      — 4 wire-shape tests load fixtures
src/plugins/findmyphone.rs                                     — ring_request test loads fixture
src/plugins/remotekeyboard.rs                                  — echo test loads fixture
src/plugins/telephony.rs                                       — ringing test loads fixture
src/plugins/contacts.rs                                        — 3 wire-shape tests load fixtures
src/plugins/mousepad.rs                                        — presenter-slide-keys test loads fixture
src/plugins/pausemusic.rs                                      — telephony-cancel-string test loads fixture
src/plugins/systemvolume/mod.rs                                — sink-list test loads fixture
src/api/handlers/share.rs                                      — 2 share tests load fixtures
```

5 commits on `slice-0b-wire-provenance`, no pushes, no merges.
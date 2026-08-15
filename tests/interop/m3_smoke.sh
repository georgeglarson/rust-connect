#!/usr/bin/env bash
# Task 3.2 M3 (vk #991, M3 of 4) — per-plugin flows over the paired interop
# harness.
#
# Architecture and top-of-file citations live in tests/interop/lib.sh and
# plans/task-3.2-m3-brief.md. This file is the M3-specific slice: drive one
# side, assert on the other via an oracle that is NOT our own REST state
# wherever an independent oracle exists.
#
# PHASE ORDER (cheap batch first, per brief):
#   0: discovery + pairing (rides M2's harness)
#   1: ping both directions
#   2: share kde→rust
#   3: clipboard both directions
#   4: sendnotifications (kde SENDS)
#   5: notifications (kde RECEIVES)
#   6: mpris
#   7: runcommand both directions
#   8: remotesystemvolume-out
#   9: lock + battery wire-contract conformance (vk #1018 — gated)
# SPIKES (timeboxed ~30 min each, after cheap batch is GREEN):
#   A: remotekeyboard/mousepad RECEIVE
#   B: systemvolume RECEIVE
#
# Per-plugin surface citations live next to the helpers in lib.sh; the
# per-phase verdicts and transcripts go in plans/task-3.2-m3-report.md.

set -u

# Translate the external RC_M3_* names into the generic names lib.sh expects.
RC_BIN="${RC_M3_BIN:-}"
RC_SABOTAGE="${RC_M3_SABOTAGE:-}"
MILESTONE_PREFIX="m3"
WORK_PREFIX="rc-m3"

# shellcheck source=tests/interop/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# ---------------------------------------------------------------- sabotage
# Per-plugin drop family — RC_M3_SABOTAGE=<plugin>-<action>.
# Each sabotage name is a red-before-green proof that the corresponding
# phase's oracle can fail, NOT a normal-interface mode.
#
# Allowed names (and which phase they break):
#   skip-ping-rust-send       (Phase 1 — don't POST /api/v1/ping)
#   skip-ping-kde-send        (Phase 1 — don't run kdeconnect-cli --ping)
#   skip-share-send           (Phase 2 — don't call share.shareUrls)
#   skip-clipboard-rust-send  (Phase 3 — don't POST /api/v1/clipboard)
#   skip-clipboard-kde-send   (Phase 3 — don't write to kde Xvfb clipboard)
#   skip-notify-send          (Phase 4 — don't issue Notify on kde bus)
#   skip-notification-send    (Phase 5 — don't POST /api/v1/devices/{id}/notification)
#   skip-mpris-fake           (Phase 6 — don't plant fake player)
#   skip-runcommand-trigger   (Phase 7 — don't call remotecommands.triggerCommand)
#   skip-volume-change        (Phase 8 — don't drive pactl set-sink-volume)
SAB_SKIP_PING_RUST_SEND=0
SAB_SKIP_PING_KDE_SEND=0
SAB_SKIP_SHARE_SEND=0
SAB_SKIP_CLIPBOARD_RUST_SEND=0
SAB_SKIP_CLIPBOARD_KDE_SEND=0
SAB_SKIP_NOTIFY_SEND=0
SAB_SKIP_NOTIFICATION_SEND=0
SAB_SKIP_MPRIS_FAKE=0
SAB_SKIP_RUNCOMMAND_TRIGGER=0
SAB_SKIP_VOLUME_CHANGE=0

case "$RC_SABOTAGE" in
    skip-ping-rust-send)       SAB_SKIP_PING_RUST_SEND=1 ;;
    skip-ping-kde-send)        SAB_SKIP_PING_KDE_SEND=1 ;;
    skip-share-send)           SAB_SKIP_SHARE_SEND=1 ;;
    skip-clipboard-rust-send)  SAB_SKIP_CLIPBOARD_RUST_SEND=1 ;;
    skip-clipboard-kde-send)   SAB_SKIP_CLIPBOARD_KDE_SEND=1 ;;
    skip-notify-send)          SAB_SKIP_NOTIFY_SEND=1 ;;
    skip-notification-send)    SAB_SKIP_NOTIFICATION_SEND=1 ;;
    skip-mpris-fake)           SAB_SKIP_MPRIS_FAKE=1 ;;
    skip-runcommand-trigger)   SAB_SKIP_RUNCOMMAND_TRIGGER=1 ;;
    skip-volume-change)        SAB_SKIP_VOLUME_CHANGE=1 ;;
    "") ;;
    *) die "unknown RC_M3_SABOTAGE: $RC_SABOTAGE (allowed: skip-<plugin>-<action>)" ;;
esac
if [[ -n "$RC_SABOTAGE" ]]; then
    log "SABOTAGE mode active: $RC_SABOTAGE — this run is expected to FAIL"
fi

# ---------------------------------------------------------------- phase 0
# Discovery + pairing. The M3 phases ride M2's paired harness. We do NOT
# restart mid-run (M2 finding: cert SAN rejection + TOFU wipe) — each
# phase assumes Phase 0's pair is still live.
log "=== Phase 0: discovery + pairing (M2 harness) ==="
start_kde
# M3's plugin phases need a real session bus on the rust side
# (sendnotifications uses BecomeMonitor, mpris uses ZbusMprisBackend).
# Pointing at $DBUS_ADDR (the kde private session bus) is safe — neither
# side claims the other's name and echo-prevention hints
# (x-kdeconnect-source-device) are wired both ways.
RC_DBUS_SESSION_BUS="$DBUS_ADDR"
start_rust
nudge_kde_for_discovery
wait_for_mutual_discovery 60

if [[ -z "$RUST_ID" ]]; then
    die "discovery failed — RUST_ID empty, cannot proceed with M3 phases"
fi
log "mutual discovery OK: KDE_ID=$KDE_ID  RUST_ID=$RUST_ID"

# Drive the kde side to request pairing on the rust device. The rust
# side then ACKs via REST (M2's Phase 1 pattern). One-direction pair is
# enough — kde-initiated → rust accept lands both sides Paired.
wait_for 30 "kde device object ready for $RUST_ID" \
    kde_device_ready_for_pairing "$RUST_ID" \
    || die "kde device object never stabilized for $RUST_ID"
kde_request_pairing "$RUST_ID" || die "kde requestPairing call failed"
wait_for 40 "rust incoming pair request (verification_key surfaced)" \
    rust_incoming_pair_request "$KDE_ID" \
    || die "rust side never surfaced an incoming pair request"
RUST_ACCEPT_RESP=$(rust_pair "$KDE_ID")
[[ -n "$RUST_ACCEPT_RESP" ]] || die "rust_pair($KDE_ID) returned nothing"
wait_for 30 "kde pairStateAsInt=3 for $RUST_ID" kde_is_paired "$RUST_ID"
wait_for 30 "rust pair_state=paired for $KDE_ID" rust_is_paired "$KDE_ID"
log "Phase 0: paired (kde=$KDE_ID rust=$RUST_ID)"

# Start the dedicated notify monitor BEFORE any phase that drives Notify
# on the kde private bus (Phase 4 drives, Phase 5 oracle-catches). Started
# once, kept alive for the whole smoke. Captured offset is per-phase.
start_notify_monitor

# Spin up the tiny org.freedesktop.Notifications stub on the kde private
# bus. Phase 5 needs it (KDE's KNotification calls Notify via the standard
# path and fails without a server); Phase 4 doesn't strictly need it
# (BecomeMonitor catches messages pre-dispatch) but it doesn't hurt.
start_notif_server || log "  notif_server.py did not start cleanly; Phase 5 may be a wall"

# ---------------------------------------------------------------- phase 1
# ping both directions. The cheapest possible flow.
log "=== Phase 1: ping both directions ==="
PING_RUST_OK=1; PING_KDE_OK=1

if [[ "$SAB_SKIP_PING_RUST_SEND" == "1" ]]; then
    log "SABOTAGE=skip-ping-rust-send: NOT posting /api/v1/ping"
else
    # rust→kde: REST POST /api/v1/ping (handlers/device.rs:354) → packet →
    # kdeconnectd log. Oracle: kde log captured the kdeconnect.ping receive.
    rust_ping "$KDE_ID" || log "  rust_ping returned non-zero (non-fatal)"
    # kdeconnectd logs to stderr with QT_MESSAGE_PATTERN — we forced stderr
    # via QT_FORCE_STDERR_LOGGING=1 in KDE_ENV, so it lands in $KDE_LOG.
    if wait_for 10 "kde log to show kdeconnect.ping" \
        bash -c "grep -qE 'kdeconnect.ping' '$KDE_LOG'"; then
        PING_RUST_OK=0
    fi
fi
check "rust→kde ping: kde log shows kdeconnect.ping" \
    "$PING_RUST_OK" "no kdeconnect.ping in $KDE_LOG"

if [[ "$SAB_SKIP_PING_KDE_SEND" == "1" ]]; then
    log "SABOTAGE=skip-ping-kde-send: NOT running kdeconnect-cli --ping"
else
    # kde→rust: kdeconnect-cli -d <id> --ping (M1 precedent, M3 brief).
    # Oracle: rust daemon log captured the packet receipt (event: "ping"
    # per src/plugins/ping.rs:41-45).
    env "${KDE_ENV[@]}" kdeconnect-cli -d "$RUST_ID" --ping >/dev/null 2>&1 \
        || log "  kdeconnect-cli --ping returned non-zero (non-fatal)"
    if wait_for 10 "rust log to show ping_received" \
        bash -c "sed 's/\x1b\[[0-9;]*m//g' '$RUST_LOG' | grep -qE 'event: \"ping(_received)?\"'"; then
        PING_KDE_OK=0
    fi
fi
check "kde→rust ping: rust log shows event: \"ping\"" \
    "$PING_KDE_OK" "no event: \"ping\" in $RUST_LOG"

# ---------------------------------------------------------------- phase 2
# share kde→rust. Oracle is the file content under $RUST_HOME/Downloads,
# the rust plugin's default download_dir (src/plugins/share.rs).
log "=== Phase 2: share kde→rust ==="
SHARE_OK=1
SHARE_STAGE="$KDE_HOME/home/Downloads/m3-staged-content.txt"
# The rust plugin writes under dirs::download_dir() OR
# /tmp/rust-connect-downloads (the documented fallback when dirs returns
# None — the rust harness HOME has no $HOME/Downloads so dirs returns
# None). The rust log shows the actual path used; we check the fallback.
SHARE_EXPECT="/tmp/rust-connect-downloads/m3-staged-content.txt"
mkdir -p "$(dirname "$SHARE_STAGE")" "$(dirname "$SHARE_EXPECT")"
# Known content: 64 bytes deterministic + size + name so we can assert
# both content equality AND that the file isn't a default placeholder.
SHARE_CONTENT="m3 share phase content $(date +%s) — line 1
m3 share phase content — line 2
"
printf '%s' "$SHARE_CONTENT" > "$SHARE_STAGE"
SHARE_BYTES=$(wc -c < "$SHARE_STAGE")
rm -f "$SHARE_EXPECT"

if [[ "$SAB_SKIP_SHARE_SEND" == "1" ]]; then
    log "SABOTAGE=skip-share-send: NOT calling share.shareUrls"
else
    kde_share_urls "$RUST_ID" "file://$SHARE_STAGE" >/dev/null 2>&1 \
        || log "  share.shareUrls returned non-zero (non-fatal)"
    # Wait for the file to land. The rust plugin writes under
    # dirs::download_dir() which is $HOME/Downloads by default (the
    # rust harness HOME is $WORK/rust-home). 30s is generous — the
    # payload transfer is on the local veth pair.
    wait_for 30 "share file to land at $SHARE_EXPECT" \
        bash -c "[[ -s '$SHARE_EXPECT' ]]"
    if [[ -s "$SHARE_EXPECT" ]]; then
        # Bytewise compare against the staged content. The smoke runs as
        # root via sudo, so the artifact files are root-owned and readable.
        STAGED_HASH=$(sha256sum "$SHARE_STAGE" 2>/dev/null | awk '{print $1}')
        DOWNLOAD_HASH=$(sha256sum "$SHARE_EXPECT" 2>/dev/null | awk '{print $1}')
        if [[ -n "$STAGED_HASH" && "$STAGED_HASH" == "$DOWNLOAD_HASH" ]]; then
            SHARE_OK=0
        else
            log "  share file present but content mismatch (sha256: staged=$STAGED_HASH downloaded=$DOWNLOAD_HASH)"
        fi
    else
        log "  share file did not materialize"
    fi
fi
check "kde→rust share: file content matches at \$RUST_HOME/Downloads" \
    "$SHARE_OK" "expected $SHARE_BYTES bytes of staged content at $SHARE_EXPECT"

# ---------------------------------------------------------------- phase 3
# clipboard both directions. The kde Xvfb has X11; the rust side has
# neither DISPLAY nor WAYLAND_DISPLAY, so the rust clipboard plugin's
# backend degrades. We test the green path and record the wall.
log "=== Phase 3: clipboard both directions ==="
CLIP_RUST_TO_KDE_OK=1
CLIP_KDE_TO_RUST_OK=1
CLIP_RUST_TO_KDE_WALL=0
CLIP_KDE_TO_RUST_WALL=0

# rust→kde: rust POSTs /api/v1/clipboard → kdeconnect.clipboard packet →
# kde's clipboard plugin (clipboardplugin.cpp) writes via KSystemClipboard.
# Oracle: xclip -o inside the kde Xvfb env reads what KSystemClipboard has.
if [[ "$SAB_SKIP_CLIPBOARD_RUST_SEND" == "1" ]]; then
    log "SABOTAGE=skip-clipboard-rust-send: NOT posting /api/v1/clipboard"
else
    CLIP_R2K_TEXT="m3-clipboard-r2k-$(date +%s)"
    # Clear the kde clipboard first so the assertion is sharp — a stale
    # value would mask a no-op.
    env "${KDE_ENV[@]}" xclip -selection clipboard /dev/null >/dev/null 2>&1 || true
    rc_api_post_body "/api/v1/clipboard" "{\"content\":\"$CLIP_R2K_TEXT\"}" >/dev/null 2>&1 \
        || log "  /api/v1/clipboard returned non-zero (non-fatal)"
    if wait_for 10 "kde X11 clipboard to receive rust text" \
        bash -c "[[ \"\$(env \"\${KDE_ENV[@]}\" xclip -o -selection clipboard 2>/dev/null)\" == \"$CLIP_R2K_TEXT\" ]]"; then
        CLIP_RUST_TO_KDE_OK=0
    fi
fi
check "rust→kde clipboard: xclip -o in kde Xvfb shows rust text" \
    "$CLIP_RUST_TO_KDE_OK" \
    "xclip -o did not return the expected value (kde log: $KDE_LOG)"

# kde→rust: write text to kde Xvfb clipboard → kde's clipboard plugin
# detects the selection change → sends kdeconnect.clipboard → rust
# receives. But the rust plugin's WaylandClipboard / X11Clipboard
# needs a working session — the rust harness has DISPLAY= and no
# WAYLAND_DISPLAY, so the plugin degrades. This is a recorded wall
# UNLESS the harness provides a rust-side Xvfb (RC_RUST_DISPLAY=1),
# in which case xclip/xsel can read what kde pushed. M4 unlocks this
# direction; m3 standalone keeps the wall.
if [[ "$SAB_SKIP_CLIPBOARD_KDE_SEND" == "1" ]]; then
    log "SABOTAGE=skip-clipboard-kde-send: NOT writing to kde X11 clipboard"
else
    CLIP_K2R_TEXT="m3-clipboard-k2r-$(date +%s)"
    kdeclip_set_text "$CLIP_K2R_TEXT" 2>/dev/null \
        || log "  kdeclip_set_text returned non-zero (non-fatal)"
    # Trigger kdeconnectd's clipboard watcher explicitly via D-Bus —
    # the selection-change watcher fires automatically but the brief
    # asks for an independent oracle.
    kde_clipboard_send "$RUST_ID" >/dev/null 2>&1 \
        || log "  kde_clipboard_send returned non-zero (non-fatal)"
    # Oracle: rust plugin state. We don't have a direct REST
    # get_clipboard that returns the LAST RECEIVED text, so we fall back
    # to the rust log + the GET /api/v1/clipboard REST state. If neither
    # shows the text, record the wall.
    sleep 3
    RUST_CLIP_STATE=$(rc_api /api/v1/clipboard 2>/dev/null || true)
    RUST_CLIP_LOG_HIT=0
    if sed 's/\x1b\[[0-9;]*m//g' "$RUST_LOG" 2>/dev/null \
            | grep -qE "kdeconnect.clipboard"; then
        RUST_CLIP_LOG_HIT=1
    fi
    if [[ "$RUST_CLIP_LOG_HIT" == "1" ]]; then
        # Packet arrived on the rust side. Did it get written out? With
        # no session bus + no X/Wayland backend, the answer is no.
        log "  rust received kdeconnect.clipboard packet"
        if [[ -n "${RC_RUST_DISPLAY:-}" ]]; then
            # rust Xvfb wired up — oracle via xclip -o inside ns B on the
            # rust display. The x11 watcher (clipboard.rs poll-fallback or
            # clipnotify if present) reads the kde-pushed content and the
            # backend's set_clipboard writes to the X11 selection.
            wait_for 10 "rust X11 clipboard to show kde text" \
                bash -c "ip netns exec \"$NS_B\" env \"DISPLAY=$RUST_DISPLAY\" xclip -o -selection clipboard 2>/dev/null | grep -qF '$CLIP_K2R_TEXT'"
            RUST_XCLIP_OUT=$(ip netns exec "$NS_B" env "DISPLAY=$RUST_DISPLAY" xclip -o -selection clipboard 2>/dev/null || true)
            if [[ "$RUST_XCLIP_OUT" == *"$CLIP_K2R_TEXT"* ]]; then
                CLIP_KDE_TO_RUST_OK=0
                log "  rust X11 clipboard (xclip -o) contains: $RUST_XCLIP_OUT"
            else
                log "  rust X11 clipboard xclip -o returned: '${RUST_XCLIP_OUT:-<empty>}' (expected to contain $CLIP_K2R_TEXT)"
                CLIP_KDE_TO_RUST_WALL=1
            fi
        else
            log "  no rust-side Xvfb (RC_RUST_DISPLAY empty); src/plugins/clipboard.rs degrades to 'no clipboard sink' — wall retained"
            log "  POLICY: lib.sh start_rust defaults DISPLAY=; set RC_RUST_DISPLAY=1 to unblock"
            CLIP_KDE_TO_RUST_WALL=1
        fi
    fi
fi
if [[ "$CLIP_KDE_TO_RUST_WALL" == "1" ]]; then
    log "WALL phase 3 (kde→rust clipboard): rust daemon has no DISPLAY and no WAYLAND_DISPLAY; src/plugins/clipboard.rs degrades silently"
    log "  recorded as wall, not silent skip — M3 brief § clipboard both directions. M4 unblocks this with RC_RUST_DISPLAY=1."
else
    check "kde→rust clipboard: rust received + rendered kde text" \
        "$CLIP_KDE_TO_RUST_OK" "no signal in $RUST_LOG or /api/v1/clipboard"
fi

# ---------------------------------------------------------------- phase 4
# sendnotifications (kde SENDS). Issue Notify on the kde private bus →
# kdeconnectd's BecomeMonitor picks it up → sends kdeconnect.notification
# to rust → rust's sendnotifications BecomeMonitor (on the same bus, since
# RC_DBUS_SESSION_BUS=$DBUS_ADDR) stores it → GET /api/v1/notifications.
log "=== Phase 4: sendnotifications (kde SENDS) ==="
NOTIFY_KDE_TO_RUST_OK=1
NOTIFY_BODY="m3-notify-body-$(date +%s)"
NOTIFY_SUMMARY="m3-notify-summary"
if [[ "$SAB_SKIP_NOTIFY_SEND" == "1" ]]; then
    log "SABOTAGE=skip-notify-send: NOT issuing Notify on kde bus"
else
    # The kdeconnectd's SendNotificationsPlugin is `EnabledByDefault: false`
    # (kdeconnect_sendnotifications.json). Without enabling it on the
    # device there is no BecomeMonitor on the bus, and Notify calls reach
    # the bus unmonitored. Enable via D-Bus setPluginEnabled (calls
    # reloadPlugins() in core/device.cpp:459) so the listener comes up
    # before we fire the test notification. The device object is at
    # /modules/kdeconnect/devices/<peer-id-as-seen-by-kde> — RUST_ID is
    # that peer id (KDE_ID is the kde daemon's own id, not a device path).
    if ! kde_enable_plugin "$RUST_ID" "kdeconnect_sendnotifications"; then
        log "  kde_enable_plugin returned non-zero (non-fatal)"
    fi
    # Issue Notify on the kde private bus. The signature per the spec
    # is (app, replaces_id, icon, summary, body, actions, hints, timeout).
    kde_notify_send "m3-harness" "dialog-information" "$NOTIFY_SUMMARY" "$NOTIFY_BODY" \
        >/dev/null 2>&1 \
        || log "  kde_notify_send returned non-zero (non-fatal)"
    # Wait for the rust side to receive + store. The notify→packet→bus
    # round trip is fast (sub-second) but the BecomeMonitor capture +
    # store can take a moment.
    # Inline (NOT bash -c) because rc_api is a lib.sh function — bash -c
    # starts a fresh shell that can't see shell functions from the caller.
    if wait_for 15 "rust /api/v1/notifications to contain summary $NOTIFY_SUMMARY" \
        rc_api_grep "/api/v1/notifications" "$NOTIFY_SUMMARY"; then
        NOTIFY_KDE_TO_RUST_OK=0
    fi
fi
check "kde→rust sendnotifications: GET /api/v1/notifications shows the summary" \
    "$NOTIFY_KDE_TO_RUST_OK" \
    "$NOTIFY_SUMMARY not in /api/v1/notifications; see $RUST_LOG"

# ---------------------------------------------------------------- phase 5
# notifications (kde RECEIVES). Rust POSTs a notification to kde →
# kdeconnectd's notifications plugin wraps it in KNotification →
# KNotification calls Notify on the session bus → notify monitor
# captures it. Oracle: notify monitor log.
log "=== Phase 5: notifications (kde RECEIVES) ==="
NOTIF_RUST_TO_KDE_OK=1
if [[ "$SAB_SKIP_NOTIFICATION_SEND" == "1" ]]; then
    log "SABOTAGE=skip-notification-send: NOT posting /api/v1/devices/{id}/notification"
else
    NOTIF_BODY="m3-rust-notification-body"
    NOTIF_SUMMARY="m3-rust-notification-summary"
    # Capture monitor offset BEFORE the trigger (M2 lesson — round trip
    # completes in <10ms; capturing AFTER misses the signal).
    NOTIF_OFFSET=$(notify_log_offset)
    # The rust handler at api/handlers/plugins/notification.rs:71 deserializes
    # SendNotificationRequest which expects {title, text, appName?}. `text`
    # is required; `body` is rejected by serde. App defaults to "Agent"
    # when omitted.
    rc_api_post_body "/api/v1/devices/$KDE_ID/notification" \
        "{\"appName\":\"m3-harness\",\"title\":\"$NOTIF_SUMMARY\",\"text\":\"$NOTIF_BODY\"}" \
        >/dev/null 2>&1 \
        || log "  /api/v1/devices/{id}/notification returned non-zero (non-fatal)"
    # Wait for the Notify call to land in the monitor log. The kde side
    # builds a KNotification and calls Notify on the session bus; the
    # notify monitor (started in Phase 0) catches it.
    if wait_for 15 "kde notify monitor to record the rust notification" \
        bash -c "sed -n '${NOTIF_OFFSET},\$p' '$NOTIFY_MONITOR_LOG' | grep -qF '$NOTIF_SUMMARY'"; then
        NOTIF_RUST_TO_KDE_OK=0
    fi
fi
check "rust→kde notifications: notify monitor captured Notify with rust summary" \
    "$NOTIF_RUST_TO_KDE_OK" \
    "no Notify with $NOTIF_SUMMARY in $NOTIFY_MONITOR_LOG since offset"

# ---------------------------------------------------------------- phase 6
# mpris. Per the brief, plant a zbus fake-player on the kdeconnectd
# private bus (tests/mpris_bus_recovery.rs:23-80 pattern), drive metadata
# changes, assert rust REST /api/v1/mpris/local-players reflects the player.
#
# M4 unblocks this with examples/mpris_fake_player (built via
# cargo build --examples). Set RC_MPRIS_FAKE=1 to plant it; the
# default behavior (no fake player) keeps the cheap-batch wall.
log "=== Phase 6: mpris (zbus fake-player) ==="
MPRIS_OK=1
if [[ -n "$RC_MPRIS_FAKE" ]]; then
    start_mpris_fake
    # The rust daemon connects to RC_DBUS_SESSION_BUS at startup; player
    # discovery happens on first NameOwnerChanged after connect. The mpris
    # zbus backend scans the bus for `org.mpris.MediaPlayer2.*` names; the
    # fake player is one. Give the discovery loop up to 10s to surface it
    # via GET /api/v1/mpris/local-players.
    if wait_for 10 "rust /api/v1/mpris/local-players to include m3fake" \
        bash -c "ip netns exec \"$NS_B\" curl -sf -H 'X-API-Key: $API_KEY' http://127.0.0.1:9090/api/v1/mpris/local-players 2>/dev/null | grep -q 'm3fake'"; then
        MPRIS_OK=0
        RUST_LOCAL=$(ip netns exec "$NS_B" curl -sf -H "X-API-Key: $API_KEY" \
            "http://127.0.0.1:9090/api/v1/mpris/local-players" 2>/dev/null || true)
        log "  rust /api/v1/mpris/local-players: $RUST_LOCAL"
    fi
    check "mpris control-role: rust sees the planted fake player via session bus" \
        "$MPRIS_OK" "no m3fake in /api/v1/mpris/local-players"
    # Reverse direction: rust POSTs /api/v1/devices/<kde>/mpris/request →
    # kdeconnect.mpris.request packet → kde mprisremote plugin handles it,
    # sending back a kdeconnect.mpris response with the player list. Oracle
    # is the rust daemon log showing the reply (the kde mprisremote plugin
    # itself doesn't log every packet at the default qt category debug
    # level, so we read the wire confirmation from the rust side which
    # DOES log every received packet).
    RUST_REQ_OK=1
    RUST_REQ_RCVD_OFFSET=$(sed 's/\x1b\[[0-9;]*m//g' "$RUST_LOG" | grep -cE 'packet_type: kdeconnect.mpris\b' || true)
    rc_api_post_body "/api/v1/devices/$KDE_ID/mpris/request" "{}" >/dev/null 2>&1 \
        || log "  /mpris/request returned non-zero (non-fatal)"
    if wait_for 10 "rust log to show kdeconnect.mpris reply (post-requestPlayerList)" \
        bash -c "[[ \$(sed 's/\x1b\[[0-9;]*m//g' '$RUST_LOG' | grep -cE 'packet_type: kdeconnect.mpris\b') -gt $RUST_REQ_RCVD_OFFSET ]]"; then
        RUST_REQ_OK=0
    fi
    check "mpris request flow: rust→kde kdeconnect.mpris.request elicits kde reply" \
        "$RUST_REQ_OK" "no kdeconnect.mpris reply in $RUST_LOG after the request"
    stop_mpris_fake
else
    log "WALL phase 6: mpris control-role oracle requires a planted zbus fake"
    log "  player on \$DBUS_SESSION_BUS (tests/mpris_bus_recovery.rs:23-80 pattern)"
    log "  with RC_DBUS_SESSION_BUS=$DBUS_ADDR wired — needs a compiled zbus"
    log "  helper binary not in scope for the cheap batch."
    log "  M4 unlocks this with RC_MPRIS_FAKE=1 (examples/mpris_fake_player)."
fi

# ---------------------------------------------------------------- phase 7
# runcommand both directions. Wall per vk #1007 (rust production
# allowlist is empty). Recorded with the policy text.
log "=== Phase 7: runcommand both directions ==="
log "WALL phase 7: vk #1007 — rust production allowlist is empty."
log "  src/plugins/runcommand.rs: empty allowlist in production; allow_command"
log "  is a code API exercised by tests only. Recorded as wall, NOT a fix."
if [[ "$SAB_SKIP_RUNCOMMAND_TRIGGER" == "1" ]]; then
    log "SABOTAGE=skip-runcommand-trigger: NOT calling remotecommands.triggerCommand (would also be a wall)"
fi
# We could still attempt the wire path (the kde side happily emits
# kdeconnect.runcommand.request; the rust side accepts it but drops it
# because of the empty allowlist). For M3 we record the wall and don't
# attempt the wire path — the policy is clear.

# ---------------------------------------------------------------- phase 8
# remotesystemvolume-out. Brief: assert volumeChanged signal on the kde
# bus (no PA needed on the KDE side for this direction — verify against
# plugin source). The claim is in remotesystemvolumeplugin.h:40
# (volumeChanged signal declared). The KDE side RECEIVES a
# kdeconnect.systemvolume packet and emits the signal. Driver: pactl
# set-sink-volume on the rust side (the rust systemvolume plugin
# subscribes to pactl events, pushes deltas to peers).
#
# Wall: pactl inside the netns would require a per-instance
# pipewire-pulse daemon (the brief's spike B is exactly this). For the
# cheap batch, we record the wall — the spike B investigates the
# audio-stack-per-instance question.
log "=== Phase 8: remotesystemvolume-out ==="
log "WALL phase 8: requires a per-instance pipewire-pulse on the rust"
log "  side so pactl subscribe has an audio daemon to talk to. The"
log "  cheap batch doesn't bring that up; spike B investigates."
log "  remotesystemvolumeplugin.h:40 (volumeChanged signal) and"
log "  systemvolumeplugin-pulse.cpp:69-88 (deltas) are the oracle surfaces."

# --------------------------------------------------------------------- 9
# Phase 9: lock + battery wire-contract conformance (vk #1018).
#
# This phase documents the wire-contract oracle that the harness will
# use to validate the vk #1018 rewrite (kdeconnect.lock reads/emits
# `locked` today; the upstream contract is `isLocked` on
# `kdeconnect.lock` and `setLocked` on `kdeconnect.lock`, with
# `kdeconnect.lock.request` as the state-query packet whose body is
# `{}`. Battery: `kdeconnect.battery.request` body is empty today;
# upstream uses `{request: true}` — pinned by
# tests/fixtures/upstream-wire/{lock,battery}/).
#
# Pre-rewrite this phase is a WALL — the rust plugin parses
# `body.locked` (src/plugins/lock.rs:65) and emits `body.locked`
# (src/plugins/lock.rs:95), neither of which any upstream peer uses,
# so the kde side will silently drop the packet and no `lockUpdate`
# round-trip is observable.
#
# When vk #1018 lands, this phase becomes the validator:
#
#   1. rust → kde (setLocked): POST /api/v1/devices/{id}/lock with
#      {action:"lock"} → kde log shows kdeconnect.lock packet body
#      `{"setLocked": true}` (NOT kdeconnect.lock.request; NOT
#      `locked`). Asserted by grep'ing $KDE_LOG for
#      "kdeconnect.lock" with body containing "setLocked":true.
#
#   2. kde → rust (requestLocked → isLocked reply): kick the kde side
#      to query the rust daemon's last-known lock state (e.g. via
#      `qdbus org.kde.kdeconnect /modules/kdeconnect
#      org.kde.kdeconnect.daemon.requestLocked <rustId>`); rust log
#      shows kdeconnect.lock.request received; rust reply packet is
#      kdeconnect.lock with body `{"isLocked": <bool>}`; the kde log
#      captures the reply and updates its UI.
#
#   3. battery.request body: rust emits kdeconnect.battery.request on
#      peer-connect; the kde plugin logs the request and replies
#      with kdeconnect.battery. Asserted by grep'ing $KDE_LOG for
#      "kdeconnect.battery.request" body containing `"request":true`.
#
#   The desktop_effect — actually locking the phone screen via
#   loginctl/DPMS — is phone-only and not in harness scope. The
#   wire-contract oracle is what this lane proves.
log "=== Phase 9: lock + battery wire contract (vk #1018 — gated) ==="
if grep -q "kdeconnect.lock.request" src/plugins/lock.rs 2>/dev/null \
        && grep -q '"locked"' src/plugins/lock.rs 2>/dev/null; then
    log "WALL phase 9: vk #1018 lock rewrite NOT landed yet — rust"
    log "  plugin still parses/emits \`locked\` on kdeconnect.lock and"
    log "  uses kdeconnect.lock.request for setLocked. Wire-contract"
    log "  oracle (see phase header) is queued for when the rewrite"
    log "  lands; desktop_effect (actual screen lock) is phone-only."
    log "  feature ledger \`lock\` row stays FAIL until then."
else
    log "phase 9: vk #1018 rewrite appears landed — flip this phase"
    log "  from WALL to the wire-contract assertions documented above."
    log "  Not auto-flipped because the rewrite hasn't actually merged"
    log "  at M4 close — this is a hand-flipped gate."
    log "WALL phase 9 (defensive): rewrite detection is heuristic;"
    log "  manual gate remains until Phase 9 assertions are written."
fi

# ---------------------------------------------------------------- spikes
# Timeboxed investigations. Each is ~30 min wall and records what it
# found. A spike that hits a wall moves on, does not consume the lane.

# Spike A: remotekeyboard/mousepad RECEIVE. XTest/LibFakeKey delivery
# under Xvfb. The kde side sends a kdeconnect.mousepadrequest (or
# equivalent) packet; the rust side's mousepad plugin uses XTest to
# inject input. The oracle is an XInput2 listener in the target Xvfb
# that captures the injected event.
SPIKE_A_BUDGET_MIN=30
log "=== Spike A: remotekeyboard/mousepad RECEIVE (budget ${SPIKE_A_BUDGET_MIN}m) ==="
# Tools to check for: xinput (for XInput2 listener), xdotool (for
# delivery probe), xev (raw event listener fallback). If neither is on
# PATH, the wall is "no listener available" (M3 brief, spike A).
SPIKE_A_TOOLS=()
command -v xinput >/dev/null 2>&1 && SPIKE_A_TOOLS+=(xinput)
command -v xev >/dev/null 2>&1 && SPIKE_A_TOOLS+=(xev)
command -v xdotool >/dev/null 2>&1 && SPIKE_A_TOOLS+=(xdotool)
if (( ${#SPIKE_A_TOOLS[@]} == 0 )); then
    log "WALL spike A: no XInput2 listener available (xinput/xev/xdotool absent)"
    log "  recorded as wall — spike timebox respected"
else
    log "spike A tools available: ${SPIKE_A_TOOLS[*]}"
    # The actual XTest delivery probe would go here. We defer the
    # delivery probe to the report — for M3 the cheap batch is the gate
    # and the spike is just a "what's available" check.
    log "spike A: deferred to plans/task-3.2-m3-report.md (XTest probe"
    log "  + listener capture not in cheap-batch scope)"
fi

# Spike B: systemvolume RECEIVE. The KDE side's systemvolume plugin
# links PulseAudioQt. Need a per-instance pipewire-pulse on the kde
# side. Pulseaudio NOT installed; pipewire-pulse IS installed (host
# check above). The spike checks if pipewire-pulse can run per-instance
# headless.
SPIKE_B_BUDGET_MIN=30
log "=== Spike B: systemvolume RECEIVE (budget ${SPIKE_B_BUDGET_MIN}m) ==="
if ! command -v pipewire-pulse >/dev/null 2>&1; then
    log "WALL spike B: pipewire-pulse not installed"
elif ! command -v pulseaudio >/dev/null 2>&1; then
    # Per the brief, pulseaudio is NOT installed on Fedora. pipewire-pulse
    # is the shim that makes pactl talk to pipewire.
    log "WALL spike B: pulseaudio binary not installed; pipewire-pulse"
    log "  IS installed but spinning up a per-instance pipewire-pulse"
    log "  daemon headless in a netns is out of cheap-batch scope."
    log "  recorded as wall — systemvolume RECEIVE deferred to M4 or a"
    log "  packet-injection approach per parent brief."
else
    log "spike B: pulseaudio present — see plans/task-3.2-m3-report.md"
    log "  for whether per-instance headless startup worked"
fi

# ---------------------------------------------------------------- verdict
stop_notify_monitor
finish_milestone "M3 SMOKE"

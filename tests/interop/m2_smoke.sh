#!/usr/bin/env bash
# Task 3.2 M2 (vk #991) — kdeconnectd <-> rust-connect scripted pairing +
# reconnect smoke.
#
# Architecture and top-of-file citations live in tests/interop/lib.sh.
# This file is the M2-specific slice: pairwise pairing (both directions),
# veth-flap reconnect, and restart persistence. The shared topology, debug
# helpers, and zero-leak cleanup come from lib.sh.
#
# ACCEPTANCE (plan § M2):
#   both-direction pairing via D-Bus + REST, asserted on the OTHER
#   implementation; veth flap with reconnect asserted on both sides.
#
# SURFACES (M2-specific, full citations in the report):
#   KDE device iface: /modules/kdeconnect/devices/<id> (device.h:55-61),
#     methods requestPairing / acceptPairing / unpair (device.h:113-127),
#     signal pairStateChanged(int) (device.h:134). PairState enum is
#     0=NotPaired, 1=Requested, 2=RequestedByPeer, 3=Paired (pairstate.h:10-15).
#   KDE pairing timeout: 30s (pairinghandler.h:20).
#   KDE trust store: <XDG_CONFIG_HOME>/kdeconnect/trusted_devices INI
#     (kdeconnectconfig.cpp:55-62).
#   Rust pair endpoint: POST /api/v1/devices/<id>/pair (router.rs:64),
#     handler pair_device (handles/device.rs:154). Dispatches by
#     has_incoming_request: ACCEPT path sends pair_response(true),
#     INITIATE path sends pair_request (handles/device.rs:160-265).
#   SSE stream: /api/v1/events (router.rs:240, sse.rs:28) — not directly
#     needed by the harness; the verification_key on the GET
#     /api/v1/devices/<id> response is the surfaced "incoming request"
#     signal (handles/device.rs:116-118, test_pair_initiate_surfaces_
#     verification_key).

set -u

# Translate the external RC_M2_* names into the generic names lib.sh expects.
RC_BIN="${RC_M2_BIN:-}"
RC_SABOTAGE="${RC_M2_SABOTAGE:-}"
MILESTONE_PREFIX="m2"
WORK_PREFIX="rc-m2"

# shellcheck source=tests/interop/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# Sabotage parsing — RC_M2_SABOTAGE picks a single phase to break.
# Each sabotage name is a red-before-green proof that the corresponding
# assertion can fail, NOT a normal-interface mode.
SAB_SKIP_RUST_ACCEPT=0
SAB_SKIP_KDE_ACCEPT=0
SAB_NO_TRUSTED_DEVICES=0
case "$RC_SABOTAGE" in
    skip-rust-accept)
        SAB_SKIP_RUST_ACCEPT=1
        ;;
    skip-kde-accept)
        SAB_SKIP_KDE_ACCEPT=1
        ;;
    no-trusted-devices)
        SAB_NO_TRUSTED_DEVICES=1
        ;;
    "")
        ;;
    *)
        die "unknown RC_M2_SABOTAGE: $RC_SABOTAGE (allowed: skip-rust-accept | skip-kde-accept | no-trusted-devices)"
        ;;
esac
if [[ -n "$RC_SABOTAGE" ]]; then
    log "SABOTAGE mode active: $RC_SABOTAGE — this run is expected to FAIL"
fi

# ---------------------------------------------------------------- phases
# Phase 0: discovery. M2 rides the same mutual-discovery path M1 proved.
start_kde
start_rust
nudge_kde_for_discovery
wait_for_mutual_discovery 60

if [[ -z "$RUST_ID" ]]; then
    die "discovery failed — RUST_ID empty, cannot proceed with pairing"
fi
log "mutual discovery OK: KDE_ID=$KDE_ID  RUST_ID=$RUST_ID"

# ---------------------------------------------------------------- Phase 1
# KDE-initiated pair:
#   1. harness calls requestPairing on the kde device object for the rust id
#   2. kde sends pair_request packet; rust side registers the incoming request
#      (verification_key becomes surface-able on GET /api/v1/devices/<kde_id>)
#   3. harness calls REST POST /api/v1/devices/<kde_id>/pair — the handler's
#      ACCEPT branch (handles/device.rs:160) sends pair_response(true)
#   4. kde receives pair=true → state 3 (Paired); rust side marks Paired
log "=== Phase 1: kde-initiated pair ==="
kde_request_pairing "$RUST_ID" \
    || die "kde requestPairing call failed"

# Wait for the rust side to have the incoming request reflected in the API.
# Without this, the harness REST call would land on the INITIATE branch and
# race against the kde-incoming pair request — both sides would each try
# the other direction at once. The verification_key field is the surfaced
# signal (handles/device.rs:116-118).
wait_for 40 "rust incoming pair request (verification_key surfaced)" \
    rust_incoming_pair_request "$KDE_ID" \
    || die "rust side never surfaced an incoming pair request (kde's pair_request packet not received?)"

if [[ "$SAB_SKIP_RUST_ACCEPT" == "1" ]]; then
    log "SABOTAGE=skip-rust-accept: NOT calling REST POST /pair — expect kde timeout"
    # Pairing timeout is 30s (pairinghandler.h:20); the assertion below
    # is the red proof that without the rust accept, the pair never lands.
    sleep 35
    KDE_PS=$(kde_pair_state_as_int "$RUST_ID")
    RUST_PS=$(rust_pair_state "$KDE_ID")
    check "kde still UNPAIRED after rust accept skipped (sabotage)" \
        "$([ "$KDE_PS" != "3" ] && echo 0 || echo 1)" \
        "kde pairStateAsInt=$KDE_PS (expected !=3)"
    check "rust still UNPAIRED after rust accept skipped (sabotage)" \
        "$([ "$RUST_PS" != "paired" ] && echo 0 || echo 1)" \
        "rust pair_state=$RUST_PS (expected !=paired)"
    # Confirm the timeout is the failure mode by checking the kde log.
    if grep -qE "pairingTimeout|Pairing timeout|timeout" "$KDE_LOG" 2>/dev/null; then
        log "  kde-side timeout confirmed in $KDE_LOG"
    fi
    finish_milestone "M2 SMOKE (skip-rust-accept)"
    exit 0
fi

# Accept on the rust side via REST.
RUST_ACCEPT_RESP=$(rust_pair "$KDE_ID")
[[ -n "$RUST_ACCEPT_RESP" ]] || die "rust_pair($KDE_ID) returned nothing"
log "rust accepted pair: $RUST_ACCEPT_RESP"

# Wait for both sides to converge to Paired.
wait_for 30 "kde pairStateAsInt=3 for $RUST_ID" kde_is_paired "$RUST_ID"
wait_for 30 "rust pair_state=paired for $KDE_ID" rust_is_paired "$KDE_ID"

KDE_PS1=$(kde_pair_state_as_int "$RUST_ID")
RUST_PS1=$(rust_pair_state "$KDE_ID")
check "kde Paired after kde-initiated pair (pairStateAsInt=3)" \
    "$([ "$KDE_PS1" == "3" ] && echo 0 || echo 1)" \
    "got $KDE_PS1"
check "rust Paired after kde-initiated pair (pair_state=paired)" \
    "$([ "$RUST_PS1" == "paired" ] && echo 0 || echo 1)" \
    "got $RUST_PS1"

# Trust store artifact (kdeconnectconfig.cpp:55-62).
check "kde trusted_devices file exists" \
    "$([ -f "$KDE_TRUSTED_DEVICES" ] && echo 0 || echo 1)" \
    "$KDE_TRUSTED_DEVICES missing"
TRUSTED_COUNT_1=$(kde_trusted_count)
check "kde trusted_devices is non-empty" \
    "$([ "$TRUSTED_COUNT_1" -gt 0 ] && echo 0 || echo 1)" \
    "$(kde_trusted_count) entries (expected >=1)"

# TLS-established link on TCP 1716 — assert the connection exists, do not
# try to parse the encrypted payload. From inside ns A (where the
# connecting side lives in our actual test, despite the brief's note
# that upstream kdeconnectd is "the TCP listener" — both daemons listen
# on 1716 per the rust.toml and kdeconnectd's hardcoded port, and our
# daemon is the one rust-initiates to), an ESTABLISHED TCP socket to
# the rust peer has dport = :1716 (kde's outgoing ephemeral source
# port, rust's well-known destination port).
# TLS-established link on TCP 1716 — assert the connection happened,
# not that it's alive RIGHT NOW. The connection lifecycle in this test
# is racier than the brief suggests: both daemons listen on 1716
# (rust.toml: tcp_port = 1716, kdeconnectd: hardcoded), and kdeconnectd
# fires a "Same-cert redial" cycle that closes the first TLS link and
# adopts a fresh one within a second (rust log: incoming_connection_
# replacing + stale_disconnect). So an ESTABLISHED check at a single
# moment can land between cycles and miss the state entirely.
#
# What we CAN prove after a successful pair:
#  - rust's 1716 listener is in LISTEN state (the daemon is up; if it
#    wasn't, no ESTAB could ever have existed).
#  - the rust daemon log captured at least one incoming_connection_
#    established for the kde device id (proof of a real TLS handshake
#    reaching the listener).
#  - the rust daemon log captured the encrypted_identity_received /
#    identity_exchange_complete pair (proof the TLS handshake finished
#    — anything below that point is plaintext, but it crossed the
#    listener).
TLS_OK=1
# (1) ns B must show the 1716 LISTEN socket. State 0A = LISTEN,
# local port 06B4 = 1716.
NS_B_TCP="$(ip netns exec "$NS_B" cat /proc/net/tcp 2>/dev/null || true)"
LISTEN_OK=0
if printf '%s\n' "$NS_B_TCP" | grep -qE '^ +[0-9]+: 00000000:06B4 .* 0A '; then
    LISTEN_OK=1
fi
# (2) rust log must show incoming_connection_established for the kde id.
RUST_TLS_OK=0
if sed 's/\x1b\[[0-9;]*m//g' "$RUST_LOG" 2>/dev/null \
        | grep -qE "Incoming connection established.*$KDE_ID"; then
    RUST_TLS_OK=1
fi
# (3) rust log must show the TLS handshake crossing the listener.
RUST_HANDSHAKE_OK=0
if sed 's/\x1b\[[0-9;]*m//g' "$RUST_LOG" 2>/dev/null \
        | grep -qE "encrypted_identity_received.*$KDE_ID"; then
    RUST_HANDSHAKE_OK=1
fi
if [[ "$LISTEN_OK" == "1" && "$RUST_TLS_OK" == "1" && "$RUST_HANDSHAKE_OK" == "1" ]]; then
    TLS_OK=0
fi
check "TLS link established: 1716 LISTEN + kde TLS handshake completed" \
    "$TLS_OK" \
    "LISTEN_OK=$LISTEN_OK RUST_TLS_OK=$RUST_TLS_OK RUST_HANDSHAKE_OK=$RUST_HANDSHAKE_OK"

# Monitor log: pairStateChanged signal must have been emitted on the kde
# device's path with the Paired value (3).
SIGNAL_PAIR=1
if grep -qE "pairStateChanged" "$MONITOR_LOG" && grep -qE "pairStateChanged \(3,\)" "$MONITOR_LOG"; then
    SIGNAL_PAIR=0
fi
check "pairStateChanged signal observed on the kde private bus" \
    "$SIGNAL_PAIR" "no pairStateChanged (3,) in $MONITOR_LOG"

# ---------------------------------------------------------------- Phase 2
# Restart KDE between phases. Rationale (M2 finding, see report):
# kdeconnect-kde's Device::privateReceivedPacket (device.cpp:391-394)
# calls unpair() on EVERY non-pair packet from a non-Paired device.
# After Phase 1's unpair, the kdeconnectd goes into a state where it
# emits unpaired() → KdeConnectConfig::removeTrustedDevice repeatedly
# for each buffered plugin packet from rust's pre-unpair send queue —
# each iteration does a disk write (QSettings IniFormat + QSaveFile
# temp+rename), backing up the PairingHandler event queue enough that
# rust's pair=true packet sits unprocessed for tens of seconds. The test
# needs a fresh KDE state for Phase 2; the trust file from Phase 1 is
# preserved by the restart (kdeconnect-kde reads it on startup —
# kdeconnectconfig.cpp:55-62), so the new device object reloads with
# the same Paired state and Phase 2's flow is rust-initiated on top of
# that. This still exercises both directions because Phase 1 was
# kde-initiated; Phase 2 restarts KDE to get a clean slate, then
# rust-initiates.
log "=== Phase 2: restart KDE, then rust-initiated pair ==="
restart_kde
# Restart resets the daemon's in-memory PairingHandler; the trust file
# still holds Phase 1's entry. We want Phase 2 to start from a clean
# pairing state, not from "Paired", so unpair explicitly.
rust_unpair "$KDE_ID" >/dev/null \
    || die "rust_unpair($KDE_ID) failed after KDE restart"
wait_for 30 "kde pairStateAsInt=0 post-unpair" kde_is_unpaired "$RUST_ID" \
    || die "kde side never dropped to NotPaired after unpair"

# Rust-initiated pair:
#   1. harness calls REST POST /api/v1/devices/<kde_id>/pair — the handler
#      INITIATE branch (handles/device.rs:221+) sends pair_request packet
#   2. kde side receives the pair request → state 2 (RequestedByPeer); kde
#      emits pairingRequestsChanged signal on the daemon (daemon.h:82)
#   3. harness calls acceptPairing on the kde device object for the rust id
#   4. kde sends pair_response(true); rust side marks Paired
log "--- rust-initiated pair ---"
RUST_INIT_RESP=$(rust_pair "$KDE_ID")
[[ -n "$RUST_INIT_RESP" ]] || die "rust_pair($KDE_ID) initiate returned nothing"
log "rust initiated pair: $RUST_INIT_RESP"

# Capture the monitor-log offset so we only inspect events from now on
# (the pair-state signals above belong to Phase 1).
PHASE2_LOG_OFFSET=$(wc -l < "$MONITOR_LOG" 2>/dev/null || echo 0)

# Wait for kde to receive the pair request. The brief says "kde sees
# `pairingRequestsChanged`"; we observe it via the D-Bus monitor. This
# is faster than polling gdbus for pairStateAsInt (which is also subject
# to kde's 30s pairing timer turning the state back to NotPaired before
# a slow poll could land).
wait_for 30 "kde pairingRequestsChanged signal after rust-initiated pair" \
    bash -c "sed -n '${PHASE2_LOG_OFFSET},\$p' '$MONITOR_LOG' | grep -qE 'pairingRequestsChanged'" \
    || die "kde never emitted pairingRequestsChanged after rust-initiated pair"
wait_for 30 "kde pairStateChanged (2,)" \
    bash -c "sed -n '${PHASE2_LOG_OFFSET},\$p' '$MONITOR_LOG' | grep -qE 'pairStateChanged \(2,\)'" \
    || die "kde pairStateChanged (2,) never observed after rust-initiated pair"

# D-Bus signal: pairingRequestsChanged must have been emitted on the
# daemon's path (daemon.h:82) AFTER the rust-initiated pair (i.e. in the
# Phase 2 slice).
SIGNAL_PAIR_REQ=1
sed -n "${PHASE2_LOG_OFFSET},\$p" "$MONITOR_LOG" \
    | grep -qE "pairingRequestsChanged" && SIGNAL_PAIR_REQ=0
check "pairingRequestsChanged signal observed on the kde daemon (Phase 2)" \
    "$SIGNAL_PAIR_REQ" "no pairingRequestsChanged in $MONITOR_LOG after Phase 2"

if [[ "$SAB_SKIP_KDE_ACCEPT" == "1" ]]; then
    log "SABOTAGE=skip-kde-accept: NOT calling acceptPairing — expect rust timeout"
    # The kde side starts a 30s timer for the request; without accept the
    # pair factory drops back to NotPaired. The rust side's pair_request
    # stays pending until it sees a pair_response or the requesting
    # connection drops. Wait past the kde timeout, then assert both sides
    # are still unpaired.
    sleep 35
    KDE_PS=$(kde_pair_state_as_int "$RUST_ID")
    RUST_PS=$(rust_pair_state "$KDE_ID")
    check "kde still UNPAIRED after kde accept skipped (sabotage)" \
        "$([ "$KDE_PS" != "3" ] && echo 0 || echo 1)" \
        "kde pairStateAsInt=$KDE_PS (expected !=3)"
    check "rust still UNPAIRED after kde accept skipped (sabotage)" \
        "$([ "$RUST_PS" != "paired" ] && echo 0 || echo 1)" \
        "rust pair_state=$RUST_PS (expected !=paired)"
    finish_milestone "M2 SMOKE (skip-kde-accept)"
    exit 0
fi

# Accept on the kde side.
kde_accept_pairing "$RUST_ID" \
    || die "kde acceptPairing call failed"

# Wait for both sides to converge to Paired.
wait_for 30 "kde pairStateAsInt=3 for $RUST_ID (post-rust-initiated)" \
    kde_is_paired "$RUST_ID"
wait_for 30 "rust pair_state=paired for $KDE_ID (post-rust-initiated)" \
    rust_is_paired "$KDE_ID"

KDE_PS2=$(kde_pair_state_as_int "$RUST_ID")
RUST_PS2=$(rust_pair_state "$KDE_ID")
check "kde Paired after rust-initiated pair (pairStateAsInt=3)" \
    "$([ "$KDE_PS2" == "3" ] && echo 0 || echo 1)" \
    "got $KDE_PS2"
check "rust Paired after rust-initiated pair (pair_state=paired)" \
    "$([ "$RUST_PS2" == "paired" ] && echo 0 || echo 1)" \
    "got $RUST_PS2"

TRUSTED_COUNT_2=$(kde_trusted_count)
check "kde trusted_devices still non-empty after re-pair" \
    "$([ "$TRUSTED_COUNT_2" -gt 0 ] && echo 0 || echo 1)" \
    "$TRUSTED_COUNT_2 entries"

# ---------------------------------------------------------------- Phase 3
# Reconnect on veth flap. The pair state must SURVIVE — this is the
# trusted_devices persistence claim, not a fresh pairing.
if [[ "$SAB_NO_TRUSTED_DEVICES" == "1" ]]; then
    log "SABOTAGE=no-trusted-devices: removing trusted_devices before reconnect"
    # Wipe the trust store AFTER a successful pair. Both sides still
    # believe they are paired in memory; the reconnect phase should
    # expose the missing trust on the kde side.
    rm -f "$KDE_TRUSTED_DEVICES"
    log "removed $KDE_TRUSTED_DEVICES"
fi

log "=== Phase 3: reconnect on veth flap ==="
# Capture the position of the monitor log so we can grep the slice that
# belongs to the reconnect window.
RECONNECT_LOG_OFFSET=$(wc -l < "$MONITOR_LOG" 2>/dev/null || echo 0)

# Record timestamps for the "who redials first" question.
FLAP_DOWN_TS=$(date -Ins)

# Flap the kde end (the brief: ip link set <kde-end> down → brief wait → up).
ip netns exec "$NS_A" ip link set "$VETH_A" down \
    || die "could not bring $VETH_A down"

# Two windows of observation, both bounded by the test wall-clock budget:
# (a) rust side — its tokio read loop exits with Disconnected almost
#     immediately once either side tries to use the dead socket. The
#     log marker is "incoming_reconnect_triggered" or "Packet loop
#     exited" with loop_result: "Disconnected".
# (b) kde side — `reachableChanged(false)` requires KDE's LanDeviceLink
#     socket to emit QAbstractSocket::disconnected. With no traffic the
#     kernel doesn't surface a TCP error on a veth-down idle socket,
#     so this signal is best-effort (we record what we see, we don't
#     fail the test on it). The brief itself names the second
#     mechanism — forceOnNetworkChange — as the active re-discovery
#     path; that's what we rely on below.
wait_for 10 "rust side to detect disconnect (Packet loop exited)" \
    bash -c "sed 's/\x1b\[[0-9;]*m//g' '$RUST_LOG' | grep -q 'Packet loop exited'"
RUST_DISCONNECT_OBSERVED=0
if sed 's/\x1b\[[0-9;]*m//g' "$RUST_LOG" | grep -q 'Packet loop exited'; then
    RUST_DISCONNECT_OBSERVED=1
fi
log "rust detected disconnect: $RUST_DISCONNECT_OBSERVED (1=rust kicked reconnect, 0=rust waited)"

# Best-effort: see if kde ever emitted reachableChanged(false) during the
# down window. We don't fail on this — it's an observation.
sleep 2
REACHABLE_LOST=1
sed -n "${RECONNECT_LOG_OFFSET},\$p" "$MONITOR_LOG" \
    | grep -qE "reachableChanged.*false" && REACHABLE_LOST=0
log "kde reachableChanged(false) observed during flap: $((1 - REACHABLE_LOST)) (best-effort, not asserted)"

# Bring the link back up.
ip netns exec "$NS_A" ip link set "$VETH_A" up \
    || die "could not bring $VETH_A back up"

# Active re-discovery mechanism (brief: "Second mechanism:
# forceOnNetworkChange after flap"). KDE's LanLinkProvider has a 7s
# identity-broadcast timer (lanlinkprovider.cpp:148) but it doesn't
# guarantee a re-emission right when our veth comes back up —
# forceOnNetworkChange collapses the debounce window and forces an
# immediate UDP broadcast (and mDNS query). After it fires, KDE
# re-discovers rust, opens the TCP/TLS link, and emits deviceAdded +
# reachableChanged(true).
kde_force_on_network_change
log "forceOnNetworkChange issued (kde re-broadcast)"

# Wait for the kde side to re-discover rust. deviceAdded on the daemon
# iface is the most reliable signal — the daemon's deviceListChanged
# signal follows it.
wait_for 30 "kde deviceAdded for $RUST_ID after re-discovery" \
    bash -c "sed -n '${RECONNECT_LOG_OFFSET},\$p' '$MONITOR_LOG' | grep -qE 'daemon.deviceAdded.*$RUST_ID'"
REACHABLE_BACK=1
sed -n "${RECONNECT_LOG_OFFSET},\$p" "$MONITOR_LOG" \
    | grep -qE "reachableChanged.*true" && REACHABLE_BACK=0
check "kde reachableChanged(true) after re-discovery" \
    "$REACHABLE_BACK" \
    "no reachableChanged(true) in monitor after re-discovery"
REACHABLE_BACK_TS=$(date -Ins)

# Pair state must STAY Paired — this is the persistence claim, not a
# re-pair.
KDE_PS3=$(kde_pair_state_as_int "$RUST_ID")
RUST_PS3=$(rust_pair_state "$KDE_ID")
check "kde pair state STAYS Paired after veth flap" \
    "$([ "$KDE_PS3" == "3" ] && echo 0 || echo 1)" \
    "got $KDE_PS3"
check "rust pair state STAYS Paired after veth flap" \
    "$([ "$RUST_PS3" == "paired" ] && echo 0 || echo 1)" \
    "got $RUST_PS3"

# Trust store must STILL be present + non-empty if it wasn't sabotaged.
if [[ "$SAB_NO_TRUSTED_DEVICES" != "1" ]]; then
    check "kde trusted_devices still non-empty after veth flap" \
        "$([ "$(kde_trusted_count)" -gt 0 ] && echo 0 || echo 1)" \
        "$(kde_trusted_count) entries (expected >=1)"
fi

# Who redials first? The rust daemon runs reconnect_loop (task 2.2);
# upstream kdeconnectd does NOT redial (waits for the peer). The
# harness OBSERVES this difference, doesn't judge it. Already captured
# above (RUST_DISCONNECT_OBSERVED) — log it again for the verdict.
log "rust side reconnect activity observed: $RUST_DISCONNECT_OBSERVED (1=rust kicked a reconnect, 0=rust waited)"

# Verify the rust side actually re-sees kde via the REST API (the
# canonical "rust knows about kde" oracle).
sleep 2
RUST_RESEEN=1
if rust_found_id "$KDE_ID"; then
    RUST_RESEEN=0
fi
check "rust still sees kde after re-discovery" \
    "$RUST_RESEEN" "$KDE_ID missing from /api/v1/devices after re-discovery"

# ---------------------------------------------------------------- Phase 4
# Restart persistence. The pair state must survive a full stop/start of
# both daemons with the SAME XDG / data directories. This is the real
# trusted_devices persistence claim — the in-memory state is gone.
log "=== Phase 4: restart persistence ==="
restart_kde
restart_rust

# After restart, kde's pair state should reload from the on-disk
# trusted_devices file (only if the file is still present — the
# no-trusted-devices sabotage removes it).
if [[ "$SAB_NO_TRUSTED_DEVICES" == "1" ]]; then
    log "SABOTAGE=no-trusted-devices: after restart, expect NOT Paired"
    # The kde side no longer has the rust id in trusted_devices — the
    # restarted daemon will treat the rust device as NotPaired.
    # Allow it a few seconds to discover rust and see the absent trust.
    KDE_POST_RESTART=$(kde_pair_state_as_int "$RUST_ID" 2>/dev/null || echo "0")
    check "kde pair state NOT Paired after restart (no-trusted-devices sabotage)" \
        "$([ "$KDE_POST_RESTART" != "3" ] && echo 0 || echo 1)" \
        "kde pairStateAsInt=$KDE_POST_RESTART (expected !=3)"
    finish_milestone "M2 SMOKE (no-trusted-devices)"
    exit 0
fi

# Green path: pair state survives restart.
wait_for 30 "kde pairStateAsInt=3 after restart" kde_is_paired "$RUST_ID" \
    || die "kde pair state did not reload as Paired after restart"
wait_for 30 "rust pair_state=paired after restart" rust_is_paired "$KDE_ID" \
    || die "rust pair state did not reload as Paired after restart"

KDE_PS4=$(kde_pair_state_as_int "$RUST_ID")
RUST_PS4=$(rust_pair_state "$KDE_ID")
check "kde pair state persists after restart" \
    "$([ "$KDE_PS4" == "3" ] && echo 0 || echo 1)" \
    "got $KDE_PS4"
check "rust pair state persists after restart" \
    "$([ "$RUST_PS4" == "paired" ] && echo 0 || echo 1)" \
    "got $RUST_PS4"

TRUSTED_COUNT_4=$(kde_trusted_count)
check "kde trusted_devices still non-empty after restart" \
    "$([ "$TRUSTED_COUNT_4" -gt 0 ] && echo 0 || echo 1)" \
    "$TRUSTED_COUNT_4 entries"

# ---------------------------------------------------------------- verdict
finish_milestone "M2 SMOKE"

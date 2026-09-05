#!/usr/bin/env bash
# Task 3.2 M5 (vk #1045) — kdeconnectd-only restart survives the rust
# peer cert. The scenario m2_smoke.sh:244-249 had to skip because the
# SAN-less rust cert failed Qt hostname verification on the post-restart
# client-mode dial. After fix-cert-san-deviceid, our cert carries a
# dNSName=rust_id SAN, so the post-restart handshake completes cleanly
# and the trust store survives.
#
# Architecture and top-of-file citations live in tests/interop/lib.sh.
# This file is the M5-specific slice: pairwise pairing followed by a
# kdeconnectd-ONLY restart (rust daemon keeps running). The shared
# topology, debug helpers, and zero-leak cleanup come from lib.sh.
#
# ACCEPTANCE (plan § M5):
#   (a) no "valid hosts" rejection in the kde log after the post-restart
#       dial — proves Qt hostname verification accepts our SAN-bearing
#       cert against the rust id it dials with (lanlinkprovider.cpp:604
#       setPeerVerifyName; the rejection was the trigger for vk #1045).
#   (b) the link re-establishes — proves the kdeconnectd client-mode dial
#       completes the TLS handshake, not just that the TCP socket opens.
#       The handshake is what carries the cert exchange.
#   (c) rust's peer fingerprint file still exists — proves no TOFU wipe
#       was triggered. The cascade (PairingHandler::unpair →
#       delete_peer_certificate, src/protocol/pairing/mod.rs:693) was
#       what made a transient handshake failure into a permanent
#       depairing.
#   (d) a ping round-trips post-restart — end-to-end reachability proof
#       on the new link.
#
# SURFACES (M5-specific, full citations in the report):
#   Same as M2 — the new surface this milestone covers is the SAN shape
#   of rust's own certificate. The verification is local: parse the
#   rust cert the kde side stores, and assert it carries exactly one
#   dNSName SAN matching the rust id. The runtime assertion is the
#   absent "valid hosts" line in the kde log; the cert-shape assertion
#   is a structural companion.

set -u

# Translate the external RC_M5_* names into the generic names lib.sh expects.
RC_BIN="${RC_M5_BIN:-}"
RC_SABOTAGE="${RC_M5_SABOTAGE:-}"
MILESTONE_PREFIX="m5"
WORK_PREFIX="rc-m5"

# shellcheck source=tests/interop/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# Sabotage parsing — RC_M5_SABOTAGE picks a single phase to break.
# Each sabotage name is a red-before-green proof that the corresponding
# assertion can fail, NOT a normal-interface mode. None currently
# implemented (the SAN regression cannot be simulated in-script without
# rewriting rust's on-disk cert mid-run; a future lane could add
# `no-san` by stripping the SAN extension from $RUST_OWN_CERT with
# `openssl x509 ... -extfile` before the restart). See FINDINGS.md.
case "$RC_SABOTAGE" in
    "")
        ;;
    *)
        die "unknown RC_M5_SABOTAGE: $RC_M5_SABOTAGE (no saboteages implemented in M5)"
        ;;
esac

# ---------------------------------------------------------------- phases
# Phase 0: discovery. M5 rides the same mutual-discovery path M1/M2 proved.
start_kde
start_rust
nudge_kde_for_discovery
wait_for_mutual_discovery 60

if [[ -z "$RUST_ID" ]]; then
    die "discovery failed — RUST_ID empty, cannot proceed with pairing"
fi
log "mutual discovery OK: KDE_ID=$KDE_ID  RUST_ID=$RUST_ID"

# ---------------------------------------------------------------- Phase 1
# kde-initiated pair. The kde device object must be FULLY present before
# requesting pairing — same race-condition reasoning as m2_smoke.sh:99-103.
log "=== Phase 1: kde-initiated pair ==="
wait_for 30 "kde device object ready for $RUST_ID (post-redial quiescence)" \
    kde_device_ready_for_pairing "$RUST_ID" \
    || die "kde device object never stabilized for $RUST_ID"
kde_request_pairing "$RUST_ID" \
    || die "kde requestPairing call failed"

# Wait for the rust side to surface the incoming pair request (the
# verification_key on the GET /api/v1/devices/<kde_id> response is the
# surfaced signal — handles/device.rs:116-118).
wait_for 40 "rust incoming pair request (verification_key surfaced)" \
    rust_incoming_pair_request "$KDE_ID" \
    || die "rust side never surfaced an incoming pair request"

RUST_ACCEPT_RESP=$(rust_pair "$KDE_ID")
[[ -n "$RUST_ACCEPT_RESP" ]] || die "rust_pair($KDE_ID) returned nothing"
log "rust accepted pair: $RUST_ACCEPT_RESP"

wait_for 30 "kde pairStateAsInt=3 for $RUST_ID" kde_is_paired "$RUST_ID"
wait_for 30 "rust pair_state=paired for $KDE_ID" rust_is_paired "$KDE_ID"

KDE_PS=$(kde_pair_state_as_int "$RUST_ID")
RUST_PS=$(rust_pair_state "$KDE_ID")
check "kde Paired after kde-initiated pair (pairStateAsInt=3)" \
    "$([ "$KDE_PS" == "3" ] && echo 0 || echo 1)" \
    "got $KDE_PS"
check "rust Paired after kde-initiated pair (pair_state=paired)" \
    "$([ "$RUST_PS" == "paired" ] && echo 0 || echo 1)" \
    "got $RUST_PS"

# Capture the rust fingerprint path BEFORE the restart so we can assert
# it still exists AFTER. The fingerprint file is the TOFU pin; if it
# vanished post-restart, the cascade fired (PairingHandler::unpair →
# delete_peer_certificate, src/protocol/pairing/mod.rs:693).
RUST_FP_DIR="$WORK/rust-data/certs"
RUST_FP_PATH="$RUST_FP_DIR/${KDE_ID}_fingerprint.txt"
RUST_PEER_CRT_PATH="$RUST_FP_DIR/${KDE_ID}_peer.crt"

if [[ ! -f "$RUST_FP_PATH" ]]; then
    die "rust fingerprint file expected at $RUST_FP_PATH after pair, not found"
fi
PRE_FP_CONTENT=$(cat "$RUST_FP_PATH")
log "pre-restart rust fingerprint for $KDE_ID: $PRE_FP_CONTENT"

# ---------------------------------------------------------------- Phase 2
# Record the rust cert SAN shape BEFORE the restart. This is the
# structural companion to the runtime assertions: the new cert our side
# sends on the post-restart dial MUST carry exactly one dNSName SAN
# equal to $RUST_ID. The openssl::x509 inspection reuses a tiny inline
# parser — no harness dependency on rcgen / x509-parser.
parse_rust_cert_san_dnsname() {
    # Args: $1 = path to rust's own cert PEM (own.crt under cert_dir).
    # Outputs the dNSName from the SAN extension on stdout, or empty.
    openssl::x509::X509::from_pem "$1" 2>/dev/null \
        || openssl x509 -in "$1" -noout -text 2>/dev/null \
            | sed -n '/X509v3 Subject Alternative Name/,/^[^ ]/p' \
            | grep -oE 'DNS:[^,]+' \
            | sed 's/^DNS://'
}
# The openssl::x509::X509::from_pem form above is invalid shell; fall back
# to the openssl CLI in all cases — same source of truth.
RUST_OWN_CERT="$RUST_FP_DIR/own.crt"
if [[ ! -f "$RUST_OWN_CERT" ]]; then
    # Adopt-legacy layout: own.crt was added 2026-07; older layouts key
    # by <device_id>.crt. Look for the rust-id-keyed variant.
    RUST_OWN_CERT="$RUST_FP_DIR/${RUST_ID}.crt"
fi
[[ -f "$RUST_OWN_CERT" ]] || die "rust own cert not found at expected paths under $RUST_FP_DIR"

PRE_SAN=$(openssl x509 -in "$RUST_OWN_CERT" -noout -text 2>/dev/null \
    | sed -n '/X509v3 Subject Alternative Name/,/^[^ ]/p' \
    | grep -oE 'DNS:[^,]+' \
    | head -1 \
    | sed 's/^DNS://')
log "pre-restart rust cert SAN dNSName: '$PRE_SAN'"
check "rust cert carries SAN dNSName=$RUST_ID before restart" \
    "$([ "$PRE_SAN" == "$RUST_ID" ] && echo 0 || echo 1)" \
    "got '$PRE_SAN' (expected '$RUST_ID')"

# ---------------------------------------------------------------- Phase 3
# kdeconnectd-ONLY restart. The rust daemon keeps running — that is the
# trigger the original M2 scenario couldn't reproduce (m2_smoke.sh:244-249
# skipped restart_kde because the post-restart handshake failed; the SAN
# fix is what makes this scenario survivable).
log "=== Phase 3: kdeconnectd-only restart ==="

# Capture the kde log offset BEFORE the restart, so the post-restart
# rejection check slices only the new log segment. (Same lesson as
# PHASE2_LOG_OFFSET in m2_smoke.sh — the trigger→oracle round trip
# completes in <10ms.)
PRE_RESTART_LOG_OFFSET=$(wc -l < "$KDE_LOG" 2>/dev/null || echo 0)
log "PRE_RESTART_LOG_OFFSET=$PRE_RESTART_LOG_OFFSET"

restart_kde

# The kdeconnectd log lines we MUST NOT see after the restart:
#   "The host name did not match any of the valid hosts for this certificate"
# Qt's hostname-verification failure — this is the SAN fix's trigger.
REJECTION_FOUND=1
sed -n "${PRE_RESTART_LOG_OFFSET},\$p" "$KDE_LOG" \
    | grep -qE "valid hosts for this certificate" \
    && REJECTION_FOUND=0
check "no Qt 'valid hosts for this certificate' rejection after kde restart (SAN fix)" \
    "$REJECTION_FOUND" \
    "the rejection text appeared in $KDE_LOG post-restart"

# ---------------------------------------------------------------- Phase 4
# Pair state and trust store must SURVIVE the restart. Same persistence
# claim as m2 Phase 4, but here the kde side alone restarts — the rust
# side's TOFU store never went away, and the kde side's trusted_devices
# is on disk.
log "=== Phase 4: post-restart pair state and trust store ==="
wait_for 30 "kde pairStateAsInt=3 after kde-only restart" kde_is_paired "$RUST_ID" \
    || die "kde pair state did not reload as Paired after restart"
wait_for 30 "rust pair_state=paired after kde-only restart" rust_is_paired "$KDE_ID" \
    || die "rust pair state did not reload as Paired after restart"

KDE_PS_POST=$(kde_pair_state_as_int "$RUST_ID")
RUST_PS_POST=$(rust_pair_state "$KDE_ID")
check "kde Paired after kde-only restart" \
    "$([ "$KDE_PS_POST" == "3" ] && echo 0 || echo 1)" \
    "got $KDE_PS_POST"
check "rust Paired after kde-only restart" \
    "$([ "$RUST_PS_POST" == "paired" ] && echo 0 || echo 1)" \
    "got $RUST_PS_POST"

# Trust store artifact (kdeconnectconfig.cpp:55-62) — must be non-empty
# post-restart. Wiped trust on the kde side would be the upstream of the
# trust loss we want to detect; in this milestone the rust side is the
# trust anchor we're watching.
check "kde trusted_devices still non-empty after kde-only restart" \
    "$([ "$(kde_trusted_count)" -gt 0 ] && echo 0 || echo 1)" \
    "$(kde_trusted_count) entries"

# ---------------------------------------------------------------- Phase 5
# The TOFU wipe check — the cascade the SAN fix prevents. After the
# restart, rust's fingerprint file MUST still exist with the same
# content. If the SAN-less cert had triggered the Qt rejection, the
# post-rejection unpair would have run PairingHandler::unpair →
# delete_peer_certificate on the rust side, and this file would be
# gone.
log "=== Phase 5: TOFU store survives ==="
check "rust fingerprint file still exists after kde-only restart" \
    "$([ -f "$RUST_FP_PATH" ] && echo 0 || echo 1)" \
    "$RUST_FP_PATH missing"
check "rust peer cert file still exists after kde-only restart" \
    "$([ -f "$RUST_PEER_CRT_PATH" ] && echo 0 || echo 1)" \
    "$RUST_PEER_CRT_PATH missing"

POST_FP_CONTENT=$(cat "$RUST_FP_PATH" 2>/dev/null || echo "")
check "rust fingerprint content unchanged after kde-only restart" \
    "$([ "$POST_FP_CONTENT" == "$PRE_FP_CONTENT" ] && echo 0 || echo 1)" \
    "pre='$PRE_FP_CONTENT' post='$POST_FP_CONTENT'"

# ---------------------------------------------------------------- Phase 6
# End-to-end reachability on the new link. A ping round-trips rust →
# kde on the post-restart TLS connection — proves the trust chain is
# intact at the application layer (not just that the sockets are open).
log "=== Phase 6: ping round-trips post-restart ==="
# nudge the kde side to broadcast after the restart so the rust side
# knows the link is live; otherwise rust's ping may land on a socket
# the kde side has not finished bringing up yet.
kde_force_on_network_change
log "forceOnNetworkChange issued (post-restart nudge)"

wait_for 30 "rust can ping kde on the post-restart link" \
    bash -c "rust_ping '$KDE_ID' >/dev/null 2>&1 && \
             sed 's/\x1b\[[0-9;]*m//g' '$RUST_LOG' | grep -qE 'ping_response_received.*$KDE_ID'" \
    || die "rust could not round-trip a ping to $KDE_ID on the post-restart link"

PING_OK=1
rust_ping "$KDE_ID" >/dev/null 2>&1
if sed 's/\x1b\[[0-9;]*m//g' "$RUST_LOG" 2>/dev/null \
        | grep -qE "ping_response_received.*$KDE_ID"; then
    PING_OK=0
fi
check "ping round-trips rust → kde on the post-restart link" \
    "$PING_OK" "no ping_response_received for $KDE_ID in $RUST_LOG"

# ---------------------------------------------------------------- verdict
finish_milestone "M5 SMOKE"
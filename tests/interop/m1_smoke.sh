#!/usr/bin/env bash
# Task 3.2 M1 (vk #991) — kdeconnectd <-> rust-connect identity-exchange smoke.
#
# Architecture and top-of-file citations live in tests/interop/lib.sh.
# This file is the M1-specific slice: identity exchange + discovery-channel
# determination. The shared topology, debug helpers, and zero-leak cleanup
# come from lib.sh.

set -u

# Translate the external RC_M1_* names into the generic names lib.sh expects.
RC_BIN="${RC_M1_BIN:-}"
RC_SABOTAGE="${RC_M1_SABOTAGE:-}"
MILESTONE_PREFIX="m1"
WORK_PREFIX="rc-m1"

# shellcheck source=tests/interop/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

start_kde
start_rust
nudge_kde_for_discovery
wait_for_mutual_discovery 60

# ---------------------------------------------------------------- evidence
# Let tcpdump flush the last packets before reading the pcap.
sleep 2
kill -INT "$TCPDUMP_PID" 2>/dev/null
sleep 1
TCPDUMP_PID=""

check "kde sees rust daemon (D-Bus deviceIdByName)" "$KDE_SEES_RUST" \
    "deviceIdByName($RUST_NAME) stayed empty"

SIGNAL_OK=1
if [[ "$KDE_SEES_RUST" == "0" && -n "$RUST_ID" ]]; then
    grep -q "deviceAdded" "$MONITOR_LOG" && grep -q "$RUST_ID" "$MONITOR_LOG" && SIGNAL_OK=0
fi
check "deviceAdded signal observed on the private bus" "$SIGNAL_OK" \
    "no deviceAdded($RUST_ID) in $MONITOR_LOG"

check "rust sees kdeconnectd (REST /api/v1/devices)" "$RUST_SEES_KDE" \
    "$KDE_ID absent from /api/v1/devices"

# Plaintext identity JSON both directions on UDP 1716, attributable by name.
A_TO_B=1; B_TO_A=1
tcpdump -nn -r "$PCAP" -A "udp port 1716 and src host $IP_A" 2>/dev/null | grep -q "$KDE_NAME" && A_TO_B=0
tcpdump -nn -r "$PCAP" -A "udp port 1716 and src host $IP_B" 2>/dev/null | grep -q "$RUST_NAME" && B_TO_A=0
check "wire: kde->rust identity JSON on UDP 1716 ($KDE_NAME)" "$A_TO_B" "not found in pcap"
check "wire: rust->kde identity JSON on UDP 1716 ($RUST_NAME)" "$B_TO_A" "not found in pcap"

# ------------------------------------- discovery-channel determination
log "--- discovery channel evidence ---"
if [[ "$RUST_SEES_KDE" == "0" ]]; then
    # The rust daemon logs device_discovered (UDP identity received) and
    # mdns_device_resolved (mDNS resolve) as distinct events. The fmt
    # layer emits ANSI colors and `event: "..."` — strip colors first.
    FIRST_RUST=$(sed 's/\x1b\[[0-9;]*m//g' "$RUST_LOG" \
        | grep -E 'event: "(device_discovered|mdns_device_resolved)"' \
        | grep "$KDE_ID" | head -1 || true)
    log "rust-side first discovery event for $KDE_ID: ${FIRST_RUST:-<none found>}"
    case "$FIRST_RUST" in
        *mdns_device_resolved*) log "VERDICT rust-side: mDNS carried kde->rust discovery" ;;
        *device_discovered*)    log "VERDICT rust-side: UDP broadcast carried kde->rust discovery" ;;
        *)                      log "VERDICT rust-side: channel undetermined from logs" ;;
    esac
fi
if [[ -f "$PCAP" ]]; then
    FIRST_B_UDP=$(tcpdump -tttt -nn -r "$PCAP" "udp port 1716 and src host $IP_B" 2>/dev/null | head -1 | awk '{print $1, $2}' || true)
    FIRST_B_MDNS=$(tcpdump -tttt -nn -r "$PCAP" "udp port 5353 and src host $IP_B" 2>/dev/null | head -1 | awk '{print $1, $2}' || true)
    FIRST_A_UDP=$(tcpdump -tttt -nn -r "$PCAP" "udp port 1716 and src host $IP_A" 2>/dev/null | head -1 | awk '{print $1, $2}' || true)
    FIRST_A_MDNS=$(tcpdump -tttt -nn -r "$PCAP" "udp port 5353 and src host $IP_A" 2>/dev/null | head -1 | awk '{print $1, $2}' || true)
    MDNS_COUNT=$(tcpdump -nn -r "$PCAP" "udp port 5353" 2>/dev/null | wc -l)
    log "first rust->kde packet: udp1716=[${FIRST_B_UDP:-none}] mdns5353=[${FIRST_B_MDNS:-none}]"
    log "first kde->rust packet: udp1716=[${FIRST_A_UDP:-none}] mdns5353=[${FIRST_A_MDNS:-none}]"
    log "total mDNS (5353) packets on the wire: $MDNS_COUNT"
    if [[ "$MDNS_COUNT" == "0" ]]; then
        log "VERDICT wire: no mDNS traffic at all — UDP broadcast is the only channel that fired"
    fi
fi
grep -m1 "Using mdnsh\|Using Avahi" "$KDE_LOG" 2>/dev/null | sed 's/^/[m1] kde mDNS backend: /' || true

# Informational: the CLI view with the per-instance env (brief's alt oracle).
if [[ "$KDE_SEES_RUST" == "0" ]]; then
    log "--- kdeconnect-cli -l (per-instance env) ---"
    env "${KDE_ENV[@]}" kdeconnect-cli -l 2>&1 | sed 's/^/[kdeconnect-cli] /' || true
fi

# ---------------------------------------------------------------- verdict
finish_milestone "M1 SMOKE"

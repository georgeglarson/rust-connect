#!/usr/bin/env bash
# tests/interop/lib.sh — shared infrastructure for the kdeconnectd <-> rust-connect
# interop smokes (M1: identity exchange, M2: scripted pairing + reconnect,
# M3: per-plugin flows).
#
# SOURCED, not executed. The milestone smoke (m1_smoke.sh / m2_smoke.sh) must
# set the following before sourcing:
#
#   RC_BIN              path to the built rust-connect binary
#   RC_SABOTAGE         "skip-rust" | "skip-kde" | "" (default "")
#   MILESTONE_PREFIX    "m1" | "m2" — used as a log line prefix (m1|m2)
#   WORK_PREFIX         "rc-m1" | "rc-m2" — drives namespace + work-dir names
#
# All documentation lives in plans/task-3.2-brief.md. The architecture is
# settled there: two netns joined by a veth pair, both ends inside the
# namespaces (NOT ns<->host, which leaks broadcasts to the host's real
# kdeconnect listeners — proven during the 2026-08-14 spike); per-instance
# Xvfb (offscreen QPA is unsafe — KSystemClipboard deref null); per-instance
# private dbus-daemon with activation disabled (a plain --session bus loads
# distro servicedirs and any client can auto-activate a non-isolated
# kdeconnectd); isolated XDG_CONFIG_HOME / XDG_DATA_HOME / XDG_RUNTIME_DIR
# / HOME; the host's avahi/system-bus socket is masked via a private mount
# namespace so the test instance never announces on the real LAN.
#
# Provides the following helpers / globals after sourcing:
#
#   log, fail, die
#   KDE_PID, RC_PID, MON_PID, TCPDUMP_PID, DBUS_PID, XVFB_PID
#   KDE_ID, RUST_ID, KDE_NAME, RUST_NAME, API_KEY
#   KDE_HOME, KDE_RUNTIME, DBUS_ADDR, WORK
#   KDE_ENV (array — pass to env "${KDE_ENV[@]}" …)
#   NS_A, NS_B, VETH_A, VETH_B, IP_A, IP_B, DISPLAY
#   MONITOR_LOG, RUST_LOG, KDE_LOG, PCAP
#   kde_dbus, rc_api, wait_for, check
#   kde_discovered, rust_discovered
#   kde_pair_state_as_int <device_id>   — 0..3 (NotPaired=0, Requested=1,
#                                                  RequestedByPeer=2, Paired=3,
#                                                  per core/pairstate.h:10-15)
#   rust_pair_state <device_id>         — REST /api/v1/devices/:id pair_state
#   rust_trust_count                    — count of trusted_devices entries
#                                          on the kde side via D-Bus
#   kde_trusted_devices_path            — filesystem path to the INI
#   kde_force_on_network_change
#   start_kde, start_rust
#
# M3-only helpers (per-plugin flow surfaces; cited inline):
#   kde_share_urls / kde_share_url / kde_share_text
#   kde_trigger_command / kde_enable_plugin / kde_notify_send
#   kdeclip_get_text / kdeclip_set_text / kde_clipboard_send
#   kde_trigger_command
#   kde_notify_send
#   pactl_set_sink_volume
#   start_notify_monitor / stop_notify_monitor / notify_log_offset
#   rc_api_post_body / rc_api_post_body_raw (M3 JSON-bodied POST helpers)
#   RC_DBUS_SESSION_BUS (override for start_rust; empty = bogus no-such-bus)
#
# Sets an EXIT trap that kills every leftover child, sweeps /proc for any
# process whose environ or cmdline references $WORK, deletes the namespaces
# + veths, and asserts the post-run baselines match the pre-run baselines
# (zero-leak invariant, 2.2 precedent).

# Caller is expected to have `set -u` (both m1 and m2 already do); the
# references below would otherwise explode on unset names.

# ----------------------------------------------------------- skip gate + log
if [[ "$(id -u)" != "0" ]]; then
    printf '[%s] SKIP: not running as root — netns/veth creation needs CAP_NET_ADMIN;\n' "$MILESTONE_PREFIX" >&2
    printf '[%s] SKIP: run via `sudo tests/interop/run.sh` to execute this suite.\n' "$MILESTONE_PREFIX" >&2
    exit 0
fi

[[ -n "$RC_BIN" && -x "$RC_BIN" ]] || { printf '[%s] FAIL: RC_BIN must point at a built rust-connect binary (use tests/interop/run.sh)\n' "$MILESTONE_PREFIX" >&2; exit 1; }
RC_SABOTAGE="${RC_SABOTAGE:-}"

log()  { printf '[%s] %s\n' "$MILESTONE_PREFIX" "$*"; }
fail() { printf '[%s] FAIL: %s\n' "$MILESTONE_PREFIX" "$*" >&2; }
die()  { fail "$*"; exit 1; }

for tool in ip Xvfb dbus-daemon gdbus tcpdump curl unshare mount; do
    command -v "$tool" >/dev/null || die "required tool not on PATH: $tool"
done
# ---------------------------------------------------------------- kde ref
# RC_KDECONNECTD overrides the default /usr/bin/kdeconnectd to the
# source-built reference at tests/interop/.kde/install/bin/kdeconnectd
# (built from kdeconnect-kde tag v26.04.3, see tests/interop/.kde/SOURCE_MANIFEST.toml).
# Empty / unset = distro binary, the historical default.
# KDE_NEVRA / KDE_SOURCE_TAG / KDE_SOURCE_SHA are ALWAYS defined for the
# finish_milestone() summary line; source lane supersedes NEVRA.
KDE_NEVRA=""
KDE_SOURCE_TAG=""
KDE_SOURCE_SHA=""
if [[ -n "${RC_KDECONNECTD:-}" ]]; then
    KDECONNECTD="$RC_KDECONNECTD"
    [[ -x "$KDECONNECTD" ]] || die "RC_KDECONNECTD=$KDECONNECTD but not executable"
    # Source manifest must exist alongside the binary — it's the only
    # evidence the binary was actually built from the claimed tag. Missing
    # manifest = refuse to run rather than silently fall back.
    KDE_CACHE_DIR="$(dirname "$(dirname "$(readlink -f "$KDECONNECTD")")")"
    if [[ -f "$KDE_CACHE_DIR/../SOURCE_MANIFEST.toml" ]]; then
        KDE_SOURCE_TAG=$(grep '^source_tag' "$KDE_CACHE_DIR/../SOURCE_MANIFEST.toml" | head -1 | cut -d'"' -f2)
        KDE_SOURCE_SHA=$(grep '^source_commit' "$KDE_CACHE_DIR/../SOURCE_MANIFEST.toml" | head -1 | cut -d'"' -f2)
        KDE_NEVRA="source-build $KDE_SOURCE_TAG @ $KDE_SOURCE_SHA"
        log "KDE reference (pinned SOURCE build): tag=$KDE_SOURCE_TAG commit=$KDE_SOURCE_SHA binary=$KDECONNECTD"
    else
        die "RC_KDECONNECTD=$KDECONNECTD but no SOURCE_MANIFEST.toml found at $KDE_CACHE_DIR/../"
    fi
else
    KDECONNECTD=/usr/bin/kdeconnectd
    [[ -x "$KDECONNECTD" ]] || die "$KDECONNECTD not installed"
    KDE_NEVRA=$(rpm -q kdeconnectd kde-connect-libs kde-connect 2>/dev/null | tr '\n' ' ')
    log "KDE reference (pinned binary NEVRA, not source SHA): $KDE_NEVRA"
fi

# A pre-existing kdeconnectd is REPORTED, never killed (executor
# discipline) — per-instance bus/XDG isolation keeps this run correct
# regardless of what the host is running.
if pgrep -x kdeconnectd >/dev/null 2>&1; then
    log "NOTE: pre-existing kdeconnectd on the host (pids: $(pgrep -x kdeconnectd | tr '\n' ' ')) — left untouched"
fi

# ---------------------------------------------------------------- naming
PID=$$
NS_A="${WORK_PREFIX}-a-$PID"
NS_B="${WORK_PREFIX}-b-$PID"
VETH_A="${WORK_PREFIX}a$PID"   # IFNAMSIZ is 16 incl NUL; these stay well under
VETH_B="${WORK_PREFIX}b$PID"
SUBNET=$((20 + PID % 200))
IP_A="10.250.$SUBNET.2"
IP_B="10.250.$SUBNET.3"
DISPLAY_NUM=$((100 + PID % 800))
DISPLAY=":$DISPLAY_NUM"
WORK=$(mktemp -d "/tmp/${WORK_PREFIX}-interop.XXXXXX")
KDE_HOME="$WORK/kde"
KDE_RUNTIME="$KDE_HOME/runtime"
DBUS_ADDR="unix:path=$KDE_RUNTIME/bus"
API_KEY="${WORK_PREFIX}-interop-test-key"
KDE_NAME="${WORK_PREFIX}-kde"
RUST_NAME="${WORK_PREFIX}-rust"
PCAP="$WORK/identity-exchange.pcap"
MONITOR_LOG="$WORK/dbus-monitor.log"
RUST_LOG="$WORK/rust-daemon.log"
KDE_LOG="$WORK/kdeconnectd.log"
# Path to kde's trusted_devices file (kdeconnectconfig.cpp:55-62, INI format
# under <XDG_CONFIG_HOME>/kdeconnect/trusted_devices).
KDE_TRUSTED_DEVICES="$KDE_HOME/config/kdeconnect/trusted_devices"

KD_PID="" ; RC_PID="" ; TCPDUMP_PID="" ; MON_PID="" ; DBUS_PID="" ; XVFB_PID=""

# ------------------------------------------------------- cleanup + zero-leak
BASELINE_NETNS=$(ip netns list 2>/dev/null)
BASELINE_VETH=$(ip link show type veth 2>/dev/null)

# Kills every process whose environ OR cmdline references our work dir.
# The wrapper pids alone are NOT sufficient: `ip netns exec` may exec in
# place or fork depending on iproute2 version, and an orphaned
# kdeconnectd is exactly the stray-daemon pollution the brief bars
# (observed during development: a spike kdeconnectd outlived its wrapper
# and kept announcing via the host's avahi).
sweep_work_procs() {
    local signal="$1" pids=() pid content
    for pid in /proc/[0-9]*; do
        pid=${pid#/proc/}
        [[ "$pid" == "$$" ]] && continue
        # The brace group's stderr redirect also swallows the shell's own
        # "No such process" when a pid exits between the glob and the open.
        content=$({ tr '\0' '\n' < "/proc/$pid/environ"; tr '\0' '\n' < "/proc/$pid/cmdline"; } 2>/dev/null)
        if grep -qF "$WORK" <<< "$content"; then
            pids+=("$pid")
        fi
    done
    ((${#pids[@]})) && kill "$signal" "${pids[@]}" 2>/dev/null
    return 0
}

cleanup() {
    for pid in "$KD_PID" "$RC_PID" "$MON_PID" "$TCPDUMP_PID" "$DBUS_PID" "$XVFB_PID" "${RUST_XVFB_PID:-}" "${MPRIS_FAKE_PID:-}" "${NOTIFY_MON_PID:-}" "${NOTIF_SERVER_PID:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
    done
    sweep_work_procs TERM
    sleep 1
    for pid in "$KD_PID" "$RC_PID" "$MON_PID" "$TCPDUMP_PID" "$DBUS_PID" "$XVFB_PID" "${RUST_XVFB_PID:-}" "${MPRIS_FAKE_PID:-}" "${NOTIFY_MON_PID:-}" "${NOTIF_SERVER_PID:-}"; do
        [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null
    done
    sweep_work_procs KILL
    # Deleting a namespace removes the veth end living inside it.
    ip netns del "$NS_A" 2>/dev/null
    ip netns del "$NS_B" 2>/dev/null
    ip link del "$VETH_A" 2>/dev/null
    ip link del "$VETH_B" 2>/dev/null
}

on_exit() {
    local body_rc=$?
    cleanup
    local after_netns after_veth
    after_netns=$(ip netns list 2>/dev/null)
    after_veth=$(ip link show type veth 2>/dev/null)
    if [[ "$after_netns" == "$BASELINE_NETNS" && "$after_veth" == "$BASELINE_VETH" ]]; then
        log "ZERO-LEAK: PASS (ip netns list + ip link show type veth match pre-run baseline)"
    else
        fail "ZERO-LEAK: netns/veth state differs from baseline"
        fail "  netns after: ${after_netns:-<empty>} | veth after: ${after_veth:-<empty>}"
        body_rc=1
    fi
    log "artifacts kept at: $WORK"
    exit "$body_rc"
}
trap on_exit EXIT

# ---------------------------------------------------------------- setup
log "work dir: $WORK"
mkdir -p "$KDE_HOME"/{config,data,runtime,home} "$WORK/rust-home"
chmod 700 "$KDE_RUNTIME"

# Two namespaces joined by ONE veth pair, both ends inside a namespace.
ip netns add "$NS_A" || die "ip netns add $NS_A"
ip netns add "$NS_B" || die "ip netns add $NS_B"
ip link add "$VETH_A" type veth peer name "$VETH_B" || die "veth create"
ip link set "$VETH_A" netns "$NS_A" || die "move $VETH_A"
ip link set "$VETH_B" netns "$NS_B" || die "move $VETH_B"
ip netns exec "$NS_A" ip addr add "$IP_A/24" dev "$VETH_A" || die "addr A"
ip netns exec "$NS_A" ip link set "$VETH_A" up || die "up A"
ip netns exec "$NS_A" ip link set lo up || die "lo A"
ip netns exec "$NS_B" ip addr add "$IP_B/24" dev "$VETH_B" || die "addr B"
ip netns exec "$NS_B" ip link set "$VETH_B" up || die "up B"
ip netns exec "$NS_B" ip link set lo up || die "lo B"
# Explicit default routes: without one a send to 255.255.255.255 fails
# ENETUNREACH even with the directly-connected /24 (2.2 empirical finding).
ip netns exec "$NS_A" ip route add default via "$IP_B" || die "route A"
ip netns exec "$NS_B" ip route add default via "$IP_A" || die "route B"
log "topology: $NS_A ($IP_A) <-> $VETH_A/$VETH_B <-> $NS_B ($IP_B)"

# Per-instance Xvfb (precedent: tests/clipboard_x11.rs).
Xvfb "$DISPLAY" -screen 0 1024x768x24 >"$WORK/xvfb.log" 2>&1 &
XVFB_PID=$!
disown $! 2>/dev/null || true   # keep SIGKILL job-control noise out of the transcript
for _ in $(seq 1 50); do [[ -S "/tmp/.X11-unix/X$DISPLAY_NUM" ]] && break; sleep 0.1; done
[[ -S "/tmp/.X11-unix/X$DISPLAY_NUM" ]] || die "Xvfb socket never appeared (see $WORK/xvfb.log)"

# ---------------------------------------------------------------- mpris fake
# Plant a zbus MPRIS fake-player helper on the kde private session bus.
# Modeled on tests/mpris_bus_recovery.rs:23-80 (FakeRoot + FakePlayer).
# Used by M3 Phase 6 when RC_MPRIS_FAKE is set; otherwise the phase is a wall.
# Requires cargo build --example mpris_fake_player (run.sh builds every
# example target via `cargo build --examples --locked` below).
RC_MPRIS_FAKE="${RC_MPRIS_FAKE:-}"
MPRIS_FAKE_PID=""
MPRIS_FAKE_BIN="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}/target/debug/examples/mpris_fake_player"
start_mpris_fake() {
    [[ -n "$RC_MPRIS_FAKE" ]] || return 0
    [[ -x "$MPRIS_FAKE_BIN" ]] || die "RC_MPRIS_FAKE set but $MPRIS_FAKE_BIN not built"
    [[ -n "$MPRIS_FAKE_PID" ]] && kill -0 "$MPRIS_FAKE_PID" 2>/dev/null && return 0
    local dbus_addr="$DBUS_ADDR"
    [[ -n "${RC_DBUS_SESSION_BUS:-}" ]] && dbus_addr="$RC_DBUS_SESSION_BUS"
    env "${KDE_ENV[@]}" \
        "DBUS_SESSION_BUS_ADDRESS=$dbus_addr" \
        "MPRIS_FAKE_TITLE=m3-fake-title" \
        "MPRIS_FAKE_ARTIST=m3-fake-artist" \
        "MPRIS_FAKE_ALBUM=m3-fake-album" \
        "$MPRIS_FAKE_BIN" "m3fake" \
        >"$WORK/mpris-fake.log" 2>&1 &
    MPRIS_FAKE_PID=$!
    disown $! 2>/dev/null || true
    # Verify the player name got claimed on the bus.
    for _ in $(seq 1 50); do
        if env "${KDE_ENV[@]}" gdbus call --session --dest org.freedesktop.DBus \
            --object-path / --method org.freedesktop.DBus.NameHasOwner \
            "org.mpris.MediaPlayer2.m3fake" 2>/dev/null | grep -q true; then
            log "mpris fake-player up: org.mpris.MediaPlayer2.m3fake (pid $MPRIS_FAKE_PID)"
            return 0
        fi
        sleep 0.1
    done
    die "mpris fake-player did not claim org.mpris.MediaPlayer2.m3fake within 5s (see $WORK/mpris-fake.log)"
}
stop_mpris_fake() {
    [[ -n "$MPRIS_FAKE_PID" ]] && kill "$MPRIS_FAKE_PID" 2>/dev/null
    sleep 0.5
    [[ -n "$MPRIS_FAKE_PID" ]] && kill -9 "$MPRIS_FAKE_PID" 2>/dev/null
    MPRIS_FAKE_PID=""
}

# ---------------------------------------------------------------- rust Xvfb
# A second Xvfb INSIDE ns B so the rust daemon has a session clipboard
# backend (X11) and can read what the kde side pushes. Set by M4 Phase 3
# (kde→rust clipboard); other phases leave RC_RUST_DISPLAY empty and the
# rust daemon starts with DISPLAY=" (the historical default — degrades to
# 'no clipboard sink'). RC_RUST_DISPLAY_NUM picks the display number so
# the socket never collides with the kde-side Xvfb or with another run.
RUST_DISPLAY_NUM=""
RC_RUST_DISPLAY="${RC_RUST_DISPLAY:-}"
if [[ -n "$RC_RUST_DISPLAY" ]]; then
    RUST_DISPLAY_NUM="${RC_RUST_DISPLAY_NUM:-$((200 + PID % 400))}"
    RUST_DISPLAY=":$RUST_DISPLAY_NUM"
    ip netns exec "$NS_B" Xvfb "$RUST_DISPLAY" -screen 0 1024x768x24 \
        >"$WORK/rust-xvfb.log" 2>&1 &
    RUST_XVFB_PID=$!
    disown $! 2>/dev/null || true
    for _ in $(seq 1 50); do
        [[ -S "/tmp/.X11-unix/X$RUST_DISPLAY_NUM" ]] && break
        sleep 0.1
    done
    [[ -S "/tmp/.X11-unix/X$RUST_DISPLAY_NUM" ]] \
        || die "rust Xvfb socket never appeared (see $WORK/rust-xvfb.log)"
    log "rust-side Xvfb up on $RUST_DISPLAY (pid $RUST_XVFB_PID)"
fi

# Private session bus at an explicit filesystem path (pattern:
# tests/mpris_bus_recovery.rs — private bus, but path-addressed so it is
# reachable identically from inside and outside the netns).
#
# CRITICAL: a plain `dbus-daemon --session` loads the distro session.conf,
# whose <servicedir> entries let ANY client of our "private" bus
# auto-activate the distro org.kde.kdeconnect.service — observed in
# development: the first gdbus poll raced our isolated kdeconnectd,
# activated a NON-isolated instance (host network, host avahi,
# /root/.config identity) which won KDBusService::Unique. This bus runs
# with a minimal config with NO servicedirs, so activation is impossible
# and the only kdeconnectd that can ever own the name is ours.
cat > "$WORK/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop.org//DTD D-Bus Bus Configuration 1.0//EN"
  "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>$DBUS_ADDR</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
DBUS_OUT=$(dbus-daemon --config-file="$WORK/bus.conf" --fork --print-address=1 --print-pid=1) \
    || die "dbus-daemon failed"
DBUS_ADDR=$(printf '%s\n' "$DBUS_OUT" | sed -n 1p)
DBUS_PID=$(printf '%s\n' "$DBUS_OUT" | sed -n 2p)
log "private session bus (activation disabled): $DBUS_ADDR (pid $DBUS_PID)"

# Every kde-side child gets this EXACT env (see header).
KDE_ENV=(
    "DISPLAY=$DISPLAY"
    "DBUS_SESSION_BUS_ADDRESS=$DBUS_ADDR"
    "XDG_CONFIG_HOME=$KDE_HOME/config"
    "XDG_DATA_HOME=$KDE_HOME/data"
    "XDG_RUNTIME_DIR=$KDE_RUNTIME"
    "HOME=$KDE_HOME/home"
    "QT_QPA_PLATFORM=xcb"
    "XDG_SESSION_TYPE=x11"
    "QT_LOGGING_RULES=kdeconnect.*.debug=true"
    "QT_MESSAGE_PATTERN=%{time h:mm:ss.zzz} %{category}: %{message}"
    # Fedora's Qt build logs to journald, not stderr — force stderr so the
    # captured log is the actual evidence artifact.
    "QT_FORCE_STDERR_LOGGING=1"
)

# D-Bus signal oracle, started BEFORE either daemon so no deviceAdded can
# be missed (brief: oracles are D-Bus signals, not pcaps).
env "${KDE_ENV[@]}" gdbus monitor --session --dest org.kde.kdeconnect \
    >"$MONITOR_LOG" 2>&1 &
MON_PID=$!
disown $! 2>/dev/null || true   # keep SIGKILL job-control noise out of the transcript

# Wire capture INSIDE ns A: plaintext UDP-1716 identity exchange plus any
# mDNS (5353) traffic, so the discovery-channel question is answered from
# evidence, not assumption.
ip netns exec "$NS_A" tcpdump -U -i "$VETH_A" -w "$PCAP" \
    "udp port 1716 or udp port 5353" >"$WORK/tcpdump.log" 2>&1 &
TCPDUMP_PID=$!
disown $! 2>/dev/null || true   # keep SIGKILL job-control noise out of the transcript
for _ in $(seq 1 40); do
    grep -q "listening on" "$WORK/tcpdump.log" 2>/dev/null && break
    sleep 0.25
done
grep -q "listening on" "$WORK/tcpdump.log" || die "tcpdump never started (see $WORK/tcpdump.log)"

# ---------------------------------------------------------------- helpers
# gdbus wrapper — defaults to the daemon object's methods; pass
# /modules/kdeconnect/devices/<id> as the object path for the device
# interface (device.h:55-61).
kde_dbus() {
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path /modules/kdeconnect --method "$@" 2>/dev/null
}

# Same as kde_dbus but allows the caller to specify the object path (the
# device iface lives at /modules/kdeconnect/devices/<id>).
kde_dbus_device() {
    local device_id="$1"; shift
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id" --method "$@" 2>/dev/null
}

# Poll helper: wait_for <seconds> <description> <cmd...>
wait_for() {
    local timeout="$1" desc="$2"; shift 2
    local deadline=$((SECONDS + timeout))
    while (( SECONDS < deadline )); do
        if "$@"; then return 0; fi
        sleep 1
    done
    fail "timed out after ${timeout}s waiting for: $desc"
    return 1
}

rc_api() {
    ip netns exec "$NS_B" curl -sf -H "X-API-Key: $API_KEY" \
        "http://127.0.0.1:9090$1" 2>/dev/null
}

# Same as rc_api but POSTs — empty body; the pair endpoint accepts empty.
rc_api_post() {
    local path="$1"
    ip netns exec "$NS_B" curl -sf -X POST -H "X-API-Key: $API_KEY" \
        "http://127.0.0.1:9090$path" 2>/dev/null
}

# rc_api + grep in one helper so wait_for callers don't need bash -c
# (which can't see lib.sh functions because it starts a fresh shell).
# Returns 0 if the substring appears in the GET response body.
rc_api_grep() {
    local path="$1" needle="$2"
    rc_api "$path" 2>/dev/null | grep -qF "$needle"
}

FAILURES=()
check() { # check <assertion-name> <ok=0/1> <detail>
    if [[ "$2" == "0" ]]; then
        log "ASSERT PASS: $1"
    else
        fail "ASSERT FAIL: $1 — $3"
        FAILURES+=("$1")
    fi
}

# Discovery polling: best-effort, used by both m1 and m2.
#   kde_discovered <name>  ->  rc=0 when the kde side has a deviceIdByName hit
#   rust_discovered <id>   ->  rc=0 when the rust side has the id in /devices
kde_found_name() {
    kde_dbus org.kde.kdeconnect.daemon.deviceIdByName "$1" \
        | grep -qE "\('[0-9A-Za-z_-]+',\)"
}

rust_found_id() {
    rc_api /api/v1/devices | grep -q "$1"
}

# D-Bus device iface: pairStateAsInt returns 0..3 per core/pairstate.h:10-15
# (NotPaired=0, Requested=1, RequestedByPeer=2, Paired=3). The device object
# path is /modules/kdeconnect/devices/<id> (device.h:55-61) and the
# interface is `org.kde.kdeconnect.device` (device.h:25 — lowercase, the
# QT convention — not `Device`).
kde_pair_state_as_int() {
    local device_id="$1"
    local out
    out=$(env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id" \
        --method org.kde.kdeconnect.device.pairStateAsInt 2>/dev/null)
    # Typical output: (3,)  — strip the ( , ) wrapper.
    printf '%s' "$out" | tr -d "()', " | head -c 4
}

# REST pair_state field as reported by /api/v1/devices/:id (paired | unpaired | …).
rust_pair_state() {
    local device_id="$1"
    rc_api "/api/v1/devices/$device_id" | grep -oE '"pair_state":"[^"]*"' | head -1 | cut -d'"' -f4
}

# Number of trusted_devices entries on the kde side by counting INI
# sections (each section is "[<id>]"). Returns 0 on a missing file.
# Returns a single integer line — grep -c prints "0" with exit 1 on
# 0 matches, and the previous `|| echo 0` appended a second "0" on top
# of that, which broke `[ -gt ]` integer comparisons. Capture the
# count and default to 0 instead.
kde_trusted_count() {
    if [[ ! -f "$KDE_TRUSTED_DEVICES" ]]; then
        echo 0
        return
    fi
    local n
    n=$(grep -cE '^\[[^]]+\]$' "$KDE_TRUSTED_DEVICES" 2>/dev/null || true)
    echo "${n:-0}"
}

# forceOnNetworkChange — kde's daemon method (daemon.h:55) re-scans for
# peers. Used by m2's reconnect phase to re-establish announcements after
# a veth flap.
kde_force_on_network_change() {
    kde_dbus org.kde.kdeconnect.daemon.forceOnNetworkChange >/dev/null 2>&1
}

# ---------------------------------------------------------------- pair (M2)
# KDE device iface methods (device.h:113-127). All operate on the device
# object at /modules/kdeconnect/devices/<id>. The id is the kde-normalized
# form (dashes→underscores, networkpacket.cpp:82-87) — same form KDE's
# deviceIdByName returns.
kde_request_pairing() {
    local device_id="$1"
    kde_dbus_device "$device_id" org.kde.kdeconnect.device.requestPairing
}

# Wait for the kde device object to be FULLY present at
# /modules/kdeconnect/devices/<id> AND surface a real pair state (not the
# empty `()` that gdbus returns when the object is mid-destruction).
#
# Why this exists: after the rust side's "Same-cert redial" (closes the
# first TLS link and adopts a fresh one within a second), kdeconnectd
# destroys the device object on the old link and re-creates it on the
# new one. The window between deviceRemoved and deviceAdded is short
# (tens of ms) but non-zero — and during it, kde_request_pairing's
# gdbus call races the destroy and fails ("requestPairing call failed"
# because the object path returns nothing callable). Polling on
# pairStateAsInt alone is NOT enough: when the object is gone, it
# returns the empty `()` string, which is distinct from `0` (NotPaired).
# We wait for a NUMERIC state, not the empty pre-add state.
kde_device_ready_for_pairing() {
    local device_id="$1"
    local ps
    ps=$(kde_pair_state_as_int "$device_id")
    # Numeric state means the device object exists and pairStateAsInt
    # resolved. Empty (`()`) means the object is gone — the same-cert
    # redial cycle, or any other in-flight destroy.
    [[ "$ps" =~ ^[0-9]+$ ]]
}
kde_not_paired_for_pairing() {
    local device_id="$1"
    local ps
    ps=$(kde_pair_state_as_int "$device_id")
    [[ "$ps" == "0" ]]
}
kde_accept_pairing() {
    local device_id="$1"
    kde_dbus_device "$device_id" org.kde.kdeconnect.device.acceptPairing
}
kde_unpair() {
    local device_id="$1"
    kde_dbus_device "$device_id" org.kde.kdeconnect.device.unpair
}
# Rust pairing via REST. POST /api/v1/devices/<id>/pair (router.rs:64) —
# the handler dispatches based on has_incoming_request (handles/device.rs:160).
# The same endpoint is the harness's accept path for an incoming pair
# request and the rust's initiate path when no request is pending.
rust_pair() {
    local device_id="$1"
    rc_api_post "/api/v1/devices/$device_id/pair"
}
rust_unpair() {
    local device_id="$1"
    ip netns exec "$NS_B" curl -sf -X DELETE -H "X-API-Key: $API_KEY" \
        "http://127.0.0.1:9090/api/v1/devices/$device_id/unpair" 2>/dev/null
}

# Send a ping packet to a device via REST. Used by Phase 3 to provoke
# the rust side to actually use (and surface) a dead TCP socket left
# dangling by a veth flap. The kernel only reports ECONNRESET on a
# write to a dead socket when something WRITES — an idle connection
# stays "alive" in user-space until the next user-level I/O. So flap
# alone won't surface the disconnect; a ping after the flap will.
rust_ping() {
    local device_id="$1"
    ip netns exec "$NS_B" curl -s -X POST -H "X-API-Key: $API_KEY" \
        -H "Content-Type: application/json" \
        -d "{\"device_id\":\"$device_id\"}" \
        "http://127.0.0.1:9090/api/v1/ping" >/dev/null 2>&1
    return 0
}

# Polling helpers: return 0 when the corresponding side is Paired.
kde_is_paired() {
    [[ "$(kde_pair_state_as_int "$1")" == "3" ]]
}
rust_is_paired() {
    local ps
    ps=$(rust_pair_state "$1")
    [[ "$ps" == "paired" ]]
}

# Wait for both sides to be Paired within the timeout.
wait_for_paired_both() {
    local rust_id="$1" kde_id="$2" timeout="${3:-40}"
    wait_for "$timeout" "kde pairStateAsInt=3 for $rust_id" kde_is_paired "$rust_id"
    wait_for "$timeout" "rust pair_state=paired for $kde_id" rust_is_paired "$kde_id"
}

# Wait for the kde side to be NotPaired (state 0) — for the unpair dance
# between phases. Treats the device-object-gone case as unpaired: when a
# Paired device is unpaired while unreachable, kdeconnectd's Device
# destructor drops the D-Bus object (device.cpp:113-118 +
# Daemon::removeDevice — once isReachable=false && !isPaired(), the
# device is removed from the in-memory map). The object path
# /modules/kdeconnect/devices/<id> then returns from gdbus as `()` —
# empty. That is the same observable state as NotPaired for our test.
kde_is_unpaired() {
    local ps
    ps=$(kde_pair_state_as_int "$1")
    [[ -z "$ps" || "$ps" == "0" ]]
}
rust_is_unpaired() {
    local ps
    ps=$(rust_pair_state "$1")
    # The rust API surfaces PairState::NotPaired as the underscore form
    # (src/protocol/pairing/mod.rs:as_api_str) — the human form "unpaired"
    # is not what the wire emits. Compare against the wire form.
    [[ "$ps" == "not_paired" ]]
}

# Wait for the rust side to surface an incoming pair request. The handler
# reflects this via verification_key (handles/device.rs:116-118; the SAS
# is upper-case hex delivered on the GET /api/v1/devices/<id> response).
# Exposed as the field that the harness CALLS accept on — without this
# the accept REST call would take the initiate branch and the round would
# race against itself.
rust_incoming_pair_request() {
    rc_api "/api/v1/devices/$1" \
        | grep -qE '"verification_key":"[0-9A-F]{8}"'
}

# ---------------------------------------------------------------- start_kde
# Starts the kdeconnectd inside ns A under the per-instance env. Honors
# RC_SABOTAGE=skip-kde by leaving KDE_PID empty and stuffing a sentinel
# id that can never collide with the rust registry; the assertion polls
# still run for real (so a red proof exercises the actual poll instead of
# short-circuiting).
start_kde() {
    if [[ "$RC_SABOTAGE" == "skip-kde" ]]; then
        log "SABOTAGE=skip-kde: kdeconnectd NOT started (red-proof mode only)"
        # Sentinel that can never appear in the rust registry, so the rust-side
        # poll below still runs for real (and times out) instead of being
        # short-circuited — a red proof must exercise the actual poll.
        KDE_ID="redproof000000000000000000000000"
        return 0
    fi

    # Mask the system bus (host avahi!) inside a private mount namespace —
    # see header. Without this kdeconnectd picks AvahiDiscovery and
    # announces the test instance onto the real LAN via host avahi.
    ip netns exec "$NS_A" unshare --mount --propagation private \
        bash -c 'mount --bind /dev/null /run/dbus/system_bus_socket || exit 1; exec env "$@"' _ \
        "${KDE_ENV[@]}" "$KDECONNECTD" >"$WORK/kde.stdout" 2>"$KDE_LOG" &
    KD_PID=$!
    disown $! 2>/dev/null || true   # keep SIGKILL job-control noise out of the transcript
    log "kdeconnectd started (pid $KD_PID) in $NS_A"

    _kde_bus_up() { kde_dbus org.kde.kdeconnect.daemon.selfId >/dev/null; }
    wait_for 30 "org.kde.kdeconnect on the private bus" _kde_bus_up \
        || die "kdeconnectd never claimed org.kde.kdeconnect (see $KDE_LOG)"
    kill -0 "$KD_PID" 2>/dev/null || die "kdeconnectd exited (see $KDE_LOG)"

    KDE_ID=$(kde_dbus org.kde.kdeconnect.daemon.selfId | tr -d "()', ")
    [[ -n "$KDE_ID" ]] || die "could not read kde selfId"
    # Anti-phantom proof: the org.kde.kdeconnect owner must be OUR
    # isolated instance (its environ carries our XDG_CONFIG_HOME), and the
    # identity must have materialized under the isolated config dir — not
    # an auto-activated distro instance (see the bus.conf comment above).
    OWNER_PID=$(env "${KDE_ENV[@]}" gdbus call --session --dest org.freedesktop.DBus \
        --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.GetConnectionUnixProcessID org.kde.kdeconnect 2>/dev/null \
        | sed -n 's/.*uint32 \([0-9][0-9]*\).*/\1/p' | head -1)
    [[ -n "$OWNER_PID" ]] || die "could not resolve org.kde.kdeconnect owner pid"
    grep -qzF "XDG_CONFIG_HOME=$KDE_HOME/config" "/proc/$OWNER_PID/environ" \
        || die "org.kde.kdeconnect owner (pid $OWNER_PID) is NOT our isolated instance"
    [[ -f "$KDE_HOME/config/kdeconnect/privateKey.pem" ]] \
        || die "isolated identity never materialized under $KDE_HOME/config"
    log "kde side up: selfId=$KDE_ID (owner pid $OWNER_PID, isolation verified)"

    # Distinctive announced name so wire + registry evidence is attributable.
    kde_dbus org.kde.kdeconnect.daemon.setAnnouncedName "$KDE_NAME" >/dev/null \
        || die "setAnnouncedName failed"
    log "kde announcedName set to $KDE_NAME"
}

# ---------------------------------------------------------------- start_rust
start_rust() {
    if [[ "$RC_SABOTAGE" == "skip-rust" ]]; then
        log "SABOTAGE=skip-rust: rust-connect NOT started (red-proof mode only)"
        return 0
    fi

    cat > "$WORK/rust.toml" <<EOF
device_name = "$RUST_NAME"
tcp_port = 1716
udp_port = 1716
data_dir = "$WORK/rust-data"
cert_dir = "$WORK/rust-data/certs"
log_level = "debug"
api_enabled = true
api_port = 9090
api_bind = "127.0.0.1"
api_keys = ["$API_KEY"]
idle_timeout_secs = 0
ui_enabled = false
EOF
    # DBUS_SESSION_BUS_ADDRESS is explicit for this child: a bogus path by
    # default so the daemon's D-Bus session backends (mpris, notifications,
    # …) degrade instantly instead of touching any real bus. M3 overrides
    # via RC_DBUS_SESSION_BUS (e.g. to point at $DBUS_ADDR — the kde private
    # bus — so plugins that read the session bus can find it).
    local dbus_addr="unix:path=$WORK/rust-home/no-such-bus"
    [[ -n "${RC_DBUS_SESSION_BUS:-}" ]] && dbus_addr="$RC_DBUS_SESSION_BUS"
    # M4: when a rust-side Xvfb was started (RC_RUST_DISPLAY non-empty),
    # expose it so the clipboard plugin uses xclip/xsel instead of degrading
    # to 'no clipboard sink'. Otherwise leave DISPLAY empty.
    local rust_display=""
    [[ -n "${RC_RUST_DISPLAY:-}" && -n "${RUST_DISPLAY:-}" ]] && rust_display="$RUST_DISPLAY"
    ip netns exec "$NS_B" env \
        "HOME=$WORK/rust-home" \
        "DBUS_SESSION_BUS_ADDRESS=$dbus_addr" \
        "DISPLAY=$rust_display" \
        "$RC_BIN" --config "$WORK/rust.toml" \
        >"$RUST_LOG" 2>&1 &
    RC_PID=$!
    disown $! 2>/dev/null || true   # keep SIGKILL job-control noise out of the transcript
    log "rust-connect started (pid $RC_PID) in $NS_B (DISPLAY=${rust_display:-<empty>})"

    wait_for 30 "rust REST API on 127.0.0.1:9090 (inside $NS_B)" \
        bash -c "ip netns exec \"$NS_B\" curl -sf -H 'X-API-Key: $API_KEY' http://127.0.0.1:9090/api/v1/devices >/dev/null 2>&1" \
        || die "rust REST API never came up (see $RUST_LOG)"
    kill -0 "$RC_PID" 2>/dev/null || die "rust-connect exited (see $RUST_LOG)"
    log "rust side up: REST API answering"
}

# ---------------------------------------------------------------- restart (M2)
# Stop a daemon without tearing down the topology. Used by M2's restart
# persistence test — the daemon must reload its identity + trust store
# from disk on a fresh start, and the pair state must survive.
stop_kde() {
    [[ -n "$KD_PID" ]] && kill "$KD_PID" 2>/dev/null
    sleep 1
    [[ -n "$KD_PID" ]] && kill -9 "$KD_PID" 2>/dev/null
    # Deliberately NOT sweeping here: sweep_work_procs matches on the
    # $WORK prefix, which is shared by the bus.conf path — sweeping would
    # also kill the private dbus-daemon (whose --config-file cmdline
    # references $WORK/bus.conf), and the post-restart kdeconnectd would
    # then fail with "DBus session bus not found". The full sweep runs
    # from cleanup() in on_exit, where it belongs.
    KD_PID=""
}
stop_rust() {
    [[ -n "$RC_PID" ]] && kill "$RC_PID" 2>/dev/null
    sleep 1
    [[ -n "$RC_PID" ]] && kill -9 "$RC_PID" 2>/dev/null
    # Same reasoning as stop_kde — see the comment there. Sweep lives in
    # cleanup(); per-stop sweep risks killing peers we still need.
    RC_PID=""
}

# Restart kdeconnectd in the same namespace, same XDG dirs. Asserts the
# restored selfId matches the one before the restart — otherwise the
# identity didn't persist and the rest of the test is meaningless.
restart_kde() {
    stop_kde
    # Append (not truncate) so we can see both the original
    # kdeconnectd.log AND the post-restart log in the artifact. A
    # hard failure to start is still diagnosable from the appended
    # segment alone.
    ip netns exec "$NS_A" unshare --mount --propagation private \
        bash -c 'mount --bind /dev/null /run/dbus/system_bus_socket || exit 1; exec env "$@"' _ \
        "${KDE_ENV[@]}" "$KDECONNECTD" >>"$WORK/kde.stdout" 2>>"$KDE_LOG" &
    KD_PID=$!
    disown $! 2>/dev/null || true
    log "kdeconnectd restarted (pid $KD_PID) in $NS_A"

    _kde_bus_up2() { kde_dbus org.kde.kdeconnect.daemon.selfId >/dev/null; }
    wait_for 30 "org.kde.kdeconnect (post-restart)" _kde_bus_up2 \
        || die "kdeconnectd never claimed org.kde.kdeconnect after restart (see $KDE_LOG)"
    kill -0 "$KD_PID" 2>/dev/null || die "kdeconnectd exited after restart (see $KDE_LOG)"

    local kde_id_after
    kde_id_after=$(kde_dbus org.kde.kdeconnect.daemon.selfId | tr -d "()', ")
    if [[ "$kde_id_after" != "$KDE_ID" ]]; then
        die "kde selfId changed after restart: was $KDE_ID, now $kde_id_after (identity did not persist)"
    fi
    log "kde restarted, identity preserved: selfId=$KDE_ID"
}

# Restart rust-connect with the same data dir. The cert is reloaded from
# $WORK/rust-data/certs; the pairing state is reloaded from the same
# data dir. Readiness oracle is the REST API responding.
restart_rust() {
    stop_rust
    local dbus_addr="unix:path=$WORK/rust-home/no-such-bus"
    [[ -n "${RC_DBUS_SESSION_BUS:-}" ]] && dbus_addr="$RC_DBUS_SESSION_BUS"
    local rust_display=""
    [[ -n "${RC_RUST_DISPLAY:-}" && -n "${RUST_DISPLAY:-}" ]] && rust_display="$RUST_DISPLAY"
    ip netns exec "$NS_B" env \
        "HOME=$WORK/rust-home" \
        "DBUS_SESSION_BUS_ADDRESS=$dbus_addr" \
        "DISPLAY=$rust_display" \
        "$RC_BIN" --config "$WORK/rust.toml" \
        >"$RUST_LOG" 2>&1 &
    RC_PID=$!
    disown $! 2>/dev/null || true
    log "rust-connect restarted (pid $RC_PID) in $NS_B"

    wait_for 30 "rust REST API (post-restart)" \
        bash -c "ip netns exec \"$NS_B\" curl -sf -H 'X-API-Key: $API_KEY' http://127.0.0.1:9090/api/v1/devices >/dev/null 2>&1" \
        || die "rust REST API never came up after restart (see $RUST_LOG)"
    kill -0 "$RC_PID" 2>/dev/null || die "rust-connect exited after restart (see $RUST_LOG)"
    log "rust restarted, REST API answering"
}

# ---------------------------------------------------------------- nudge
# Both implementations broadcast ONCE at start (lanlinkprovider.cpp:149 /
# service_manager startup broadcast) and kdeconnectd re-broadcasts only on
# network change — so order matters. kde started first; rust's startup
# broadcast already carried rust->kde. forceOnNetworkChange makes kde
# re-broadcast now that rust is listening, carrying kde->rust over UDP.
# mDNS runs concurrently the whole time; which channel actually carried
# each direction is determined from evidence, not assumed.
nudge_kde_for_discovery() {
    if [[ -n "$KD_PID" && -n "$RC_PID" ]]; then
        kde_dbus org.kde.kdeconnect.daemon.forceOnNetworkChange >/dev/null \
            && log "forceOnNetworkChange issued (kde re-broadcast)"
    fi
}

# ---------------------------------------------------------------- wait
# Wait for mutual discovery. Outputs RUST_ID. Returns 0 if both sides
# resolve, 1 if either side fails.
wait_for_mutual_discovery() {
    local timeout="${1:-60}"
    RUST_ID=""
    KDE_SEES_RUST=1
    RUST_SEES_KDE=1

    if [[ -n "$KD_PID" ]]; then
        if wait_for "$timeout" "kde side to discover $RUST_NAME (D-Bus deviceIdByName)" kde_found_name "$RUST_NAME"; then
            RUST_ID=$(kde_dbus org.kde.kdeconnect.daemon.deviceIdByName "$RUST_NAME" | tr -d "()', ")
            KDE_SEES_RUST=0
            log "kde discovered rust: deviceId=$RUST_ID"
        fi
    fi

    if [[ -n "$RC_PID" ]]; then
        if wait_for "$timeout" "rust side to discover kde $KDE_ID (REST /api/v1/devices)" \
            bash -c "ip netns exec \"$NS_B\" curl -sf -H 'X-API-Key: $API_KEY' http://127.0.0.1:9090/api/v1/devices 2>/dev/null | grep -q '$KDE_ID'"; then
            RUST_SEES_KDE=0
            log "rust discovered kde: $KDE_ID present in /api/v1/devices"
        fi
    fi
}

# ---------------------------------------------------------------- finish
# Final teardown helper: flush tcpdump, log the NEVRA, and write the
# milestone verdict. The EXIT trap still runs after this.
finish_milestone() {
    local label="$1"
    log "KDE reference NEVRA: $KDE_NEVRA"
    if (( ${#FAILURES[@]} == 0 )); then
        log "${label}: PASS"
        return 0
    fi
    fail "${label}: FAIL — ${#FAILURES[@]} assertion(s) failed: ${FAILURES[*]}"
    return 1
}

# ---------------------------------------------------------------- M3 surfaces
# Per-plugin flow helpers for M3 (vk #991, M3 of 4). All assume a paired
# topology; none of these call kdeconnectd's setup. M3-specific saboteages
# (skip-share-send, skip-clipboard-rx, etc.) are interpreted by m3_smoke.sh
# itself; the generic skip-rust / skip-kde saboteages are honored by
# start_kde/start_rust.
#
# All citations upstream point to /tmp/kdeconnect-kde (pinned at dcd6ded4)
# and to rust-connect's plugin + handler code. Each helper has its citations
# next to the call shape.
#
# Surfaces mapped here (paths, interfaces, method names):
#   share.shareUrls(QStringList)   — org.kde.kdeconnect.device.share
#                                    /modules/kdeconnect/devices/<id>/share
#                                    (shareplugin.cpp:276-281,290-293)
#   share.shareUrl(QString)        — same path (shareplugin.cpp:251-274)
#   share.shareText(QString)       — same path (shareplugin.cpp:283-288)
#   clipboard.sendClipboard()      — org.kde.kdeconnect.device.clipboard
#                                    /modules/kdeconnect/devices/<id>/clipboard
#                                    (clipboardplugin.h:65-66)
#   remotecommands.triggerCommand(QString) — org.kde.kdeconnect.device.remotecommands
#                                    /modules/kdeconnect/devices/<id>/remotecommands
#                                    (remotecommandsplugin.cpp:54-58)
#   remotesystemvolume.sendVolume(QString, int) — same iface (volumes.h:35)
#   remotesystemvolume.volumeChanged(QString, int) — SIGNAL (volumes.h:40)

# Allow start_rust to use a real session bus for the plugins that need one
# (sendnotifications, mpris, clipboard). Default empty → bogus no-such-bus
# path. M3 sets this to $DBUS_ADDR so the rust daemon speaks the kde private
# session bus for plugins that read it. The kde side has BecomeMonitor match
# rules on org.freedesktop.Notifications.Notify and the rust side has its
# own (src/plugins/sendnotifications.rs) — multiple monitors on the same
# bus are fine, and the echo-prevention hint (x-kdeconnect-source-device)
# is already wired both ways.
RC_DBUS_SESSION_BUS="${RC_DBUS_SESSION_BUS:-}"

# kde→rust share: invokes share.shareUrls on the kde device path with a
# single-element QStringList. The kdeconnectd side builds a
# kdeconnect.share.request packet (shareplugin.cpp:251-274) with payload =
# the file bytes. Receiver is rust-connect's SharePlugin, which writes
# the file under dirs::download_dir() — i.e. $RUST_HOME/Downloads by
# default (src/plugins/share.rs).
kde_share_urls() {
    local device_id="$1"; shift
    local url="$1"
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id/share" \
        --method org.kde.kdeconnect.device.share.shareUrls \
        "['$url']" 2>&1
}

# kde→rust share via the single-URL variant (shareplugin.cpp:251). Useful
# when the harness wants one share.shareUrl call rather than the list form.
kde_share_url() {
    local device_id="$1"; shift
    local url="$1"
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id/share" \
        --method org.kde.kdeconnect.device.share.shareUrl \
        "'$url'" 2>&1
}

# kde→rust share text (shareplugin.cpp:283). Used to test the no-payload
# text branch — the kde side posts a KNotification and writes to
# KSystemClipboard; the rust side receives a kdeconnect.share.request with
# {text: ...}. Both share the same packet type; the text branch is a
# distinct assertion.
kde_share_text() {
    local device_id="$1"; shift
    local text="$1"
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id/share" \
        --method org.kde.kdeconnect.device.share.shareText \
        "'$text'" 2>&1
}

# Read the kde Xvfb's X11 clipboard. KSystemClipboard is what kdeconnectd's
# clipboard plugin writes to on the receive side, so xclip -o inside the
# kde per-instance env is the independent oracle for a rust→kde clipboard
# flow. Returns the raw text (no trailing newline). Times out hard so a
# hung X server can't stall the smoke.
kdeclip_get_text() {
    env "${KDE_ENV[@]}" timeout 5 xclip -o -selection clipboard 2>/dev/null
}

# Write text to the kde Xvfb's X11 clipboard. Used by the kde→rust phase
# to set the source, then sleep long enough for kdeconnectd's clipboard
# watcher to fire on the selection change. Per M3 brief: "if the rust
# side has no X integration, that direction may be a recorded wall."
kdeclip_set_text() {
    local text="$1"
    env "${KDE_ENV[@]}" timeout 5 bash -c "printf '%s' \"\$1\" | xclip -selection clipboard" _ "$text"
}

# Send the kdeconnect.clipboard packet via kde's clipboard plugin
# (clipboardplugin.h:65). The plugin reads its own KSystemClipboard and
# sends the packet — so this is a no-payload "request push" trigger that
# surfaces whatever is currently on the KDE clipboard.
kde_clipboard_send() {
    local device_id="$1"
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id/clipboard" \
        --method org.kde.kdeconnect.device.clipboard.sendClipboard \
        2>&1
}

# Trigger a configured kdeconnect.runcommand on the paired rust device.
# This is the kde→rust direction; the actual command runs on the rust
# side. Per vk #1007 the rust production allowlist is empty, so this
# is recorded as a wall with policy text — see m3_smoke.sh Phase 8.
kde_trigger_command() {
    local device_id="$1"; shift
    local key="$1"
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id/remotecommands" \
        --method org.kde.kdeconnect.device.remotecommands.triggerCommand \
        "'$key'" 2>&1
}

# Enable a plugin on a kde device. The kdeconnectd per-device config
# (core/device.cpp:448-460) holds `<pluginName>Enabled=true|false`; the
# D-Bus method writes it and calls reloadPlugins(). Plugins that ship
# with `EnabledByDefault: false` — e.g. kdeconnect_sendnotifications
# (kdeconnect_sendnotifications.json:line) — MUST be enabled this way
# before any phase that depends on their listener is meaningful.
# Without this, the BecomeMonitor on org.freedesktop.Notifications.Notify
# never registers and kde Notify calls reach the bus unmonitored.
kde_enable_plugin() {
    local device_id="$1"; shift
    local plugin_name="$1"
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path "/modules/kdeconnect/devices/$device_id" \
        --method org.kde.kdeconnect.device.setPluginEnabled \
        "'$plugin_name'" true 2>&1
}

# Send a desktop notification on the kde private session bus. This is
# the driver for the kde→rust sendnotifications phase: the kdeconnectd's
# BecomeMonitor listener on org.freedesktop.Notifications.Notify picks
# up the call, filters x-kdeconnect-source-device (none here), builds
# a kdeconnect.notification packet (dbusnotificationslistener.cpp:317-329),
# and forwards to the paired rust device. Signature per the spec
# (susssasa{sv}i) = (app, replaces_id, icon, summary, body, actions,
# hints, timeout).
kde_notify_send() {
    local app="$1" icon="$2" summary="$3" body="$4"
    env "${KDE_ENV[@]}" gdbus call --session --dest org.freedesktop.Notifications \
        --object-path /org/freedesktop/Notifications \
        --method org.freedesktop.Notifications.Notify \
        "'$app'" 0 "'$icon'" "'$summary'" "'$body'" "[]" "{}" 5000 2>&1
}

# Drive a PulseAudio sink volume change via pactl. Requires pactl on the
# host + a running PA/pipewire-pulse socket the harness can reach. M3's
# remotesystemvolume-out spike needs this; the cheap-batch phase records
# the wall if pactl is unavailable inside the netns.
pactl_set_sink_volume() {
    local sink="$1" volume="$2"
    pactl set-sink-volume "$sink" "$volume" 2>&1
}

# Start a dedicated gdbus monitor targeting org.freedesktop.Notifications
# (NOT the default org.kde.kdeconnect monitor from line 254, which is
# kdeconnect-only). Captures the Notify call shape and the app/summary/
# body/hints so the rust→kde notifications phase has an oracle that
# observes the kde side posting the notification it received from rust.
NOTIFY_MONITOR_LOG="$WORK/notify-monitor.log"
start_notify_monitor() {
    # NOTE: gdbus monitor only catches SIGNALS from the destination object,
    # not method calls addressed to it. Phase 5 (rust→kde notifications)
    # needs to observe Notify() method calls, so we use dbus-monitor with a
    # type=method_call filter on the Notifications interface. The kde
    # sendnotifications plugin's BecomeMonitor also sees these (it's
    # pre-dispatch), which is why kdeconnectd's log shows "Got notification
    # from KDE Connect" while gdbus monitor captured nothing.
    env "${KDE_ENV[@]}" dbus-monitor --session \
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'" \
        >"$NOTIFY_MONITOR_LOG" 2>&1 &
    NOTIFY_MON_PID=$!
    disown $! 2>/dev/null || true
    log "notify monitor started (pid $NOTIFY_MON_PID) at $NOTIFY_MONITOR_LOG"
}

# Stop the notify monitor and reap.
stop_notify_monitor() {
    [[ -n "${NOTIFY_MON_PID:-}" ]] && kill "$NOTIFY_MON_PID" 2>/dev/null
    sleep 1
    [[ -n "${NOTIFY_MON_PID:-}" ]] && kill -9 "$NOTIFY_MON_PID" 2>/dev/null
    NOTIFY_MON_PID=""
}

# Spin up the tiny org.freedesktop.Notifications stub on the kde private
# session bus. Required for Phase 5 (rust→kde notifications) because
# KNotification calls Notify through the KDE framework's standard path
# — which fails on a bus with no notification server. Without this, the
# monitor captures nothing (kf.notifications: Failed to notify ... in
# the kdeconnectd log). Phase 4 (kde→rust sendnotifications) does NOT
# need it — BecomeMonitor sees messages before destination dispatch.
# Starts once, kept alive for the smoke lifetime; cleanup() in lib.sh
# kills it.
NOTIF_SERVER_PID=""
start_notif_server() {
    [[ -n "$NOTIF_SERVER_PID" ]] && kill -0 "$NOTIF_SERVER_PID" 2>/dev/null && return 0
    python3 "$(dirname "${BASH_SOURCE[0]}")/lib/notif_server.py" "$DBUS_ADDR" \
        >"$WORK/notif-server.log" 2>&1 &
    NOTIF_SERVER_PID=$!
    disown $! 2>/dev/null || true
    # Give it a moment to claim the name before any phase issues a
    # Notify. name_has_owner is the right check.
    for _ in $(seq 1 20); do
        if env "${KDE_ENV[@]}" gdbus call --session --dest org.freedesktop.DBus \
            --object-path / --method org.freedesktop.DBus.NameHasOwner \
            "org.freedesktop.Notifications" 2>/dev/null | grep -q true; then
            log "notif_server.py started (pid $NOTIF_SERVER_PID) — org.freedesktop.Notifications claimed"
            return 0
        fi
        sleep 0.1
    done
    log "WARN: notif_server.py failed to claim org.freedesktop.Notifications within 2s"
    return 1
}

stop_notif_server() {
    [[ -n "${NOTIF_SERVER_PID:-}" ]] && kill "$NOTIF_SERVER_PID" 2>/dev/null
    sleep 0.5
    [[ -n "${NOTIF_SERVER_PID:-}" ]] && kill -9 "$NOTIF_SERVER_PID" 2>/dev/null
    NOTIF_SERVER_PID=""
}

# Capture log offset BEFORE the trigger, so subsequent greps land only on
# new lines. Same lesson as M2's PHASE2_LOG_OFFSET — the trigger→oracle
# round trip completes in <10ms, and capturing AFTER the trigger misses
# the signal entirely.
notify_log_offset() {
    wc -l < "$NOTIFY_MONITOR_LOG" 2>/dev/null || echo 0
}

# REST GET on the rust side, body-bearing POST helper. Same shape as
# rc_api/rc_api_post (lib.sh:312-318) but with body support for the M3
# phases whose handlers want JSON (clipboard.set, notification.send, mpris).
# M3 handlers route through these — not through the existing rc_api_* —
# because rc_api_post hardcodes an empty body.
rc_api_post_body() {
    local path="$1" body="${2:-}"
    ip netns exec "$NS_B" curl -sf -X POST -H "X-API-Key: $API_KEY" \
        -H "Content-Type: application/json" \
        -d "$body" \
        "http://127.0.0.1:9090$path" 2>/dev/null
}

# Same as rc_api_post_body but tolerates non-2xx (no -f). Useful for
# red-proofs where we expect a 4xx/5xx response and want the body.
rc_api_post_body_raw() {
    local path="$1" body="${2:-}"
    ip netns exec "$NS_B" curl -s -X POST -H "X-API-Key: $API_KEY" \
        -H "Content-Type: application/json" \
        -d "$body" \
        "http://127.0.0.1:9090$path" 2>/dev/null
}

# We intentionally do NOT return a non-zero status from sourcing; the
# milestone script owns the exit code. The EXIT trap fires on whatever
# exit path the milestone takes.

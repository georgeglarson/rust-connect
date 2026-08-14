#!/usr/bin/env bash
# Task 3.2 M1 (vk #991) — kdeconnectd <-> rust-connect identity-exchange smoke.
#
# Two network namespaces joined by ONE veth pair (both ends inside the
# namespaces — deliberately NOT Task 2.2's ns<->host topology: a host leg
# leaks limited broadcasts to the host's real kdeconnect listeners, proven
# during the 2026-08-14 spike when a spike-ns kdeconnectd discovered the
# host's rust-connect.service through rcsp0h). Task 2.2's proven gotchas
# are honored: each namespace gets an explicit default route or a send to
# 255.255.255.255 fails ENETUNREACH (tests/netns_discovery.rs:203-213),
# and the route would not survive a link flap (not flapped here).
#
#   ns A (rc-m1-a-*): distro kdeconnectd under per-instance Xvfb
#       (QT_QPA_PLATFORM=xcb, XDG_SESSION_TYPE=x11 — the offscreen QPA is
#       unsafe per the brief: KSystemClipboard derefs a null QClipboard),
#       private dbus-daemon session bus at an explicit unix:path,
#       isolated XDG_CONFIG_HOME/XDG_DATA_HOME/XDG_RUNTIME_DIR/HOME.
#   ns B (rc-m1-b-*): the repo's rust-connect daemon, isolated data dir,
#       REST API on 127.0.0.1:9090 inside the ns, fixed test API key.
#
# DBUS_SESSION_BUS_ADDRESS is set EXPLICITLY for every child (the distro
# D-Bus service file can auto-activate a stray host kdeconnectd if the
# env leaks; for the rust daemon it is set to a deliberately nonexistent
# path so its D-Bus session backends degrade immediately instead of
# touching any real bus). The private bus additionally runs with NO
# servicedirs (custom bus.conf), so D-Bus activation is impossible on it
# at all — see the bus setup comment for the phantom-instance incident
# that motivated this.
#
# avahi runs on this host and /run/dbus/system_bus_socket is a FILESYSTEM
# socket — reachable from inside a netns. kdeconnectd checks avahi first
# (avahidiscovery.cpp:58-62, lanlinkprovider.cpp:62-69 @ dcd6ded4) and
# would announce via host avahi onto the real LAN. kdeconnectd therefore
# runs in a private mount namespace with the system-bus socket masked by
# /dev/null, forcing the embedded mdnsh path the brief wants verified.
#
# Root-only with the repo's visible-skip convention
# (tests/netns_discovery.rs:1-23): run directly as non-root -> loud skip,
# exit 0. The one-command entry point is tests/interop/run.sh.
#
# Zero-leak invariant (2.2 precedent): an EXIT trap tears everything down
# and asserts `ip netns list` / `ip link show type veth` match the
# pre-run baseline, on success AND on failure.
#
# RC_M1_SABOTAGE=skip-rust|skip-kde exists ONLY for red-before-green
# proof of the assertions; it is not part of the normal interface.

set -u

log()  { printf '[m1] %s\n' "$*"; }
fail() { printf '[m1] FAIL: %s\n' "$*" >&2; }
die()  { fail "$*"; exit 1; }

# ---------------------------------------------------------------- skip gate
if [[ "$(id -u)" != "0" ]]; then
    printf '[m1] SKIP: not running as root — netns/veth creation needs CAP_NET_ADMIN;\n' >&2
    printf '[m1] SKIP: run via `sudo tests/interop/run.sh` to execute this suite.\n' >&2
    exit 0
fi

RC_M1_BIN="${RC_M1_BIN:-}"
[[ -n "$RC_M1_BIN" && -x "$RC_M1_BIN" ]] || die "RC_M1_BIN must point at a built rust-connect binary (use tests/interop/run.sh)"
SABOTAGE="${RC_M1_SABOTAGE:-}"

for tool in ip Xvfb dbus-daemon gdbus tcpdump curl unshare mount; do
    command -v "$tool" >/dev/null || die "required tool not on PATH: $tool"
done
KDECONNECTD=/usr/bin/kdeconnectd
[[ -x "$KDECONNECTD" ]] || die "$KDECONNECTD not installed"

# A pre-existing kdeconnectd is REPORTED, never killed (executor
# discipline) — per-instance bus/XDG isolation keeps this run correct
# regardless of what the host is running.
if pgrep -x kdeconnectd >/dev/null 2>&1; then
    log "NOTE: pre-existing kdeconnectd on the host (pids: $(pgrep -x kdeconnectd | tr '\n' ' ')) — left untouched"
fi

# Honesty note (brief): this is a pinned BINARY version, not a pinned
# source SHA — Fedora can push 26.08.x; the pinned-source lane is M4.
KDE_NEVRA=$(rpm -q kdeconnectd kde-connect-libs kde-connect 2>/dev/null | tr '\n' ' ')
log "KDE reference (pinned binary NEVRA, not source SHA): $KDE_NEVRA"

# ---------------------------------------------------------------- naming
PID=$$
NS_A="rc-m1-a-$PID"
NS_B="rc-m1-b-$PID"
VETH_A="rcm1a$PID"   # IFNAMSIZ is 16 incl NUL; these stay well under
VETH_B="rcm1b$PID"
SUBNET=$((20 + PID % 200))
IP_A="10.250.$SUBNET.2"
IP_B="10.250.$SUBNET.3"
DISPLAY_NUM=$((100 + PID % 800))
DISPLAY=":$DISPLAY_NUM"
WORK=$(mktemp -d /tmp/rc-m1-interop.XXXXXX)
KDE_HOME="$WORK/kde"
KDE_RUNTIME="$KDE_HOME/runtime"
DBUS_ADDR="unix:path=$KDE_RUNTIME/bus"
API_KEY="rc-m1-interop-test-key"
KDE_NAME="rc-m1-kde"
RUST_NAME="rc-m1-rust"
PCAP="$WORK/identity-exchange.pcap"
MONITOR_LOG="$WORK/dbus-monitor.log"
RUST_LOG="$WORK/rust-daemon.log"
KDE_LOG="$WORK/kdeconnectd.log"

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
    for pid in "$KD_PID" "$RC_PID" "$MON_PID" "$TCPDUMP_PID" "$DBUS_PID" "$XVFB_PID"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
    done
    sweep_work_procs TERM
    sleep 1
    for pid in "$KD_PID" "$RC_PID" "$MON_PID" "$TCPDUMP_PID" "$DBUS_PID" "$XVFB_PID"; do
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
kde_dbus() {
    env "${KDE_ENV[@]}" gdbus call --session --dest org.kde.kdeconnect \
        --object-path /modules/kdeconnect --method "$@" 2>/dev/null
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

FAILURES=()
check() { # check <assertion-name> <ok=0/1> <detail>
    if [[ "$2" == "0" ]]; then
        log "ASSERT PASS: $1"
    else
        fail "ASSERT FAIL: $1 — $3"
        FAILURES+=("$1")
    fi
}

# ---------------------------------------------------------------- ns A: kde
KDE_ID=""
if [[ "$SABOTAGE" == "skip-kde" ]]; then
    log "SABOTAGE=skip-kde: kdeconnectd NOT started (red-proof mode only)"
    # Sentinel that can never appear in the rust registry, so the rust-side
    # poll below still runs for real (and times out) instead of being
    # short-circuited — a red proof must exercise the actual poll.
    KDE_ID="redproof000000000000000000000000"
else
    # Mask the system bus (host avahi!) inside a private mount namespace —
    # see header. Without this kdeconnectd picks AvahiDiscovery and
    # announces the test instance onto the real LAN via host avahi.
    ip netns exec "$NS_A" unshare --mount --propagation private \
        bash -c 'mount --bind /dev/null /run/dbus/system_bus_socket || exit 1; exec env "$@"' _ \
        "${KDE_ENV[@]}" "$KDECONNECTD" >"$WORK/kde.stdout" 2>"$KDE_LOG" &
    KD_PID=$!
    disown $! 2>/dev/null || true   # keep SIGKILL job-control noise out of the transcript
    log "kdeconnectd started (pid $KD_PID) in $NS_A"

    kde_bus_up() { kde_dbus org.kde.kdeconnect.daemon.selfId >/dev/null; }
    wait_for 30 "org.kde.kdeconnect on the private bus" kde_bus_up \
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
fi

# --------------------------------------------------------------- ns B: rust
if [[ "$SABOTAGE" == "skip-rust" ]]; then
    log "SABOTAGE=skip-rust: rust-connect NOT started (red-proof mode only)"
else
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
    # DBUS_SESSION_BUS_ADDRESS is explicit for this child too: a bogus
    # path, so the daemon's D-Bus session backends (mpris, notifications,
    # …) degrade instantly instead of touching any real bus.
    ip netns exec "$NS_B" env \
        "HOME=$WORK/rust-home" \
        "DBUS_SESSION_BUS_ADDRESS=unix:path=$WORK/rust-home/no-such-bus" \
        "DISPLAY=" \
        "$RC_M1_BIN" --config "$WORK/rust.toml" \
        >"$RUST_LOG" 2>&1 &
    RC_PID=$!
    disown $! 2>/dev/null || true   # keep SIGKILL job-control noise out of the transcript
    log "rust-connect started (pid $RC_PID) in $NS_B"

    wait_for 30 "rust REST API on 127.0.0.1:9090 (inside $NS_B)" \
        bash -c "ip netns exec \"$NS_B\" curl -sf -H 'X-API-Key: $API_KEY' http://127.0.0.1:9090/api/v1/devices >/dev/null 2>&1" \
        || die "rust REST API never came up (see $RUST_LOG)"
    kill -0 "$RC_PID" 2>/dev/null || die "rust-connect exited (see $RUST_LOG)"
    log "rust side up: REST API answering"
fi

# ------------------------------------------- nudge + mutual discovery wait
# Both implementations broadcast ONCE at start (lanlinkprovider.cpp:149 /
# service_manager startup broadcast) and kdeconnectd re-broadcasts only on
# network change — so order matters. kde started first; rust's startup
# broadcast already carried rust->kde. forceOnNetworkChange makes kde
# re-broadcast now that rust is listening, carrying kde->rust over UDP.
# mDNS runs concurrently the whole time; which channel actually carried
# each direction is determined from evidence below, not assumed.
if [[ -n "$KD_PID" && -n "$RC_PID" ]]; then
    kde_dbus org.kde.kdeconnect.daemon.forceOnNetworkChange >/dev/null \
        && log "forceOnNetworkChange issued (kde re-broadcast)"
fi

RUST_ID=""
KDE_SEES_RUST=1
RUST_SEES_KDE=1

kde_found_rust() {
    kde_dbus org.kde.kdeconnect.daemon.deviceIdByName "$RUST_NAME" \
        | grep -qE "\('[0-9A-Za-z_-]+',\)"
}

# The polls run whenever the corresponding side is actually up — including
# sabotage runs, so a red proof exercises the real poll against the broken
# setup rather than short-circuiting to the assertion.
if [[ -n "$KD_PID" ]]; then
    if wait_for 60 "kde side to discover $RUST_NAME (D-Bus deviceIdByName)" kde_found_rust; then
        RUST_ID=$(kde_dbus org.kde.kdeconnect.daemon.deviceIdByName "$RUST_NAME" | tr -d "()', ")
        KDE_SEES_RUST=0
        log "kde discovered rust: deviceId=$RUST_ID"
    fi
fi

if [[ -n "$RC_PID" ]]; then
    if wait_for 60 "rust side to discover kde $KDE_ID (REST /api/v1/devices)" \
        bash -c "ip netns exec \"$NS_B\" curl -sf -H 'X-API-Key: $API_KEY' http://127.0.0.1:9090/api/v1/devices 2>/dev/null | grep -q '$KDE_ID'"; then
        RUST_SEES_KDE=0
        log "rust discovered kde: $KDE_ID present in /api/v1/devices"
    fi
fi

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
log "KDE reference NEVRA: $KDE_NEVRA"
if (( ${#FAILURES[@]} == 0 )); then
    log "M1 SMOKE: PASS — mutual identity exchange between distro kdeconnectd and rust-connect"
    exit 0
fi
fail "M1 SMOKE: FAIL — ${#FAILURES[@]} assertion(s) failed: ${FAILURES[*]}"
exit 1

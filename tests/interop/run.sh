#!/usr/bin/env bash
# One-command runner for the Task 3.2 interop smokes (vk #991).
#
# Usage:
#   tests/interop/run.sh                # build (as the invoking user) + run M1 as root
#   tests/interop/run.sh m1             # default — identity exchange
#   tests/interop/run.sh m2             # scripted pairing + reconnect
#   tests/interop/run.sh m3             # per-plugin flows (M3 of 4)
#   tests/interop/run.sh m4             # M3 + the M4 unlock knobs (source-built
#                                       # KDE reference + rust-side Xvfb +
#                                       # mpris fake-player helper)
#   tests/interop/run.sh m5             # kdeconnectd-only restart (the SAN
#                                       # fix's live oracle, vk #1045); needs
#                                       # RC_KDECONNECTD for the source build
#   tests/interop/run.sh all            # serial M1 → M2 → M3 → M4 → M5
#   sudo tests/interop/run.sh m2        # already root
#   RC_M2_SABOTAGE=skip-kde-accept tests/interop/run.sh m2
#
# Root-only with the repo's visible-skip convention
# (tests/netns_discovery.rs:1-23): when root cannot be obtained
# (non-root + no passwordless sudo) this prints a loud skip and exits 0 —
# never a silent no-op.
#
# The build happens as the INVOKING user so target/ stays user-owned and
# the root side never touches the rustup shim (the failure mode documented
# in tests/netns_discovery.rs:14-21). The harness itself runs as root via
# `sudo -n` and lives in /mN_smoke.sh next to this file.
#
# Sabotage knobs are env-prefixed per milestone (RC_M1_SABOTAGE,
# RC_M2_SABOTAGE) and are honored for red-before-green proof runs only;
# see the individual smoke scripts.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

MILESTONE="${1:-m1}"
case "$MILESTONE" in
    m1) SABOTAGE_ENV="RC_M1_SABOTAGE" ; BIN_ENV="RC_M1_BIN" ;;
    m2) SABOTAGE_ENV="RC_M2_SABOTAGE" ; BIN_ENV="RC_M2_BIN" ;;
    m3) SABOTAGE_ENV="RC_M3_SABOTAGE" ; BIN_ENV="RC_M3_BIN" ;;
    m4) SABOTAGE_ENV="RC_M4_SABOTAGE" ; BIN_ENV="RC_M4_BIN" ;;
    m5) SABOTAGE_ENV="RC_M5_SABOTAGE" ; BIN_ENV="RC_M5_BIN" ;;
    all)
        # One-command runner: serial M1 → M2 → M3 → M4 → M5. Each is its own
        # PASS/FAIL gate; we always attempt every milestone so a single
        # failure doesn't blind subsequent lanes, then exit non-zero if
        # any of them failed. The ZERO-LEAK invariant gates every
        # milestone independently inside lib.sh.
        echo "[run.sh] all: serial M1 → M2 → M3 → M4 → M5 (each is its own PASS/FAIL gate)"
        any_fail=0
        for ms in m1 m2 m3 m4 m5; do
            echo "[run.sh] === running $ms ==="
            if ! "${0}" "$ms"; then
                echo "[run.sh] === $ms FAILED ===" >&2
                any_fail=1
            else
                echo "[run.sh] === $ms OK ==="
            fi
        done
        if [[ "$any_fail" -ne 0 ]]; then
            echo "[run.sh] all: at least one milestone failed" >&2
            exit 1
        fi
        exit 0
        ;;
    *)
        echo "[run.sh] FAIL: unknown milestone: $MILESTONE (allowed: m1 | m2 | m3 | m4 | m5 | all)" >&2
        exit 1
        ;;
esac
# External callers may also set the sabotage var directly. Honor it.
SABOTAGE_VAL="${!SABOTAGE_ENV:-${RC_SABOTAGE:-}}"

if [[ "$(id -u)" == "0" ]]; then
    SUDO=()
elif sudo -n true 2>/dev/null; then
    SUDO=(sudo -n)
else
    printf '[run.sh] SKIP: not root and passwordless sudo unavailable — the interop\n' >&2
    printf '[run.sh] SKIP: smoke needs CAP_NET_ADMIN (netns/veth). Re-run with sudo to execute.\n' >&2
    exit 0
fi

echo "[run.sh] building rust-connect (cargo build --locked) as $(id -un)…"
cargo build --locked
# Examples (mpris_fake_player) are referenced by the harness when
# RC_MPRIS_FAKE is set. Build them all up-front so the smoke doesn't need
# to know which ones it needs; `cargo build --examples --locked` is
# incremental against target/ so this is cheap on cache hits.
cargo build --examples --locked

RC_BIN="$REPO_ROOT/target/debug/rust-connect"
[[ -x "$RC_BIN" ]] || { echo "[run.sh] FAIL: expected binary missing: $RC_BIN" >&2; exit 1; }

SMOKE="$REPO_ROOT/tests/interop/${MILESTONE}_smoke.sh"
[[ -f "$SMOKE" ]] || { echo "[run.sh] FAIL: smoke not found: $SMOKE" >&2; exit 1; }

# The M4/M5 source-built reference's RUNPATH was baked to its build
# worktree (since deleted), so the install's lib64 must ride
# LD_LIBRARY_PATH — set explicitly here because sudo's env_reset strips
# it from the invoking shell before the exec'd env(1) could inherit it.
if [[ -n "${RC_KDECONNECTD:-}" ]]; then
    KDE_LIB_PATH="$(dirname "$(dirname "$(readlink -f "$RC_KDECONNECTD")")")/lib64"
    [[ -d "$KDE_LIB_PATH" ]] \
        || { echo "[run.sh] FAIL: RC_KDECONNECTD=$RC_KDECONNECTD but no lib64 at $KDE_LIB_PATH" >&2; exit 1; }
else
    KDE_LIB_PATH=""
fi

exec "${SUDO[@]}" env \
    "$BIN_ENV=$RC_BIN" \
    "$SABOTAGE_ENV=$SABOTAGE_VAL" \
    "RC_KDECONNECTD=${RC_KDECONNECTD:-}" \
    "RC_RUST_DISPLAY=${RC_RUST_DISPLAY:-}" \
    "RC_MPRIS_FAKE=${RC_MPRIS_FAKE:-}" \
    ${KDE_LIB_PATH:+"LD_LIBRARY_PATH=$KDE_LIB_PATH"} \
    bash "$SMOKE"

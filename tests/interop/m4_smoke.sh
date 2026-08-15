#!/usr/bin/env bash
# M4 wrapper: same harness as M3 with the M4 unlock knobs pre-set.
#
# vk #991 M4: M3 deferred walls re-run with packaging + source-pinned
# kdeconnect-kde reference. M4 is fundamentally an M3 lane whose unlock
# env vars (RC_KDECONNECTD / RC_RUST_DISPLAY / RC_MPRIS_FAKE) are set
# to point at the M4 build artifacts. The actual phase logic lives in
# m3_smoke.sh — Phase 3's kde→rust clipboard gates on RC_RUST_DISPLAY,
# Phase 6's MPRIS gates on RC_MPRIS_FAKE.
#
# This wrapper:
#   1. Builds the source-pinned kdeconnect-kde reference if it's missing
#      (the only network-fence exception in M4 besides dnf builddep).
#   2. Forces RC_KDECONNECTD=$REPO_ROOT/tests/interop/.kde/install/bin/kdeconnectd
#      so M1/M2/M3 all run against the pinned reference, not /usr/bin.
#   3. Forces RC_RUST_DISPLAY=1 so the rust daemon is wired to its own
#      Xvfb and kde→rust clipboard is testable.
#   4. Forces RC_MPRIS_FAKE=1 so m3 Phase 6 plants a fake player on the
#      kde side's session bus and exercises the request flow.
#
# It then exec's m3_smoke.sh with everything in place. No new phase
# logic — M4 is "M3, against source-built reference, with all M3 walls
# unwalled."  Per-instance pipewire-pulse + runcommand remain fenced
# (vk #1007 human ruling) and are tested at vk #1018 — see
# plans/task-3.2-m4-report.md § vk #1018 lock-rewrite validation.
#
# This script honors the same SUDO contract as the rest of the harness:
# run.sh invokes it under sudo so it can drop into netns/veth.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

KDE_INSTALL="$REPO_ROOT/tests/interop/.kde/install/bin/kdeconnectd"
KDE_MANIFEST="$REPO_ROOT/tests/interop/.kde/SOURCE_MANIFEST.toml"

echo "[m4_smoke] M4 unlock wrapper: source-built KDE + rust-side Xvfb + mpris fake"

# 1. Build the source-pinned reference if missing. This is the ONE M4
#    network-fence exception (the brief: invent.kde.org source fetch +
#    dnf builddep). It runs as the invoking user (us, inside sudo).
#    When the build is already present (the normal case after a prior
#    M4 run), this is a no-op.
if [[ ! -x "$KDE_INSTALL" ]]; then
    echo "[m4_smoke] source-built kdeconnectd missing; building from pinned tag"
    bash "$REPO_ROOT/tests/interop/m4_build_kde.sh"
else
    echo "[m4_smoke] source-built kdeconnectd present: $KDE_INSTALL"
fi
[[ -f "$KDE_MANIFEST" ]] || {
    echo "[m4_smoke] FAIL: SOURCE_MANIFEST.toml missing at $KDE_MANIFEST" >&2
    exit 1
}

# 2-4. Force all three M4 unlock knobs. The smoke scripts honor them via
#      the lib.sh helpers; setting them here makes the M3 phases behave
#      as their M4-unwalled forms.
export RC_KDECONNECTD="$KDE_INSTALL"
export RC_RUST_DISPLAY=1
export RC_MPRIS_FAKE=1

echo "[m4_smoke] RC_KDECONNECTD=$RC_KDECONNECTD"
echo "[m4_smoke] RC_RUST_DISPLAY=$RC_RUST_DISPLAY RC_MPRIS_FAKE=$RC_MPRIS_FAKE"
echo "[m4_smoke] execing m3_smoke.sh (M4 = M3 with M4 knobs pre-set)"

# m3_smoke.sh reads RC_BIN="${RC_M3_BIN:-}". run.sh passed the binary path
# as RC_M4_BIN (BIN_ENV for the m4 case). Lift it into RC_M3_BIN so the
# m3 smoke reads it without needing to know about the m4 dispatch.
export RC_M3_BIN="${RC_M4_BIN:-}"

exec bash "$REPO_ROOT/tests/interop/m3_smoke.sh"
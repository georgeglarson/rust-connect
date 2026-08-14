#!/usr/bin/env bash
# One-command runner for the Task 3.2 interop smokes (vk #991).
#
# Usage:
#   tests/interop/run.sh                # build (as the invoking user) + run M1 as root
#   tests/interop/run.sh m1             # default — identity exchange
#   tests/interop/run.sh m2             # scripted pairing + reconnect
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
# `sudo -n` and lives in /m1_smoke.sh or m2_smoke.sh next to this file.
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
    *)
        echo "[run.sh] FAIL: unknown milestone: $MILESTONE (allowed: m1 | m2)" >&2
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

RC_BIN="$REPO_ROOT/target/debug/rust-connect"
[[ -x "$RC_BIN" ]] || { echo "[run.sh] FAIL: expected binary missing: $RC_BIN" >&2; exit 1; }

SMOKE="$REPO_ROOT/tests/interop/${MILESTONE}_smoke.sh"
[[ -f "$SMOKE" ]] || { echo "[run.sh] FAIL: smoke not found: $SMOKE" >&2; exit 1; }

exec "${SUDO[@]}" env \
    "$BIN_ENV=$RC_BIN" \
    "$SABOTAGE_ENV=$SABOTAGE_VAL" \
    bash "$SMOKE"

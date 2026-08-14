#!/usr/bin/env bash
# One-command runner for the Task 3.2 M1 interop smoke (vk #991).
#
# Usage:
#   tests/interop/run.sh            # build (as the invoking user) + run as root
#   sudo tests/interop/run.sh       # same, already root
#
# Root-only with the repo's visible-skip convention
# (tests/netns_discovery.rs:1-23): when root cannot be obtained
# (non-root + no passwordless sudo) this prints a loud skip and exits 0 —
# never a silent no-op.
#
# The build happens as the INVOKING user so target/ stays user-owned and
# the root side never touches the rustup shim (the failure mode documented
# in tests/netns_discovery.rs:14-21). The harness itself runs as root via
# `sudo -n` and lives in m1_smoke.sh next to this file.
#
# RC_M1_SABOTAGE=skip-rust|skip-kde is honored for red-before-green proof
# runs only; see m1_smoke.sh.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

if [[ "$(id -u)" == "0" ]]; then
    SUDO=()
elif sudo -n true 2>/dev/null; then
    SUDO=(sudo -n)
else
    printf '[run.sh] SKIP: not root and passwordless sudo unavailable — the M1 interop\n' >&2
    printf '[run.sh] SKIP: smoke needs CAP_NET_ADMIN (netns/veth). Re-run with sudo to execute.\n' >&2
    exit 0
fi

echo "[run.sh] building rust-connect (cargo build --locked) as $(id -un)…"
cargo build --locked

RC_BIN="$REPO_ROOT/target/debug/rust-connect"
[[ -x "$RC_BIN" ]] || { echo "[run.sh] FAIL: expected binary missing: $RC_BIN" >&2; exit 1; }

exec "${SUDO[@]}" env \
    "RC_M1_BIN=$RC_BIN" \
    "RC_M1_SABOTAGE=${RC_M1_SABOTAGE:-}" \
    bash "$REPO_ROOT/tests/interop/m1_smoke.sh"

#!/usr/bin/env bash
# Install rust-connect as a systemd --user service on this host.
# Idempotent: safe to re-run after a rebuild.
#
# A user service, not a system one: the daemon needs the session DBus for
# desktop notifications, $HOME for its identity and paired.json, and
# ~/Downloads for received files. See packaging/rust-connect.service.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${HOME}/.local/bin"
UNIT_DIR="${HOME}/.config/systemd/user"

echo "==> Building release binary"
# Force build.rs to re-run so RC_GIT_SHA is re-stamped from THIS checkout's
# HEAD. Without this, a warm shared target dir can answer "up to date" from
# another worktree's release fingerprints (.git/HEAD doesn't move on a merge
# of a packed ref), and the installed binary silently trails main — observed
# live 2026-09-04: deploy reported 7dea30d on main bdf3bd9 until a forced
# cargo clean -p. The touch costs a rebuild only when the stamp changes.
touch "${REPO_ROOT}/build.rs"
cargo build --release --locked --manifest-path "${REPO_ROOT}/Cargo.toml"

echo "==> Installing binary to ${BIN_DIR}/rust-connect"
mkdir -p "${BIN_DIR}"
# install(1) to a running binary's path would fail with ETXTBSY, so stop first
# if the service is already up. Re-running this script is the normal upgrade
# path, and that path always has the old binary running.
systemctl --user stop rust-connect.service 2>/dev/null || true
install -m 0755 "${REPO_ROOT}/target/release/rust-connect" "${BIN_DIR}/rust-connect"

echo "==> Installing unit to ${UNIT_DIR}/rust-connect.service"
mkdir -p "${UNIT_DIR}"
install -m 0644 "${REPO_ROOT}/packaging/rust-connect.service" \
    "${UNIT_DIR}/rust-connect.service"

echo "==> Reloading user systemd"
systemctl --user daemon-reload
systemctl --user enable --now rust-connect.service

echo "==> Status"
systemctl --user --no-pager status rust-connect.service || true

#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

case "${1:-amd64}" in
    amd64)  TARGET="x86_64-unknown-linux-gnu" ;;
    arm64)  TARGET="aarch64-unknown-linux-gnu" ;;
    *) echo "ERROR: Unsupported architecture: ${1:-amd64} (supported: amd64, arm64)"; exit 1 ;;
esac
ARCH="${1:-amd64}"

VERSION=$(grep '^version' "$PROJECT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
if [ -z "$VERSION" ]; then
    echo "ERROR: Could not extract version from Cargo.toml"
    exit 1
fi

DEB_DIR="${SCRIPT_DIR}/deb"
DEB_NAME="rust-connect_${VERSION}_${ARCH}"
BINARY="target/${TARGET}/release/rust-connect"

echo "==> Building rust-connect ${VERSION} for ${ARCH} (${TARGET})..."

cd "$PROJECT_DIR"
cargo build --release --target "$TARGET"

[ -f "$BINARY" ] || { echo "ERROR: Binary not found at $BINARY"; exit 1; }

echo "==> Assembling .deb package..."

rm -rf "${DEB_DIR:?}/usr" "${DEB_DIR:?}/lib"
mkdir -p "${DEB_DIR}/usr/bin"
mkdir -p "${DEB_DIR}/usr/lib/systemd/user"

cp "$BINARY" "${DEB_DIR}/usr/bin/rust-connect"
chmod 755 "${DEB_DIR}/usr/bin/rust-connect"

# rust-connect is a USER service (session DBus, $HOME, ~/Downloads — see the
# unit's own comment), so the unit ships to /usr/lib/systemd/user/, not
# /lib/systemd/system/. The shipped unit's ExecStart points at ~/.local/bin,
# which is where install-user-service.sh puts the binary for a from-source
# install; a packaged install puts it in /usr/bin instead, so rewrite it here.
# One template, two destinations, substitution made explicit.
cp "${SCRIPT_DIR}/rust-connect.service" "${DEB_DIR}/usr/lib/systemd/user/rust-connect.service"
sed -i 's|^ExecStart=.*|ExecStart=/usr/bin/rust-connect|' \
    "${DEB_DIR}/usr/lib/systemd/user/rust-connect.service"
chmod 644 "${DEB_DIR}/usr/lib/systemd/user/rust-connect.service"

grep -q '^ExecStart=/usr/bin/rust-connect$' \
    "${DEB_DIR}/usr/lib/systemd/user/rust-connect.service" \
    || { echo "ERROR: ExecStart rewrite failed"; exit 1; }

sed -i "s/^Version: .*/Version: $VERSION/" "${DEB_DIR}/DEBIAN/control"
sed -i "s/^Architecture: .*/Architecture: $ARCH/" "${DEB_DIR}/DEBIAN/control"

chmod 755 "${DEB_DIR}/DEBIAN/postinst"
chmod 755 "${DEB_DIR}/DEBIAN/prerm"
chmod 755 "${DEB_DIR}/DEBIAN/postrm"

DEB_PATH="${SCRIPT_DIR}/${DEB_NAME}.deb"
dpkg-deb --build --root-owner-group "$DEB_DIR" "$DEB_PATH"

echo "==> Built: ${DEB_PATH}"
echo "    Install: sudo dpkg -i ${DEB_PATH}"
echo "    Start:   systemctl --user start rust-connect"

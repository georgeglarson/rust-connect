#!/usr/bin/env bash
# Build the source-pinned kdeconnect-kde reference for M4 (vk #991).
#
# This script is invoked by tests/interop/m4_smoke.sh when the pinned
# reference isn't already built. Idempotent: it skips any step whose
# output already exists. Honors the network-fence exception granted for
# M4 item 1 — ONLY invents.kde.org (git clone) and dnf builddep (deps).
#
# Pin source lives in tests/interop/.kde/SOURCE_MANIFEST.toml. Update
# that file to bump the tag/commit and the next invocation picks it up.
#
# Layout under /tmp/rc-m4-build/:
#   src/    — git clone (per-machine)
#   build/  — cmake build tree (per-machine)
#   prefix.sh — KDE-provided env script that the smoke invokes before
#               running kdeconnectd from the install/ tree
# Final install lands at tests/interop/.kde/install/ inside the worktree.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
BUILD_ROOT="${BUILD_ROOT:-/tmp/rc-m4-build}"
SRC_DIR="$BUILD_ROOT/src"
BUILD_DIR="$BUILD_ROOT/build"
INSTALL_PREFIX="$REPO_ROOT/tests/interop/.kde/install"
MANIFEST="$REPO_ROOT/tests/interop/.kde/SOURCE_MANIFEST.toml"
LOG_DIR="$BUILD_ROOT/logs"
JOBS="${JOBS:-$(nproc)}"

mkdir -p "$LOG_DIR" "$INSTALL_PREFIX"

# ---------------------------------------------------------------------
# Parse pin from SOURCE_MANIFEST.toml. The TOML is hand-authored and
# the keys are stable; do NOT introduce a TOML dependency here.
# ---------------------------------------------------------------------
[[ -f "$MANIFEST" ]] || { echo "[m4_build_kde] FAIL: $MANIFEST missing" >&2; exit 1; }
SOURCE_REPO=$(grep '^source_repo' "$MANIFEST" | head -1 | sed -E 's/.*= *"([^"]+)".*/\1/')
SOURCE_TAG=$(grep '^source_tag' "$MANIFEST" | head -1 | sed -E 's/.*= *"([^"]+)".*/\1/')
SOURCE_COMMIT=$(grep '^source_commit' "$MANIFEST" | head -1 | sed -E 's/.*= *"([^"]+)".*/\1/')
[[ -n "$SOURCE_REPO" && -n "$SOURCE_TAG" && -n "$SOURCE_COMMIT" ]] || {
    echo "[m4_build_kde] FAIL: could not parse $MANIFEST" >&2; exit 1; }
echo "[m4_build_kde] pin: $SOURCE_REPO @ $SOURCE_TAG ($SOURCE_COMMIT)"

KDE_BIN="$INSTALL_PREFIX/bin/kdeconnectd"
if [[ -x "$KDE_BIN" ]]; then
    EXISTING_VER=$("$KDE_BIN" --version 2>&1 | head -1 || true)
    echo "[m4_build_kde] SKIP build: $KDE_BIN already present ($EXISTING_VER)"
    echo "[m4_build_kde] To force a rebuild: rm -rf $BUILD_ROOT/src $BUILD_ROOT/build $INSTALL_PREFIX"
    exit 0
fi

# ---------------------------------------------------------------------
# 1. Clone (or refresh) the source.
# ---------------------------------------------------------------------
if [[ ! -d "$SRC_DIR/.git" ]]; then
    echo "[m4_build_kde] cloning $SOURCE_REPO -> $SRC_DIR"
    git clone "$SOURCE_REPO" "$SRC_DIR" 2>"$LOG_DIR/clone.log"
fi
cd "$SRC_DIR"
echo "[m4_build_kde] fetching + checking out $SOURCE_TAG"
git fetch --tags origin 2>"$LOG_DIR/fetch.log"
# Match the pinned commit exactly (not just the tag) so a force-push
# between tag and commit shows up loudly.
if ! git checkout "$SOURCE_COMMIT" 2>"$LOG_DIR/checkout.log"; then
    git checkout "$SOURCE_TAG" 2>>"$LOG_DIR/checkout.log"
    ACTUAL=$(git rev-parse HEAD)
    if [[ "$ACTUAL" != "$SOURCE_COMMIT" ]]; then
        echo "[m4_build_kde] FAIL: HEAD $ACTUAL != pinned $SOURCE_COMMIT" >&2
        exit 1
    fi
fi
echo "[m4_build_kde] HEAD: $(git rev-parse HEAD)"

# ---------------------------------------------------------------------
# 2. dnf builddep (Fedora-specific). Skipped if it ran once on this
#    machine — `rpm -qa` cross-checked against builddep-packages.txt
#    from a prior run. Fast-path if the manifest record already exists.
# ---------------------------------------------------------------------
DEPS_RECORD="$BUILD_ROOT/builddep-packages.txt"
if [[ -f "$DEPS_RECORD" ]]; then
    echo "[m4_build_kde] SKIP builddep: prior run recorded $(wc -l < "$DEPS_RECORD") packages in $DEPS_RECORD"
else
    echo "[m4_build_kde] dnf builddep kdeconnect-kde (this is the M4 network-fence exception)"
    sudo -n dnf -y builddep kdeconnect-kde \
        >"$LOG_DIR/builddep.log" 2>&1 || {
            echo "[m4_build_kde] FAIL: dnf builddep; see $LOG_DIR/builddep.log" >&2
            exit 1
        }
    rpm -qa --qf '%{NAME}\n' \
        | sort -u \
        | grep -E '^(kf6|qt6|ModemManager|libfakekey|pulseaudio-qt|wayland|avahi|gnutls|cups|extra-cmake-modules|polkit-qt6|libei)' \
        > "$DEPS_RECORD" || true
    echo "[m4_build_kde] builddep recorded $(wc -l < "$DEPS_RECORD") packages"
fi

# ---------------------------------------------------------------------
# 3. CMake configure. Output goes to BUILD_DIR; install prefix is the
#    in-tree path so the worktree doesn't depend on /tmp for runtime.
# ---------------------------------------------------------------------
mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"
echo "[m4_build_kde] cmake configure"
cmake -S "$SRC_DIR" -B "$BUILD_DIR" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$INSTALL_PREFIX" \
    -DBUILD_TESTING=OFF \
    >"$LOG_DIR/cmake-configure.log" 2>&1 || {
        echo "[m4_build_kde] FAIL: cmake configure; see $LOG_DIR/cmake-configure.log" >&2
        exit 1
    }

# ---------------------------------------------------------------------
# 4. Build + install.
# ---------------------------------------------------------------------
echo "[m4_build_kde] cmake build -j$JOBS"
cmake --build "$BUILD_DIR" -j "$JOBS" \
    >"$LOG_DIR/build.log" 2>&1 || {
        echo "[m4_build_kde] FAIL: cmake build; see $LOG_DIR/build.log (tail):" >&2
        tail -50 "$LOG_DIR/build.log" >&2
        exit 1
    }

echo "[m4_build_kde] cmake install"
cmake --install "$BUILD_DIR" \
    >"$LOG_DIR/install.log" 2>&1 || {
        echo "[m4_build_kde] FAIL: cmake install; see $LOG_DIR/install.log" >&2
        exit 1
    }

# Sanity-check the install.
[[ -x "$KDE_BIN" ]] || { echo "[m4_build_kde] FAIL: kdeconnectd not at $KDE_BIN" >&2; exit 1; }
"$KDE_BIN" --version >"$LOG_DIR/kdeconnectd-version.txt" 2>&1 || true
echo "[m4_build_kde] DONE: $KDE_BIN"
echo "[m4_build_kde] version: $(head -1 "$LOG_DIR/kdeconnectd-version.txt")"
echo "[m4_build_kde] logs: $LOG_DIR"
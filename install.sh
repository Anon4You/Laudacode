#!/bin/sh
# Laudacode installer — Termux, Linux and macOS. No sudo inside Termux.
#
#   curl -fsSL https://raw.githubusercontent.com/Anon4You/Laudacode/main/install.sh | sh
#
# Env overrides: REPO, LAUDACODE_VERSION (default: latest release), PREFIX, TMPDIR

set -e

if [ -n "${TERMUX_VERSION:-}" ]; then
    PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"
    # Termux already exports $TMPDIR (= $PREFIX/tmp); this is only a safety net.
    [ -n "${TMPDIR:-}" ] || TMPDIR="$PREFIX/tmp"
else
    PREFIX="${PREFIX:-/usr/local}"
    TMPDIR="${TMPDIR:-/tmp}"
fi
REPO="${REPO:-Anon4You/Laudacode}"
BUILD_DIR="$TMPDIR/laudacode-build"

# --- dependencies ---------------------------------------------------------------
for tool in curl tar cargo; do
    command -v "$tool" >/dev/null 2>&1 || {
        if [ "$tool" = "cargo" ]; then
            echo "✗ cargo not found — install rust first (Termux: pkg install rust | others: https://rustup.rs)" >&2
        else
            echo "✗ $tool not found — install it first (Termux: pkg install $tool)" >&2
        fi
        exit 1
    }
done

# --- resolve latest release ---------------------------------------------------------
if [ -n "${LAUDACODE_VERSION:-}" ]; then
    VERSION="$LAUDACODE_VERSION"
else
    echo "==> resolving latest release"
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
    [ -n "$VERSION" ] || {
        echo "✗ could not get latest version from GitHub — set LAUDACODE_VERSION=vX.Y.Z manually" >&2
        exit 1
    }
fi

# --- sudo only outside Termux, only if needed -------------------------------------
SUDO=""
if [ ! -w "$PREFIX" ] && [ "$(id -u)" != "0" ] && command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
fi

# --- download --------------------------------------------------------------------
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"

echo "==> downloading ${REPO}@${VERSION}"
curl -L -o "laudacode-${VERSION}.tar.gz" \
    "https://github.com/${REPO}/archive/refs/tags/${VERSION}.tar.gz"

tar -xzf "laudacode-${VERSION}.tar.gz"
cd "Laudacode-${VERSION}"

# --- build -------------------------------------------------------------------------
echo "==> building (this can take several minutes)"
CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-off}" \
    cargo build --release --locked

[ -f target/release/laudacode ] || { echo "✗ build finished but binary is missing" >&2; exit 1; }

# --- install ------------------------------------------------------------------------
mkdir -p "${PREFIX%/}/bin"
$SUDO install -m 755 target/release/laudacode "${PREFIX%/}/bin/laudacode"

cd /
rm -rf "$BUILD_DIR"

echo "==> installed: ${PREFIX%/}/bin/laudacode"
exit 0

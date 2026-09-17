#!/bin/sh
# Laudacode installer — Termux, Linux and macOS. No sudo inside Termux.
#
#   curl -fsSL https://raw.githubusercontent.com/Anon4You/Laudacode/main/install.sh | sh
#
# Tries a prebuilt release binary for your platform first (fast, no rust
# needed); falls back to building from source if no asset matches.
#
# Env overrides: REPO, LAUDACODE_VERSION (default: latest release), PREFIX,
# TMPDIR, FORCE_BUILD=1 (skip prebuilt binaries)

set -eu

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

# --- dependencies needed in all paths ---------------------------------------------
for tool in curl tar; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "✗ $tool not found — install it first (Termux: pkg install $tool)" >&2
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

install_binary() {
    if [ -n "$SUDO" ]; then
        $SUDO mkdir -p "${PREFIX%/}/bin"
        $SUDO install -m 755 "$1" "${PREFIX%/}/bin/laudacode"
    else
        mkdir -p "${PREFIX%/}/bin"
        install -m 755 "$1" "${PREFIX%/}/bin/laudacode"
    fi
}

# --- map machine → release target ---------------------------------------------------
# Release assets are named laudacode-<tag>-<rust-target>.tar.gz.
detect_target() {
    arch="$(uname -m)"
    if [ -n "${TERMUX_VERSION:-}" ]; then
        # Android builds are Bionic-linked; musl binaries are not offered for Termux.
        case "$arch" in
            aarch64)              echo "aarch64-linux-android";   return 0 ;;
            armv7l|armv8l|armv7)  echo "armv7-linux-androideabi"; return 0 ;;
            x86_64)               echo "x86_64-linux-android";    return 0 ;;
            i686|i386)            echo "i686-linux-android";      return 0 ;;
            *)                    return 1 ;;
        esac
    else
        # Static musl binaries run on any Linux (glibc/musl/alpine).
        case "$arch" in
            x86_64)  echo "x86_64-unknown-linux-musl";  return 0 ;;
            aarch64) echo "aarch64-unknown-linux-musl"; return 0 ;;
            *)       return 1 ;;
        esac
    fi
}

# --- prebuilt binary ------------------------------------------------------------------
# Returns 0 when laudacode was installed, 1 when the caller should build from source.
try_prebuilt() {
    [ "${FORCE_BUILD:-0}" = "1" ] && return 1
    TARGET="$(detect_target)" || return 1
    ASSET="laudacode-${VERSION}-${TARGET}.tar.gz"
    BASE="https://github.com/${REPO}/releases/download/${VERSION}"

    echo "==> trying prebuilt binary: ${ASSET}"
    rm -rf "$BUILD_DIR"
    mkdir -p "$BUILD_DIR"
    # -f: a missing asset must fail here (→ fallback), not save an HTML page.
    curl -fL --connect-timeout 15 -o "$BUILD_DIR/$ASSET" "$BASE/$ASSET" || return 1

    # Verify against the release's SHA256SUMS (always published by CI).
    if curl -fsSL --connect-timeout 15 -o "$BUILD_DIR/SHA256SUMS" "$BASE/SHA256SUMS" \
        && grep -q " $ASSET\$" "$BUILD_DIR/SHA256SUMS"; then
        expected="$(grep " $ASSET\$" "$BUILD_DIR/SHA256SUMS" | awk '{print $1}')"
        actual="$(sha256sum "$BUILD_DIR/$ASSET" | awk '{print $1}')"
        if [ "$expected" != "$actual" ]; then
            echo "✗ checksum mismatch — refusing to install; falling back to source build" >&2
            return 1
        fi
        echo "==> checksum OK"
    else
        echo "✗ SHA256SUMS unavailable for ${VERSION} — falling back to source build" >&2
        return 1
    fi

    tar -xzf "$BUILD_DIR/$ASSET" -C "$BUILD_DIR"
    [ -f "$BUILD_DIR/laudacode" ] || return 1
    install_binary "$BUILD_DIR/laudacode"
    echo "==> installed prebuilt binary: ${PREFIX%/}/bin/laudacode"
    "${PREFIX%/}/bin/laudacode" --version 2>/dev/null || true
}

# --- build from source (fallback) -------------------------------------------------------
build_from_source() {
    command -v cargo >/dev/null 2>&1 || {
        echo "✗ cargo not found — install rust first (Termux: pkg install rust | others: https://rustup.rs)" >&2
        exit 1
    }
    rm -rf "$BUILD_DIR"
    mkdir -p "$BUILD_DIR"
    trap 'rm -rf "$BUILD_DIR"' EXIT INT TERM

    echo "==> downloading ${REPO}@${VERSION}"
    # -f so HTTP errors (missing tag, rate limit) fail instead of saving an HTML page.
    curl -fL -o "$BUILD_DIR/laudacode.tar.gz" \
        "https://github.com/${REPO}/archive/refs/tags/${VERSION}.tar.gz"

    # Sanity-check the archive before extracting for a clearer failure message.
    tar -tzf "$BUILD_DIR/laudacode.tar.gz" >/dev/null || {
        echo "✗ downloaded file is not a valid tar.gz — check the tag name" >&2
        exit 1
    }

    cd "$BUILD_DIR"
    tar -xzf laudacode.tar.gz

    # GitHub names the root dir "<Repo>-<tag-without-leading-v>", but don't trust it.
    SRC_DIR="$(find . -maxdepth 1 -type d -name 'Laudacode-*' | head -n 1)"
    [ -n "$SRC_DIR" ] || { echo "✗ unexpected archive layout — no Laudacode-* directory" >&2; exit 1; }
    cd "$SRC_DIR"

    echo "==> building (this can take several minutes)"
    CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-off}" \
        cargo build --release --locked

    [ -f target/release/laudacode ] || { echo "✗ build finished but binary is missing" >&2; exit 1; }

    install_binary target/release/laudacode

    echo "==> installed: ${PREFIX%/}/bin/laudacode"
    "${PREFIX%/}/bin/laudacode" --version 2>/dev/null || true
}

if try_prebuilt; then
    :
else
    echo "==> no prebuilt binary for this platform — building from source"
    build_from_source
fi

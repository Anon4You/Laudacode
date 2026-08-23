#!/bin/sh
# End-to-end test harness for install.sh.
#
# Fakes the network (curl → local HTTP server) and the toolchain (cargo),
# then runs the REAL install.sh with PREFIX pointed at a scratch dir.
# Exercises: dependency check, release resolution, download, archive
# validation, extraction dir detection, build step, install, cleanup.

set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
TEST_DIR="$TMPDIR/laudacode-install-test-$$"
FAKE_BIN="$TEST_DIR/bin"
SRV_ROOT="$TEST_DIR/srv"
PREFIX_DIR="$TEST_DIR/prefix"

cleanup() {
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT INT TERM

mkdir -p "$FAKE_BIN" "$SRV_ROOT" "$PREFIX_DIR"

# Absolute paths for tools the shims need — PATH will be shadowed by FAKE_BIN.
SED="$(command -v sed)"
CAT="$(command -v cat)"
CP="$(command -v cp)"
MKDIR="$(command -v mkdir)"
CHMOD="$(command -v chmod)"
PRINTFLN="$(command -v printf || echo echo)"

# --- fake cargo: pretends to build, drops a stub binary ---------------------------
cat > "$FAKE_BIN/cargo" <<EOF
#!/bin/sh
echo "[shim] cargo \$*"
case " \$* " in
    *" --locked "*|*" build "*)
        # Real cargo builds into the invoking directory — mirror that.
        "$MKDIR" -p target/release
        "$PRINTFLN" '#!/bin/sh\necho "laudacode 9.9.9 (test stub)"\n' > target/release/laudacode
        "$CHMOD" +x target/release/laudacode
        ;;
esac
exit 0
EOF
chmod +x "$FAKE_BIN/cargo"

# --- curl shim: map github URLs onto the local server ------------------------------
cat > "$FAKE_BIN/curl" <<EOF
#!/bin/sh
url=""
out=""
prev=""
for a in "\$@"; do
    case "\$a" in
        http*|"https"*) url="\$a" ;;
        -o) prev="-o" ;;
        *) if [ "\$prev" = "-o" ]; then out="\$a"; fi; prev="" ;;
    esac
done
[ -n "\$url" ] || { echo "[shim] curl: no url in \$*" >&2; exit 22; }
path=\$("$(command -v printf)" '%s' "\$url" | "$SED" "s|https://api.github.com||; s|https://github.com||")
exec "$ROOT/fake-http" "" "\$path" \${out:+-o "\$out"}
EOF
chmod +x "$FAKE_BIN/curl"

# --- tiny http fetch helper: serves from SRV_ROOT, mirrors curl's -f semantics ----
cat > "$ROOT/fake-http" <<SHIM
#!/bin/sh
# Local stand-in for github.com: resolves paths under the fixture root.
path="\$2"; shift 2
out=""
if [ "\$1" = "-o" ]; then out="\$2"; fi
file="$SRV_ROOT\$path"
if [ ! -f "\$file" ]; then
    if [ -n "\$out" ]; then echo "404 not found" > "\$out"; exit 0; fi
    echo "curl-shim: (22) The requested URL returned error: 404" >&2
    exit 22
fi
if [ -n "\$out" ]; then "$CP" "\$file" "\$out"; else "$CAT" "\$file"; fi
SHIM
chmod +x "$ROOT/fake-http"

# --- fixtures the fake GitHub serves ----------------------------------------------
mkdir -p "$SRV_ROOT/repos/Anon4You/Laudacode/releases"
printf '{"tag_name":"v9.9.9","name":"x"}\n' \
    > "$SRV_ROOT/repos/Anon4You/Laudacode/releases/latest"

# Archive root dir WITHOUT the 'v', like GitHub does for tag v9.9.9.
STAGE="$SRV_ROOT/stage/Laudacode-9.9.9"
mkdir -p "$STAGE/src"
printf '[package]\nname="laudacode"\nversion="9.9.9"\n' > "$STAGE/Cargo.toml"
printf 'fn main(){}\n' > "$STAGE/src/main.rs"
tar -czf "$SRV_ROOT/Anon4You/Laudacode/archive/refs/tags/v9.9.9.tar.gz" -C "$SRV_ROOT/stage" . 2>/dev/null \
    || { mkdir -p "$(dirname "$SRV_ROOT/Anon4You/Laudacode/archive/refs/tags/v9.9.9.tar.gz")";
         tar -czf "$SRV_ROOT/Anon4You/Laudacode/archive/refs/tags/v9.9.9.tar.gz" -C "$SRV_ROOT/stage" .; }

# --- run ---------------------------------------------------------------------------
export PATH="$FAKE_BIN:$PATH"
export LAUDACODE_VERSION=""
export REPO="Anon4You/Laudacode"
export PREFIX="$PREFIX_DIR/usr"

echo "=== case 1: latest-release resolution + full flow ==="
sh "$ROOT/../install.sh"

BIN="$PREFIX/bin/laudacode"
[ -x "$BIN" ] || { echo "✗ FAIL: binary not installed at $BIN" >&2; exit 1; }
OUT="$("$BIN")"
[ "$OUT" = "laudacode 9.9.9 (test stub)" ] || { echo "✗ FAIL: stub output '$OUT'" >&2; exit 1; }
[ -w "$(dirname "$BIN")" ] && OWNER_OK=yes
echo "✔ binary installed and executable"

[ ! -d "$TMPDIR/laudacode-build" ] && echo "✔ build dir cleaned up" \
    || { echo "✗ FAIL: build dir left behind" >&2; exit 1; }

echo "=== case 2: explicit version bypasses resolution ==="
rm -f "$BIN"
LAUDACODE_VERSION=v9.9.9 sh "$ROOT/../install.sh" >/dev/null
[ -x "$BIN" ] && echo "✔ pinned version installs" || { echo "✗ FAIL" >&2; exit 1; }

echo "=== case 3: missing release fails cleanly ==="
if REPO="Anon4You/Nope" sh "$ROOT/../install.sh" >/dev/null 2>"$TEST_DIR/err.txt"; then
    echo "✗ FAIL: should have exited non-zero" >&2; exit 1
fi
grep -q "could not get latest version" "$TEST_DIR/err.txt" \
    && echo "✔ clean failure message shown" \
    || { echo "✗ FAIL: wrong error: $(cat "$TEST_DIR/err.txt")" >&2; exit 1; }

echo "=== case 4: corrupted archive is rejected with a clear error ==="
printf 'this is not gzip at all' \
    > "$SRV_ROOT/Anon4You/Laudacode/archive/refs/tags/v9.9.9.tar.gz"
if LAUDACODE_VERSION=v9.9.9 sh "$ROOT/../install.sh" >/dev/null 2>"$TEST_DIR/err2.txt"; then
    echo "✗ FAIL: should have exited non-zero" >&2; exit 1
fi
grep -q "not a valid tar.gz" "$TEST_DIR/err2.txt" \
    && echo "✔ archive validation caught corruption" \
    || { echo "✗ FAIL: $(cat "$TEST_DIR/err2.txt")" >&2; exit 1; }

echo
echo "ALL INSTALL TESTS PASSED"

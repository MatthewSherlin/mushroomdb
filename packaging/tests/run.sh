#!/bin/sh
# Local simulation: serve a fake GitHub Release asset and drive install.sh + npm postinstall.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
PKG="$ROOT/packaging"
NPM="$PKG/npm"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "${os}-${arch}" in
  darwin-arm64|darwin-aarch64) TARGET=aarch64-apple-darwin ;;
  darwin-x64|darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  linux-x64|linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  linux-arm64|linux-aarch64) TARGET=aarch64-unknown-linux-gnu ;;
  *)
    echo "host platform ${os}-${arch} is not one of the four release targets" >&2
    exit 1
    ;;
esac

VERSION=0.1.0
TAG=v${VERSION}
ASSET="mushroomdb-${TAG}-${TARGET}.tar.gz"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"; if [ -n "${SERVER_PID:-}" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi' EXIT

mkdir -p "$WORKDIR/rel"
{
  echo "#!/bin/sh"
  echo "echo fake-ok \"\$@\""
} > "$WORKDIR/rel/mushroomdb"
chmod +x "$WORKDIR/rel/mushroomdb"
tar -C "$WORKDIR/rel" -czf "$WORKDIR/rel/${ASSET}" mushroomdb

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$WORKDIR/rel" && sha256sum "$ASSET" > SHA256SUMS)
else
  (cd "$WORKDIR/rel" && shasum -a 256 "$ASSET" > SHA256SUMS)
fi

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$WORKDIR/rel" >/dev/null 2>&1 &
SERVER_PID=$!

ok=0
i=0
while [ "$i" -lt 50 ]; do
  if curl -sf "http://127.0.0.1:${PORT}/SHA256SUMS" >/dev/null; then
    ok=1
    break
  fi
  i=$((i + 1))
  sleep 0.05
done
if [ "$ok" != 1 ]; then
  echo "fake release server did not start on :${PORT}" >&2
  exit 1
fi

BASE="http://127.0.0.1:${PORT}"
INSTALL_DIR="$WORKDIR/prefix/bin"
mkdir -p "$INSTALL_DIR"

echo "== install.sh happy path"
MUSHROOMDB_VERSION="$VERSION" \
  MUSHROOMDB_RELEASE_BASE="$BASE" \
  MUSHROOMDB_INSTALL_DIR="$INSTALL_DIR" \
  sh "$PKG/install.sh" | tee "$WORKDIR/install-out"
test -x "${INSTALL_DIR}/mushroomdb"
out=$("${INSTALL_DIR}/mushroomdb" --help)
test "$out" = "fake-ok --help"
grep -q "installed ${INSTALL_DIR}/mushroomdb" "$WORKDIR/install-out"
grep -q "target:  ${TARGET}" "$WORKDIR/install-out"

echo "== install.sh unsupported platform"
set +e
MUSHROOMDB_VERSION="$VERSION" \
  MUSHROOMDB_RELEASE_BASE="$BASE" \
  MUSHROOMDB_FORCE_OS=win32 \
  MUSHROOMDB_FORCE_ARCH=x64 \
  sh "$PKG/install.sh" >"$WORKDIR/bad-sh.out" 2>"$WORKDIR/bad-sh.err"
st=$?
set -e
test "$st" -ne 0
grep -q "unsupported platform: win32-x64" "$WORKDIR/bad-sh.err"
grep -q "darwin-arm64" "$WORKDIR/bad-sh.err"
grep -q "linux-x64" "$WORKDIR/bad-sh.err"

echo "== npm install.js happy path"
rm -rf "$NPM/vendor"
MUSHROOMDB_RELEASE_BASE="$BASE" node "$NPM/install.js" | tee "$WORKDIR/npm-out"
test -x "$NPM/vendor/mushroomdb"
out=$(node "$NPM/bin/mushroomdb.js" --help)
test "$out" = "fake-ok --help"
grep -q "installed " "$WORKDIR/npm-out"

echo "== npm install.js unsupported platform"
set +e
MUSHROOMDB_FORCE_OS=win32 MUSHROOMDB_FORCE_ARCH=x64 \
  node "$NPM/install.js" >"$WORKDIR/bad-npm.out" 2>"$WORKDIR/bad-npm.err"
st=$?
set -e
test "$st" -ne 0
grep -q "unsupported platform: win32-x64" "$WORKDIR/bad-npm.err"
grep -q "darwin-arm64" "$WORKDIR/bad-npm.err"

echo "== tampered asset: both installers refuse"
printf x >> "$WORKDIR/rel/${ASSET}"
TAMPER_DIR="$WORKDIR/prefix-tamper/bin"
mkdir -p "$TAMPER_DIR"
set +e
MUSHROOMDB_VERSION="$VERSION" \
  MUSHROOMDB_RELEASE_BASE="$BASE" \
  MUSHROOMDB_INSTALL_DIR="$TAMPER_DIR" \
  sh "$PKG/install.sh" >"$WORKDIR/tamper-sh.out" 2>"$WORKDIR/tamper-sh.err"
st=$?
set -e
test "$st" -ne 0
grep -qi checksum "$WORKDIR/tamper-sh.err"
test ! -e "${TAMPER_DIR}/mushroomdb"

rm -rf "$NPM/vendor"
set +e
MUSHROOMDB_RELEASE_BASE="$BASE" \
  node "$NPM/install.js" >"$WORKDIR/tamper-npm.out" 2>"$WORKDIR/tamper-npm.err"
st=$?
set -e
test "$st" -ne 0
grep -qi checksum "$WORKDIR/tamper-npm.err"
test ! -e "$NPM/vendor/mushroomdb"

echo "== homebrew render.sh fills real sha256s"
FAKE_SUMS="$WORKDIR/SHA256SUMS-four"
{
  echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  mushroomdb-${TAG}-aarch64-apple-darwin.tar.gz"
  echo "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  mushroomdb-${TAG}-x86_64-apple-darwin.tar.gz"
  echo "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc  mushroomdb-${TAG}-aarch64-unknown-linux-gnu.tar.gz"
  echo "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd  mushroomdb-${TAG}-x86_64-unknown-linux-gnu.tar.gz"
} > "$FAKE_SUMS"
sh "$PKG/homebrew/render.sh" "$VERSION" "$FAKE_SUMS" "$WORKDIR/mushroomdb.rb"
grep -q 'sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"' "$WORKDIR/mushroomdb.rb"
grep -q 'sha256 "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"' "$WORKDIR/mushroomdb.rb"
grep -qv PUT_SHA256 "$WORKDIR/mushroomdb.rb"

echo "== npm pack excludes the binary"
(cd "$NPM" && npm pack --pack-destination "$WORKDIR" >/dev/null)
TAR=$(ls "$WORKDIR"/mushroomdb-*.tgz)
if tar -tzf "$TAR" | grep -q vendor/mushroomdb; then
  echo "npm pack bundled vendor/mushroomdb — the binary must stay out of the tarball" >&2
  exit 1
fi
tar -tzf "$TAR" | grep -q install.js
tar -tzf "$TAR" | grep -q bin/mushroomdb.js

rm -rf "$NPM/vendor"

echo "packaging tests ok"

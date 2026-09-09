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
  linux-x64|linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  linux-arm64|linux-aarch64) TARGET=aarch64-unknown-linux-gnu ;;
  *)
    echo "host platform ${os}-${arch} is not one of the three release targets" >&2
    exit 1
    ;;
esac

# The npm install.js happy-path check below asks for whatever version is in
# packaging/npm/package.json (it has no MUSHROOMDB_VERSION override, same as
# a real `npm install`), so the fake release this script serves has to be
# built and tagged for that same version — a hardcoded VERSION here silently
# drifts from package.json on every version bump and 404s that check, along
# with everything after it in this script.
VERSION=$(node -pe "require('$NPM/package.json').version")
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

# A package laid out the way npm leaves one: the real launcher script, and a
# fake native binary where postinstall would have put it. Built here rather
# than reusing the download step below, so these checks stand on their own.
FAKEPKG="$WORKDIR/pkg"
mkdir -p "$FAKEPKG/bin" "$FAKEPKG/vendor"
cp "$NPM/bin/mushroomdb.js" "$FAKEPKG/bin/mushroomdb.js"
cp "$WORKDIR/rel/mushroomdb" "$FAKEPKG/vendor/mushroomdb"
chmod +x "$FAKEPKG/vendor/mushroomdb"
# Canonical (symlink-resolved) paths: Node resolves a module's real path, so
# that is what the launcher prints, and on macOS $TMPDIR is behind /var -> /private/var.
FAKE_LAUNCHER="$(cd "$FAKEPKG/bin" && pwd -P)/mushroomdb.js"
FAKE_BINARY="$(cd "$FAKEPKG/vendor" && pwd -P)/mushroomdb"

echo "== launcher --print-binary / --print-launcher"
# Both answer with an absolute path that is really there. `install` and the
# plugin's hooks/run.sh ask these once, so no hook has to spawn npx.
out=$(node "$FAKE_LAUNCHER" --print-binary)
case "$out" in /*) ;; *) echo "--print-binary must print an absolute path, got: $out" >&2; exit 1 ;; esac
test "$out" = "$FAKE_BINARY"
test -x "$out"
out=$(node "$FAKE_LAUNCHER" --print-launcher)
test "$out" = "$FAKE_LAUNCHER"
test -f "$out"

echo "== launcher --print-binary exits 1 with no vendored binary"
# The caller has to be able to tell "no binary" from a path, so it can fall
# through to the launcher rung.
mv "$FAKE_BINARY" "$WORKDIR/stashed-binary"
set +e
node "$FAKE_LAUNCHER" --print-binary >"$WORKDIR/nobin.out" 2>"$WORKDIR/nobin.err"
st=$?
set -e
test "$st" -eq 1
test ! -s "$WORKDIR/nobin.out"
grep -q "binary is missing" "$WORKDIR/nobin.err"
# --print-launcher still answers: where this file is stays true either way.
test "$(node "$FAKE_LAUNCHER" --print-launcher)" = "$FAKE_LAUNCHER"
mv "$WORKDIR/stashed-binary" "$FAKE_BINARY"

echo "== run_sh_prefers_the_cached_binary"
# With a cached binary path and no npx anywhere on PATH, run.sh must still
# work: it execs the cached binary and never reaches a resolution step.
CACHE_DIR="$WORKDIR/plugindata"
PLUGIN_VERSION=$(sed -n "s/^VERSION='\\(.*\\)'\$/\\1/p" "$PKG/plugin/hooks/run.sh")
test -n "$PLUGIN_VERSION"
mkdir -p "$CACHE_DIR"
printf '%s\n' "$FAKE_BINARY" > "$CACHE_DIR/binary-${PLUGIN_VERSION}"
# A launcher cache is there too, and must lose: the binary is the faster rung.
printf '%s\n' "$FAKE_LAUNCHER" > "$CACHE_DIR/launcher-${PLUGIN_VERSION}"
out=$(CLAUDE_PLUGIN_DATA="$CACHE_DIR" PATH=/usr/bin:/bin "$PKG/plugin/hooks/run.sh" --help)
test "$out" = "fake-ok --help"

echo "== run.sh: no CLAUDE_PLUGIN_DATA and no HOME means no cache at all"
# The cache may live under $CLAUDE_PLUGIN_DATA or $HOME and nowhere else. A
# world-writable fallback such as /tmp would let any local user drop in
# /tmp/.mushroomdb/binary-<version> naming a program of theirs, and the next
# UserPromptSubmit hook would exec it with the prompt payload on stdin. With
# neither variable set the script must resolve every time instead — which is
# observable: a stub npx that logs each call is asked twice for two runs, so
# nothing was remembered in between, and nothing was read either.
STUB="$WORKDIR/stub"
NPXLOG="$WORKDIR/npx-calls"
mkdir -p "$STUB"
: > "$NPXLOG"
{
  echo "#!/bin/sh"
  echo "echo call >> '$NPXLOG'"
  echo "for a in \"\$@\"; do"
  echo "  [ \"\$a\" = --print-binary ] && { printf '%s\\n' '$FAKE_BINARY'; exit 0; }"
  echo "done"
  echo "exit 1"
} > "$STUB/npx"
chmod +x "$STUB/npx"

out=$(env -u CLAUDE_PLUGIN_DATA -u HOME PATH="$STUB:/usr/bin:/bin" \
  "$PKG/plugin/hooks/run.sh" --help)
test "$out" = "fake-ok --help"
out=$(env -u CLAUDE_PLUGIN_DATA -u HOME PATH="$STUB:/usr/bin:/bin" \
  "$PKG/plugin/hooks/run.sh" --help)
test "$out" = "fake-ok --help"
calls=$(wc -l < "$NPXLOG" | tr -d ' ')
test "$calls" = 2 || {
  echo "expected 2 npx calls with no cache directory, got $calls" >&2
  exit 1
}

# With $HOME set it caches again, under $HOME: the second run asks nothing.
HOMEDIR="$WORKDIR/fakehome"
mkdir -p "$HOMEDIR"
: > "$NPXLOG"
out=$(env -u CLAUDE_PLUGIN_DATA HOME="$HOMEDIR" PATH="$STUB:/usr/bin:/bin" \
  "$PKG/plugin/hooks/run.sh" --help)
test "$out" = "fake-ok --help"
test "$(cat "$HOMEDIR/.mushroomdb/binary-${PLUGIN_VERSION}")" = "$FAKE_BINARY"
out=$(env -u CLAUDE_PLUGIN_DATA HOME="$HOMEDIR" PATH="$STUB:/usr/bin:/bin" \
  "$PKG/plugin/hooks/run.sh" --help)
test "$out" = "fake-ok --help"
calls=$(wc -l < "$NPXLOG" | tr -d ' ')
test "$calls" = 1 || {
  echo "expected 1 npx call with a cache under HOME, got $calls" >&2
  exit 1
}

echo "== run.sh: a cached binary that has gone falls back to the launcher"
# npm's cache can be pruned. The stale line must be skipped, not trusted.
printf '%s\n' "$WORKDIR/gone/mushroomdb" > "$CACHE_DIR/binary-${PLUGIN_VERSION}"
out=$(CLAUDE_PLUGIN_DATA="$CACHE_DIR" "$PKG/plugin/hooks/run.sh" --help)
test "$out" = "fake-ok --help"
rm -rf "$CACHE_DIR"

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
FAKE_SUMS="$WORKDIR/SHA256SUMS-three"
{
  echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  mushroomdb-${TAG}-aarch64-apple-darwin.tar.gz"
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

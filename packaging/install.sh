#!/bin/sh
# curl-able installer. No sudo. Writes ~/.local/bin/mushroomdb by default.
# Override: MUSHROOMDB_VERSION, MUSHROOMDB_RELEASE_BASE, MUSHROOMDB_INSTALL_DIR,
#           MUSHROOMDB_REPO, MUSHROOMDB_FORCE_OS, MUSHROOMDB_FORCE_ARCH.
set -eu

REPO="${MUSHROOMDB_REPO:-MatthewSherlin/mushroomdb}"
DEST="${MUSHROOMDB_INSTALL_DIR:-${HOME}/.local/bin}"
OS="${MUSHROOMDB_FORCE_OS:-}"
ARCH="${MUSHROOMDB_FORCE_ARCH:-}"

if [ -z "$OS" ]; then
  OS=$(uname -s | tr '[:upper:]' '[:lower:]')
fi
if [ -z "$ARCH" ]; then
  ARCH=$(uname -m)
fi

target_for() {
  os=$1
  arch=$2
  case "${os}-${arch}" in
    darwin-arm64|darwin-aarch64) echo aarch64-apple-darwin ;;
    darwin-x64|darwin-x86_64) echo x86_64-apple-darwin ;;
    linux-x64|linux-x86_64) echo x86_64-unknown-linux-gnu ;;
    linux-arm64|linux-aarch64) echo aarch64-unknown-linux-gnu ;;
    *) echo "" ;;
  esac
}

TARGET=$(target_for "$OS" "$ARCH")
if [ -z "$TARGET" ]; then
  echo "unsupported platform: ${OS}-${ARCH}" >&2
  echo "supported: darwin-arm64, darwin-x64, linux-x64, linux-arm64" >&2
  exit 1
fi

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "install.sh needs $1 on PATH" >&2
    exit 1
  fi
}

need_cmd curl
need_cmd tar
need_cmd mktemp

VERSION="${MUSHROOMDB_VERSION:-}"
if [ -n "$VERSION" ]; then
  VERSION="${VERSION#v}"
  TAG="v${VERSION}"
elif [ -n "${MUSHROOMDB_RELEASE_BASE:-}" ]; then
  echo "MUSHROOMDB_VERSION is required when MUSHROOMDB_RELEASE_BASE is set" >&2
  exit 1
else
  LATEST_URL=$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")
  TAG=${LATEST_URL##*/}
  VERSION="${TAG#v}"
  if [ -z "$TAG" ] || [ "$TAG" = "latest" ]; then
    echo "could not resolve latest GitHub release for ${REPO}" >&2
    exit 1
  fi
fi

BASE="${MUSHROOMDB_RELEASE_BASE:-https://github.com/${REPO}/releases/download/${TAG}}"
BASE=${BASE%/}
ASSET="mushroomdb-${TAG}-${TARGET}.tar.gz"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

echo "downloading ${ASSET}"
curl -fsSL "${BASE}/${ASSET}" -o "${WORKDIR}/${ASSET}"
curl -fsSL "${BASE}/SHA256SUMS" -o "${WORKDIR}/SHA256SUMS"

WANT=$(awk -v f="$ASSET" '$2 == f || $2 ~ "/"f"$" {print $1; exit}' "${WORKDIR}/SHA256SUMS")
if [ -z "$WANT" ]; then
  echo "SHA256SUMS has no entry for ${ASSET}" >&2
  exit 1
fi
GOT=$(file_sha256 "${WORKDIR}/${ASSET}")
if [ "$GOT" != "$WANT" ]; then
  echo "checksum mismatch for ${ASSET}: got ${GOT} want ${WANT}" >&2
  exit 1
fi

tar -xzf "${WORKDIR}/${ASSET}" -C "$WORKDIR"
if [ ! -f "${WORKDIR}/mushroomdb" ]; then
  echo "tarball ${ASSET} did not contain ./mushroomdb" >&2
  exit 1
fi

mkdir -p "$DEST"
cp "${WORKDIR}/mushroomdb" "${DEST}/mushroomdb"
chmod +x "${DEST}/mushroomdb"

echo "installed ${DEST}/mushroomdb"
echo "  version: ${VERSION}"
echo "  target:  ${TARGET}"
echo "  sha256:  ${GOT}"

#!/bin/sh
# Fill packaging/homebrew/mushroomdb.rb.in from a SHA256SUMS file.
# Usage: render.sh <version> <SHA256SUMS> <out.rb>
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: render.sh <version> <SHA256SUMS> <out.rb>" >&2
  exit 1
fi

VERSION=${1#v}
SUMS=$2
OUT=$3
TAG="v${VERSION}"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
IN="$ROOT/mushroomdb.rb.in"

sha_for() {
  target=$1
  f="mushroomdb-${TAG}-${target}.tar.gz"
  awk -v f="$f" '$2 == f || $2 ~ "/"f"$" {print tolower($1); exit}' "$SUMS"
}

need() {
  name=$1
  val=$2
  if [ -z "$val" ] || [ "${#val}" -ne 64 ]; then
    echo "render.sh: missing or invalid sha256 for ${name} in ${SUMS}" >&2
    exit 1
  fi
}

SHA_AARCH64_APPLE_DARWIN=$(sha_for aarch64-apple-darwin)
SHA_AARCH64_UNKNOWN_LINUX_GNU=$(sha_for aarch64-unknown-linux-gnu)
SHA_X86_64_UNKNOWN_LINUX_GNU=$(sha_for x86_64-unknown-linux-gnu)

need aarch64-apple-darwin "$SHA_AARCH64_APPLE_DARWIN"
need aarch64-unknown-linux-gnu "$SHA_AARCH64_UNKNOWN_LINUX_GNU"
need x86_64-unknown-linux-gnu "$SHA_X86_64_UNKNOWN_LINUX_GNU"

# portable in-place substitute without relying on GNU sed -i
escaped() {
  printf '%s' "$1" | sed 's/[&/\]/\\&/g'
}

sed \
  -e "s/__VERSION__/$(escaped "$VERSION")/g" \
  -e "s/__SHA_AARCH64_APPLE_DARWIN__/$(escaped "$SHA_AARCH64_APPLE_DARWIN")/g" \
  -e "s/__SHA_AARCH64_UNKNOWN_LINUX_GNU__/$(escaped "$SHA_AARCH64_UNKNOWN_LINUX_GNU")/g" \
  -e "s/__SHA_X86_64_UNKNOWN_LINUX_GNU__/$(escaped "$SHA_X86_64_UNKNOWN_LINUX_GNU")/g" \
  "$IN" > "$OUT"

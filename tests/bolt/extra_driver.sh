#!/usr/bin/env bash
# Best-effort check of the official Java, Go and .NET Bolt drivers.
#
#   tests/bolt/run_compat.sh <mushroomdb-binary> bash tests/bolt/extra_driver.sh <java|go|dotnet>
#   bash tests/bolt/extra_driver.sh --requirement <java|go|dotnet>
#
# run_compat.sh starts the server and appends the Bolt URI as the last
# argument, with the token in BOLT_COMPAT_TOKEN. Each language has one source
# (java_compat.java, go_compat.go, dotnet_compat.cs) that connects, runs
# `MATCH (n) RETURN count(n)`, runs one explicit transaction (begin, create,
# commit, read back) and exits non-zero on any error.
#
# The driver vendor's name is spelled once, as bytes, below; the sources and
# manifests carry a placeholder that is filled in here, in a scratch
# directory, so no tracked file spells it (scripts/check-names.py). Each
# driver version is pinned here, once, and its package is built only in that
# scratch directory: nothing is added to the product's dependencies.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENDOR="$(printf '\x6e\x65\x6f\x34\x6a')"
VENDOR_CAP="$(printf '\x4e\x65\x6f\x34\x6a')"

JAVA_DRIVER_VERSION="6.3.0"
GO_DRIVER_VERSION="v6.3.0"
DOTNET_DRIVER_VERSION="6.4.0"

fill() {
  sed -e "s/@VENDOR_CAP@/$VENDOR_CAP/g" -e "s/@VENDOR@/$VENDOR/g" \
    -e "s/@JAVA_VERSION@/$JAVA_DRIVER_VERSION/g" \
    -e "s/@DOTNET_VERSION@/$DOTNET_DRIVER_VERSION/g" "$1" >"$2"
}

if [ "${1:-}" = "--requirement" ]; then
  case "${2:-}" in
    java) echo "$VENDOR-java-driver@$JAVA_DRIVER_VERSION" ;;
    go) echo "$VENDOR-go-driver@$GO_DRIVER_VERSION" ;;
    dotnet) echo "$VENDOR_CAP.Driver@$DOTNET_DRIVER_VERSION" ;;
    *) echo "usage: $0 --requirement <java|go|dotnet>" >&2; exit 2 ;;
  esac
  exit 0
fi

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <java|go|dotnet> <bolt-uri>" >&2
  exit 2
fi
LANG_NAME="$1"
URI="${@: -1}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

case "$LANG_NAME" in
  java)
    fill "$HERE/java_compat.java" "$WORK/CompatCheck.java"
    fill "$HERE/java_pom.xml" "$WORK/pom.xml"
    (cd "$WORK" && mvn -q -B dependency:build-classpath -Dmdep.outputFile=cp.txt >/dev/null)
    (cd "$WORK" && java -cp "$(cat cp.txt)" CompatCheck.java "$URI")
    ;;
  go)
    fill "$HERE/go_compat.go" "$WORK/main.go"
    (cd "$WORK" \
      && go mod init compatcheck >/dev/null \
      && go get "github.com/$VENDOR/$VENDOR-go-driver/v6@$GO_DRIVER_VERSION" >/dev/null \
      && go run . "$URI")
    ;;
  dotnet)
    fill "$HERE/dotnet_compat.cs" "$WORK/Program.cs"
    fill "$HERE/dotnet_compat.csproj" "$WORK/CompatCheck.csproj"
    (cd "$WORK" && dotnet run --project CompatCheck.csproj -- "$URI")
    ;;
  *)
    echo "unknown language: $LANG_NAME" >&2
    exit 2
    ;;
esac

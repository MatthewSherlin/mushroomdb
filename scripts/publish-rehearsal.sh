#!/usr/bin/env bash
# publish-rehearsal.sh — package everything a `v*` tag publishes, and upload nothing.
#
# The first time the eight crates were ever packaged at a new version used to
# be the real `cargo publish`, in dependency order, each one irreversible. A
# mis-pinned sibling or a file missing from a package would be found after
# three crates were already on the index. This finds it before.
#
#   publish-rehearsal.sh           crates (verified build), wheel, both npm packages
#   publish-rehearsal.sh --quick   crates without the verify build (the wheel and npm steps still run)
#
# It packages the working tree (`--allow-dirty`): an untracked file inside a
# crate directory is packaged here and would be absent from a clean checkout.
#
# Every command here is a dry run or writes only build output and a temp
# directory. Nothing contacts a registry to upload. There is no flag that
# makes this script publish; publishing is a tag, and a person pushes it.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

QUICK=0
case "${1:-}" in
  "") ;;
  --quick) QUICK=1 ;;
  *) echo "usage: publish-rehearsal.sh [--quick]" >&2; exit 2 ;;
esac

WS="$(awk -F'"' '/^version = "/{print $2; exit}' Cargo.toml)"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT
fail=0
skipped=""
step() { printf '\n── %s ──\n' "$1"; }
bad()  { echo "publish-rehearsal.sh: FAILED — $1" >&2; fail=1; }

step "version sites agree on $WS"
bash scripts/bump-version.sh --check || bad "version sites disagree"
# Nothing below means anything on manifests that disagree, and all of it is
# slow: a release wheel build, two npm builds. Stop here.
if [[ $fail -ne 0 ]]; then
  echo "publish-rehearsal.sh: FAILED — fix the version sites first. Nothing was packaged or uploaded." >&2
  exit 1
fi

# The order publish.yml uses (.github/workflows/publish.yml, the crates.io job).
CRATES=(mushroomdb-storage mushroomdb-rules mushroomdb-query mushroomdb
        mushroomdb-arrow mushroomdb-server mushroomdb-extract mushroomdb-cli)

step "crates: package all eight together, siblings resolved from the workspace"
args=(publish --dry-run --workspace --exclude core-bench --exclude sim-harness --allow-dirty)
[[ $QUICK -eq 1 ]] && args+=(--no-verify)
if ! cargo "${args[@]}"; then
  bad "cargo publish --dry-run --workspace"
fi
for c in "${CRATES[@]}"; do
  if ! ls "target/package/${c}-${WS}.crate" >/dev/null 2>&1; then
    bad "no packaged tarball for ${c} ${WS}"
  fi
done

step "wheel: build as publish.yml does, and look inside"
if [[ -x bindings/python/.venv/bin/maturin ]]; then
  if (cd bindings/python && .venv/bin/maturin build --release --out "$OUT/wheel" >/dev/null); then
    # `|| true`: with no wheel `ls` fails, and under `set -e` that would end
    # the script here, before it can say what is missing.
    wheel="$(ls "$OUT"/wheel/mushroomdb-"${WS}"-*abi3*.whl 2>/dev/null | head -1 || true)"
    if [[ -z "$wheel" ]]; then
      bad "no abi3 wheel named mushroomdb-${WS}-…"
    else
      echo "built $(basename "$wheel")"
      listing="$(python3 -m zipfile -l "$wheel")"
      for want in "mushroomdb/__init__.pyi" "mushroomdb/py.typed"; do
        grep -q "$want" <<<"$listing" || bad "wheel is missing $want"
      done
    fi
  else
    bad "maturin build"
  fi
else
  echo "skipped: no bindings/python/.venv/bin/maturin"
  skipped="${skipped:+$skipped, }wheel"
fi

step "npm: pack both packages without publishing"
if command -v npm >/dev/null 2>&1; then
  for dir in packaging/npm clients/typescript; do
    name="$(node -p "require('./$dir/package.json').name")"
    ver="$(node -p "require('./$dir/package.json').version")"
    [[ "$ver" == "$WS" ]] || bad "$dir/package.json is $ver, workspace is $WS"
    if [[ "$dir" == clients/typescript ]]; then
      (cd "$dir" && npm ci >/dev/null 2>&1 && npm run build >/dev/null 2>&1) || bad "$name: build"
    fi
    (cd "$dir" && npm pack --dry-run >/dev/null 2>&1) && echo "packed $name@$ver (dry run)" \
      || bad "$name: npm pack --dry-run"
  done
else
  echo "skipped: npm is not installed"
  skipped="${skipped:+$skipped, }npm"
fi

echo
if [[ $fail -ne 0 ]]; then
  echo "publish-rehearsal.sh: FAILED — see above. Nothing was uploaded." >&2
  exit 1
fi
# A skipped step is not a packaged one, and the last line is the one quoted.
if [[ -n "$skipped" ]]; then
  echo "publish-rehearsal.sh: OK — $WS crates packaged; skipped: $skipped. Nothing was uploaded."
  exit 0
fi
echo "publish-rehearsal.sh: OK — $WS packages cleanly. Nothing was uploaded."

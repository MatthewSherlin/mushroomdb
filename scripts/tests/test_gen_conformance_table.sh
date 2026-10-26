#!/usr/bin/env bash
# The conformance-table gate fails on what it guards: an empty runner output
# and a stale committed page; it passes on a real table.
set -euo pipefail
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
GEN="$ROOT/scripts/gen-conformance-table.sh"
dir="$(mktemp -d)"; trap 'rm -rf "$dir"' EXIT
export CONFORMANCE_TARGET="$dir/page.md"

# A runner that writes nothing.
printf '#!/usr/bin/env bash\n: > "$2"\n' > "$dir/empty-runner"
# A runner that writes a minimal table.
printf '#!/usr/bin/env bash\nprintf "| Function | Supported |\\n|---|---|\\n| abs | yes |\\n" > "$2"\n' > "$dir/table-runner"
chmod +x "$dir/empty-runner" "$dir/table-runner"

if TCK_RUNNER="$dir/empty-runner" bash "$GEN" 2>"$dir/err"; then
  echo "FAIL: an empty runner output passed" >&2; exit 1
fi
grep -q "tck-runner produced no output" "$dir/err" || { echo "FAIL: no error message" >&2; exit 1; }
[[ ! -e "$CONFORMANCE_TARGET" ]] || { echo "FAIL: an empty output was written" >&2; exit 1; }

TCK_RUNNER="$dir/table-runner" bash "$GEN" >/dev/null
[[ -s "$CONFORMANCE_TARGET" ]] || { echo "FAIL: no page written" >&2; exit 1; }
TCK_RUNNER="$dir/table-runner" bash "$GEN" --check >/dev/null

# A stale page fails --check and names the command that regenerates it.
echo "| \`old\` | no |" >> "$CONFORMANCE_TARGET"
if TCK_RUNNER="$dir/table-runner" bash "$GEN" --check 2>"$dir/err" >/dev/null; then
  echo "FAIL: a stale page passed --check" >&2; exit 1
fi
grep -q "bash scripts/gen-conformance-table.sh" "$dir/err" || { echo "FAIL: --check does not name the regenerate command" >&2; exit 1; }
echo "ok"

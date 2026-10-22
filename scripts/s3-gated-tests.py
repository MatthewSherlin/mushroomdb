#!/usr/bin/env python3
"""The S3-gated tests, read from source, and a check that every one of them ran.

A test is S3-gated when its body reads MUSHROOMDB_TEST_S3_ENDPOINT, directly or
through its file's gate helper. Without the variable such a test returns early
and passes, so CI (the hosting-templates workflow's s3-backup job) must see
each one pass with the variable set:

    python3 scripts/s3-gated-tests.py                 # list them
    python3 scripts/s3-gated-tests.py --check-log LOG # LOG: cargo test output

The check fails when:
- a file under crates/ reads MUSHROOMDB_TEST_S3_ENDPOINT but is not listed
  in GATED_FILES (the job would not run it);
- a gated test has no `test <name> ... ok` line in LOG;
- LOG holds a skip line ("skipping the S3 test").

Adding a gated test: call the file's gate helper (GATED_FILES below) at its
top, `let Some(..) = <helper> else { return };`. The helper prints the skip
line, and this script finds the test by that call. Adding a gated file: list
it in GATED_FILES with its helper, and run its target in the s3-backup job.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ENV_VAR = "MUSHROOMDB_TEST_S3_ENDPOINT"
SKIP_LINE = "skipping the S3 test"

# File -> the calls that mark a test in it as S3-gated (besides ENV_VAR itself).
GATED_FILES: dict[str, tuple[str, ...]] = {
    "crates/backup/tests/minio.rs": ("bucket()", "bucket_with("),
    "crates/cli/tests/serve_backup.rs": ("S3Env::from_env()",),
}

TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test\b")
FN_NAME = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)")


def tests_in(text: str) -> list[tuple[str, str]]:
    """(name, body) for each top-level test function. rustfmt closes a
    top-level fn with a `}` in column 0, which ends the body."""
    lines = text.splitlines()
    starts = [i for i, line in enumerate(lines) if TEST_ATTR.match(line)]
    found = []
    for start in starts:
        name = None
        for i in range(start, len(lines)):
            m = FN_NAME.match(lines[i])
            if m:
                name = m.group(1)
                end = next((j for j in range(i, len(lines)) if lines[j] == "}"), len(lines))
                body = "\n".join(lines[i : end + 1])
                break
        if name is None:
            raise SystemExit(f"s3-gated-tests: no fn after the test attribute at line {start + 1}")
        found.append((name, body))
    return found


def gated_tests() -> list[str]:
    names = []
    for rel, helpers in GATED_FILES.items():
        text = (ROOT / rel).read_text()
        for name, body in tests_in(text):
            if ENV_VAR in body or any(h in body for h in helpers):
                names.append(name)
    return names


def unlisted_files() -> list[str]:
    out = []
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        if rel not in GATED_FILES and ENV_VAR in path.read_text(errors="replace"):
            out.append(rel)
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check-log", type=Path)
    args = parser.parse_args()

    names = gated_tests()
    errors = [
        f"{rel} reads {ENV_VAR} but is not in GATED_FILES, so the s3-backup job does not run it"
        for rel in unlisted_files()
    ]
    if not names:
        errors.append("no S3-gated tests found in source")
    if args.check_log is None:
        print("\n".join(names))
    else:
        log = args.check_log.read_text(errors="replace")
        skips = [line for line in log.splitlines() if SKIP_LINE in line]
        if skips:
            errors.append(f"{len(skips)} skip line(s) in the log, e.g. {skips[0].strip()!r}")
        passed = set(re.findall(r"^test (\S+) \.\.\. ok$", log, re.M))
        missing = [n for n in names if n not in passed]
        for n in missing:
            errors.append(f"S3-gated test {n} did not pass in the log")
        print(f"s3-gated-tests: {len(names) - len(missing)} of {len(names)} S3-gated tests passed")
    for e in errors:
        print(f"s3-gated-tests: ERROR: {e}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())

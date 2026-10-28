#!/usr/bin/env python3
"""Check explicit data-safety claim references against workflow job definitions.

Only this page, five claim IDs, two local workflows, and ordinary block-style
jobs mappings are supported. This checks existence, not execution or outcomes.
"""

import argparse
from pathlib import Path
import re
import sys

CLAIMS = {
    "crash-recovery", "format-compatibility", "acknowledged-writes",
    "backup-restore", "snapshot-interruption",
}
WORKFLOWS = {
    ".github/workflows/ci.yml", ".github/workflows/hosting-templates.yml",
}
REFERENCE = re.compile(
    r"<!-- ci-evidence: ([a-z-]+) ([^\s#]+)#([A-Za-z_][A-Za-z0-9_-]*) -->"
)
JOB = re.compile(r"  ([A-Za-z_][A-Za-z0-9_-]*):\s*(?:#.*)?")


def job_ids(text: str) -> set[str]:
    """Read only real indent-two job keys beneath a column-zero jobs: key."""
    inside = False
    found: set[str] = set()
    for line in text.splitlines():
        if line.strip() == "jobs:" and not line.startswith((" ", "\t")):
            if inside:
                raise ValueError("duplicate jobs mapping")
            inside = True
            continue
        if not inside or not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line[0].isspace():
            break
        match = JOB.fullmatch(line)
        if match:
            job = match.group(1)
            if job in found:
                raise ValueError(f"duplicate job definition: {job}")
            found.add(job)
        elif line.startswith("  ") and not line[2:3].isspace():
            raise ValueError("unsupported job definition; expected an indent-two block key")
    if not found:
        raise ValueError("no supported jobs mapping found")
    return found


def validate(root: Path) -> int:
    page = root / "docs/site/data-safety.md"
    claims: set[str] = set()
    references: set[tuple[str, str, str]] = set()
    workflows: dict[str, set[str]] = {}
    for number, line in enumerate(page.read_text().splitlines(), 1):
        if "ci-evidence:" not in line:
            continue
        match = REFERENCE.fullmatch(line.strip())
        if not match:
            raise ValueError(f"{page}:{number}: malformed ci-evidence reference")
        claim, workflow, job = match.groups()
        if claim not in CLAIMS or workflow not in WORKFLOWS:
            raise ValueError(f"{page}:{number}: unknown claim or workflow")
        reference = claim, workflow, job
        if reference in references:
            raise ValueError(f"{page}:{number}: duplicate ci-evidence reference")
        references.add(reference)
        claims.add(claim)
        if workflow not in workflows:
            workflows[workflow] = job_ids((root / workflow).read_text())
        if job not in workflows[workflow]:
            raise ValueError(f"{page}:{number}: missing job {workflow}#{job}")
    if claims != CLAIMS:
        raise ValueError(f"missing evidence claims: {', '.join(sorted(CLAIMS - claims))}")
    return len(references)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = parser.parse_args()
    try:
        count = validate(args.root)
    except (OSError, ValueError) as error:
        print(f"check-safety-evidence.py: {error}", file=sys.stderr)
        return 1
    print(f"check-safety-evidence.py: OK ({len(CLAIMS)} claims, {count} job references; existence only)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

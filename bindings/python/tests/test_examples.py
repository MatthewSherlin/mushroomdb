"""The scripts in `examples/` run, and say what the README says they say."""

from __future__ import annotations

import pathlib
import subprocess
import sys

EXAMPLES = pathlib.Path(__file__).resolve().parents[3] / "examples"


def run(script: str) -> str:
    result = subprocess.run(
        [sys.executable, str(EXAMPLES / script)],
        capture_output=True,
        text=True,
        timeout=60,
        check=True,
    )
    return result.stdout


def answer_keys(section: str) -> list[str]:
    return [line.split("]")[0].strip(" [") for line in section.splitlines() if line.startswith("  [")]


def test_permissioned_assistant_answers_each_role_with_what_it_may_see():
    support, finance, after_revoke = run(
        "permissioned-assistant/permissioned_assistant.py"
    ).split("asks:")[1:]

    assert answer_keys(support) == ["postmortem", "runbook"]
    assert answer_keys(finance) == ["contract-dispute", "postmortem", "runbook"]
    assert answer_keys(after_revoke) == ["runbook"]


def test_permissioned_assistant_never_shows_support_the_restricted_text():
    output = run("permissioned-assistant/permissioned_assistant.py")
    support_answers = output.split("finance asks:")[0] + output.split("Revoke")[1]

    assert "liability cap" not in support_answers
    assert "node_info('contract-dispute') -> None" in output


def test_team_matching_moves_a_match_in_the_same_write():
    output = run("team-matching/team_matching.py")
    before, after = output.split("After the write:")

    assert "ana -> search-rewrite" in before
    assert "would gain ['dashboard']" in before
    assert "would lose ['search-rewrite']" in before
    assert "ana -> dashboard" in after
    assert "ana -> search-rewrite" not in after

#!/usr/bin/env python3
"""Compute ground truth for the agent benchmark from git + grep only.

Never reads the graph store. Writes tasks.json next to this file (or --out).

R1 is this repository; R2 is the second subject (see `truth_r2.py`), whose
tasks are appended when `--repo2` names a clone of it.

Usage: python3 ground_truth.py --repo <repo-dir> [--repo2 <dir>] [--out tasks.json]
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import re
import subprocess
import sys
from pathlib import Path

PREFIX = (
    "Answer using only this repository. "
    "Be concise: give the facts asked for, one per line, nothing else."
)

# Change-and-pass tasks ask for edits, not facts, so they carry their own
# preamble. The test command is named in the prompt on purpose: every arm may
# run it, so no arm is guessing at what "done" means.
CHANGE_PREFIX = (
    "Work in this repository. Make the change described below, then run the "
    "test command it names and make sure it passes. Leave the edits in the "
    "working tree: do not commit, do not stash, do not revert."
)


def git(repo: Path, *args: str) -> str:
    out = subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=True
    )
    return out.stdout


def rs_files(repo: Path) -> list[Path]:
    """Every .rs file under crates/, relative to repo root, sorted."""
    found = []
    for dirpath, dirnames, filenames in os.walk(repo / "crates"):
        dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
        for fn in filenames:
            if fn.endswith(".rs"):
                found.append(Path(dirpath, fn).relative_to(repo))
    return sorted(found, key=str)


def src_rs_files(repo: Path) -> list[Path]:
    """Every .rs file under a `crates/*/src/` tree: the workspace's own code.

    Fixture code under `crates/*/tests/` is deliberately out. Those trees
    write miniature repositories that define their own `sanitize`, `render`
    and friends, and counting them as call sites of the workspace symbol is
    exactly the 0.6.1 grader defect the spec calls out (§3.2).
    """
    return [f for f in rs_files(repo) if "/src/" in str(f)]


def read(repo: Path, rel: Path | str) -> str:
    return (repo / rel).read_text(encoding="utf-8", errors="replace")


def line_of(repo: Path, rel: str, pattern: str) -> int:
    """1-based line number of the first line matching `pattern`. Must exist."""
    pat = re.compile(pattern)
    for i, line in enumerate(read(repo, rel).splitlines(), start=1):
        if pat.search(line):
            return i
    raise AssertionError(f"{pattern!r} not found in {rel}")


FN_DEF = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)")


def calls_by_function(text: str, name: str) -> list[str]:
    """Names of the functions in `text` that call `name`, in order.

    The enclosing function is the nearest `fn` header above the call, which is
    how a reader would answer it; the definition's own header is skipped so a
    recursive call does not name itself.
    """
    found: list[str] = []
    current: str | None = None
    call = re.compile(r"\b" + re.escape(name) + r"\s*\(")
    for line in text.splitlines():
        m = FN_DEF.match(line)
        if m:
            current = m.group(1)
            if m.group(1) == name:
                continue
        if current and current != name and call.search(line):
            if current not in found:
                found.append(current)
    return found


# --------------------------------------------------------------------------
# commit parsing
# --------------------------------------------------------------------------

MARK = "__C__"


def commits_with_files(repo: Path, n: int) -> list[tuple[str, list[str]]]:
    """The last n commits as (short_sha, files). Merge commits list no files."""
    raw = git(repo, "log", "-n", str(n), "--name-only", f"--pretty=format:{MARK}%h")
    out: list[tuple[str, list[str]]] = []
    cur_sha = None
    cur_files: list[str] = []
    for line in raw.splitlines():
        if line.startswith(MARK):
            if cur_sha is not None:
                out.append((cur_sha, cur_files))
            cur_sha = line[len(MARK):].strip()
            cur_files = []
        elif line.strip():
            cur_files.append(line.strip())
    if cur_sha is not None:
        out.append((cur_sha, cur_files))
    return out


def name_variants(repo: Path, email: str) -> list[str]:
    """Every git author name recorded repo-wide for one email, commonest first."""
    counts: collections.Counter[str] = collections.Counter()
    for line in git(repo, "log", "--format=%an%x1f%ae").splitlines():
        if not line.strip():
            continue
        name, mail = line.split("\x1f", 1)
        if mail.strip().lower() == email.lower():
            counts[name.strip()] += 1
    return [n for n, _ in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))]


def top_author(repo: Path, path: str) -> tuple[list[str], str, int]:
    """The identity that has written `path` most: (name variants, email, count).

    The identity is the email — one person commits under several names — and
    any name recorded for that email counts as naming them.
    """
    by_email: collections.Counter[str] = collections.Counter()
    for line in git(repo, "log", "--format=%an%x1f%ae", "--", path).splitlines():
        if line.strip():
            _name, mail = line.split("\x1f", 1)
            by_email[mail.strip()] += 1
    email, count = max(by_email.items(), key=lambda kv: (kv[1], kv[0]))
    return name_variants(repo, email), email, count


def history_task(repo: Path, *, task_id: int, key: str, repo_key: str,
                 path: str, window: int = 300, top_n: int = 5) -> dict:
    """Co-change, then ownership and the shared commits of what it found.

    The §3.2 history kind — "who owns `path`, and which files change with it" —
    with the halves chained. The last-shared-commit facts are the reason this
    is not one `git log`: the log can say "either of these two files", never
    "both", so each pair has to be intersected on its own.
    """
    log = commits_with_files(repo, window)
    counts: collections.Counter[str] = collections.Counter()
    for _sha, files in log:
        if path in files:
            for f in files:
                if f != path:
                    counts[f] += 1
    ranked = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))
    top = ranked[:top_n]
    assert len(top) == top_n, f"{path} has too few co-change partners"
    assert top[0][1] > top[1][1], f"{path}'s closest partner is a tie: {top}"

    # Files tied with the last place asked for are equally right answers;
    # naming some of them and marking the rest wrong would grade a coin toss.
    # The certain places are named; the contested ones are an any_of.
    cutoff = top[-1][1]
    certain = [p for p, c in ranked if c > cutoff]
    contested = [p for p, c in ranked if c == cutoff]
    partner_checks: list[dict] = [{"kind": "text", "value": p} for p in certain]
    if len(certain) < top_n:
        partner_checks.append({"kind": "any_of", "values": contested,
                               "min": top_n - len(certain)})

    def last_shared(other: str) -> str:
        sha = next((s for s, files in log if path in files and other in files), None)
        assert sha, f"no commit in the window touches both {path} and {other}"
        return sha

    partner = top[0][0]
    variants, email, commits = top_author(repo, partner)
    shared = [last_shared(p) for p, _ in top[:3]]

    return {
        "id": task_id,
        "key": key,
        "repo": repo_key,
        "kind": "history",
        "prompt": (
            f"Over the last {window} commits, which {top_n} files change in the "
            f"same commit as `{path}` most often? Give the {top_n} paths. Then, "
            "for the three it changes with most often: the short SHA of the most "
            f"recent commit that touched that file and `{path}` together — one "
            "SHA each — and, for the closest one of all, who has written it most, "
            "counted by commits that touch it."
        ),
        "truth": {
            f"top{top_n}": [{"path": p, "commits": c} for p, c in top],
            "tied_for_last": contested if len(certain) < top_n else [],
            "runner_up": ranked[top_n:top_n + 3],
            "partner": partner,
            "partner_author": variants[0],
            "partner_email": email,
            "partner_commits": commits,
            "partner_name_variants": variants,
            "last_shared_shas": shared,
        },
        "checks": [
            *partner_checks,
            {"kind": "author", "values": variants, "email": email},
            *[{"kind": "sha", "value": s} for s in shared],
        ],
        "extras": {"kind": "sha", "allowed": shared},
    }


# --------------------------------------------------------------------------
# R1 tasks — this repository
# --------------------------------------------------------------------------

TARGET_INSTALL = "crates/cli/src/install.rs"
TARGET_DB = "crates/core-api/src/db.rs"
TARGET_RULES_DEF = "crates/core-rules/src/def.rs"
TARGET_RULES_ENGINE = "crates/core-rules/src/engine.rs"
TARGET_RENDER = "crates/core-api/src/repograph/render.rs"
TARGET_IMPACT = "crates/core-api/src/repograph/impact.rs"
TARGET_HTTP = "crates/server/src/http.rs"
TARGET_MCP = "crates/server/src/mcp.rs"
TARGET_CLI_LIB = "crates/cli/src/lib.rs"
TARGET_CLI_MAIN = "crates/cli/src/main.rs"


def r1_change_1(repo: Path) -> dict:
    """Rename `cap_lines`. Truth: every workspace file that calls it."""
    files = sorted(git(repo, "grep", "-l", "cap_lines(", "--", "crates").split())
    assert TARGET_RENDER in files, "cap_lines is not where the task expects it"
    return {
        "id": 1,
        "key": "r1-change-1",
        "repo": "R1",
        "kind": "change",
        "prompt": (
            f"`pub fn cap_lines` in `{TARGET_RENDER}` reads as if it capped the "
            "length of a line; it caps how many lines a string keeps. Rename it "
            "to `cap_line_count` everywhere in this workspace: the definition, "
            "every call site, the doc-comment links that name it, and its own "
            "unit tests. Nothing else about its behaviour changes.\n\n"
            "Test command: `cargo test -p mushroomdb --lib repograph::render`"
        ),
        "truth": {"files": files},
        "checks": [],
        "extras": {"kind": "none"},
        "verify": {"cmd": ["cargo", "test", "-p", "mushroomdb", "--lib",
                           "repograph::render"]},
    }


def r1_change_2(repo: Path) -> dict:
    """Add `--json` to `owners`, mirroring `map --json`.

    The parser table the verify command runs is a unit test inside
    `crates/cli/src/lib.rs`, so the truth set is the parser plus the binary's
    dispatch: `crates/cli/tests/` holds no parser test to extend.
    """
    files = [TARGET_CLI_LIB, TARGET_CLI_MAIN]
    for f in files:
        assert (repo / f).exists(), f
    assert "--json" in read(repo, TARGET_CLI_LIB), "map --json is gone"
    return {
        "id": 2,
        "key": "r1-change-2",
        "repo": "R1",
        "kind": "change",
        "prompt": (
            "This crate's `map` subcommand takes a `--json` flag that prints the "
            "computed map instead of the rendered digest. The `owners` "
            "subcommand has no such flag. Add `--json` to `owners` the same way "
            "`map` does it: parse the "
            "flag into the `Owners` command, dispatch it in the binary, print the "
            "JSON form of what the owners report holds, name the flag in the "
            "usage text, and extend the `parse_args_table` unit test with the new "
            "spellings (flag before and after the positional arguments).\n\n"
            "Test command: `cargo test -p mushroomdb-cli --lib parse_args_table`"
        ),
        "truth": {"files": files},
        "checks": [],
        "extras": {"kind": "none"},
        "verify": {"cmd": ["cargo", "test", "-p", "mushroomdb-cli", "--lib",
                           "parse_args_table"]},
    }


def r1_change_3(repo: Path) -> dict:
    """Make `MAX_IMPACT_FILES` an `ImpactOptions` field.

    Truth: where the options live, plus every `crates/*/src/` file that names
    `render_impact`. A `mod.rs` only re-exports it, so it is not a caller and
    a signature change never reaches it.
    """
    files = {TARGET_IMPACT}
    for rel in src_rs_files(repo):
        if rel.name == "mod.rs":
            continue
        if "render_impact" in read(repo, rel):
            files.add(str(rel))
    assert TARGET_RENDER in files and len(files) >= 3, sorted(files)
    assert "MAX_IMPACT_FILES" in read(repo, TARGET_RENDER)
    return {
        "id": 3,
        "key": "r1-change-3",
        "repo": "R1",
        "kind": "change",
        "prompt": (
            f"`render_impact` in `{TARGET_RENDER}` caps how many files it prints "
            "at the private constant `MAX_IMPACT_FILES`. Make that cap a setting: "
            f"add a `max_files` field to `ImpactOptions` in `{TARGET_IMPACT}`, "
            "defaulting to the value the constant holds today; change "
            "`render_impact` to take the options by reference and use "
            "`max_files` in place of the constant; update every caller in the "
            "workspace to pass the options it already has (or the default).\n\n"
            "Test command: `cargo test -p mushroomdb-server --test mcp impact`"
        ),
        "truth": {"files": sorted(files)},
        "checks": [],
        "extras": {"kind": "none"},
        "verify": {"cmd": ["cargo", "test", "-p", "mushroomdb-server", "--test",
                           "mcp", "impact"]},
    }


SANITIZE_IMPORT = re.compile(
    r"use\s+(?:crate|core_api)::repograph::(?:render::)?"
    r"(?:\{[^}]*\bsanitize\b[^}]*\}|sanitize\b)",
    re.S,
)


def r1_blast_1(repo: Path) -> dict:
    """Blast radius of `render::sanitize`.

    A file is in it when it imports *this* `sanitize` — `crate::repograph::
    render::sanitize` or the `core_api::repograph::{...}` re-export — and does
    not define one of its own. Three other places in the workspace define a
    same-named function, and the test fixtures define a fourth; none of them
    is reached by a signature change here.
    """
    hits = []
    for rel in src_rs_files(repo):
        if str(rel) == TARGET_RENDER:
            continue
        text = read(repo, rel)
        if re.search(r"\bfn\s+sanitize\b", text):
            continue
        # Imports it *and* calls it: a module that only re-exports the name is
        # not touched by a change to its signature.
        if SANITIZE_IMPORT.search(text) and re.search(r"\bsanitize\s*\(", text):
            hits.append(str(rel))
    assert len(hits) >= 5, hits
    return {
        "id": 4,
        "key": "r1-blast-1",
        "repo": "R1",
        "kind": "blast",
        "prompt": (
            f"`pub fn sanitize` is defined in `{TARGET_RENDER}`. Other places in "
            "this workspace define their own function of the same name, and the "
            "test fixtures do too; those are different functions and are not part "
            "of the answer. If this one changed its signature, which files under "
            "`crates/*/src/` would need attention? Give the file paths."
        ),
        "truth": {"files": hits},
        "checks": [{"kind": "text", "value": h} for h in hits],
        "extras": {"kind": "none"},
    }


def r1_blast_2(repo: Path) -> dict:
    """Blast radius of `GraphDb::node_ref`, plus the calling function per file.

    A wide one on purpose: the file list is one grep, but naming the function
    each call sits in is a read per file.
    """
    owner = TARGET_DB
    assert re.search(r"\bfn\s+node_ref\b", read(repo, owner))
    rows = []
    for rel in src_rs_files(repo):
        if str(rel) == owner:
            continue
        fns = calls_by_function(read(repo, rel), "node_ref")
        if fns:
            rows.append({"path": str(rel), "functions": fns})
    assert len(rows) >= 6, rows
    checks: list[dict] = []
    for r in rows:
        checks.append({"kind": "text", "value": r["path"]})
        checks.append({"kind": "any_of", "values": r["functions"], "min": 1})
    return {
        "id": 5,
        "key": "r1-blast-2",
        "repo": "R1",
        "kind": "blast",
        "prompt": (
            f"`pub fn node_ref` is defined once in this workspace, in `{owner}`. "
            "If its signature changed, which other files under `crates/*/src/` "
            "would need attention, and in which function does each of them call "
            "it? Give one line per file: the file path, then the name of a "
            "function in it that calls `node_ref`."
        ),
        "truth": {"callers": rows},
        "checks": checks,
        "extras": {"kind": "none"},
    }


def r1_localize_1(repo: Path) -> dict:
    """Symptom: a transient write conflict is reported as a client error.

    Written from `9ed0eb4 fix(server): map Busy to 503 with Retry-After`.
    """
    builder_line = line_of(repo, TARGET_HTTP, r"\bfn\s+busy_response\b")
    router_line = line_of(repo, TARGET_HTTP, r"\bfn\s+graph_err\b")
    return {
        "id": 6,
        "key": "r1-localize-1",
        "repo": "R1",
        "kind": "localize",
        "prompt": (
            "A client writes through the HTTP API while another process holds the "
            "store's cross-process write lock. The write does not happen, nothing "
            "is corrupted, and the same request would succeed a moment later — but "
            "the client's own retry layer treats the reply as a permanent failure "
            "and gives up. Which file decides that reply, which function builds "
            "it, and which function routes the error into it? Give the file path "
            "and the two function names."
        ),
        "truth": {"file": TARGET_HTTP, "builder": "busy_response",
                  "builder_line": builder_line, "router": "graph_err",
                  "router_line": router_line},
        "checks": [
            {"kind": "text", "value": TARGET_HTTP},
            {"kind": "text", "value": "busy_response"},
            {"kind": "text", "value": "graph_err"},
        ],
        "extras": {"kind": "none"},
    }


def r1_localize_2(repo: Path) -> dict:
    """Symptom: an OR-shaped rule silently derives nothing through one branch.

    Written from `4d64ecc fix(rules): fall back to a full scan when a predicate
    holds a KeyMatch`.
    """
    test = "predicate_contains_keymatch"
    defn_line = line_of(repo, TARGET_RULES_DEF, rf"\bfn\s+{test}\b")
    users = calls_by_function(read(repo, TARGET_RULES_ENGINE), test)
    assert users, f"nothing in {TARGET_RULES_ENGINE} calls {test}"
    return {
        "id": 7,
        "key": "r1-localize-2",
        "repo": "R1",
        "kind": "localize",
        "prompt": (
            "A derived-edge rule whose condition is an OR, with a foreign-key "
            "match in one of its branches, produces no edges at all through that "
            "branch: the destinations only that branch could reach are silently "
            "missing, while the rule itself reports no error. The narrowing of "
            "the candidate set is what drops them. Which file and which function "
            "decide whether narrowing is safe for such a condition, and name a "
            "function that consults that decision. Give the file path and the two "
            "function names."
        ),
        "truth": {"file": TARGET_RULES_DEF, "decider": test,
                  "decider_line": defn_line,
                  "consumer_file": TARGET_RULES_ENGINE, "consumers": users},
        "checks": [
            {"kind": "text", "value": TARGET_RULES_DEF},
            {"kind": "text", "value": test},
            {"kind": "any_of", "values": users, "min": 1},
        ],
        "extras": {"kind": "none"},
    }


def r1_navigate_1(repo: Path) -> dict:
    """Where the automatic-snapshot archive bound lives and who applies it."""
    const_line = line_of(repo, TARGET_CLI_LIB, r"\bAUTO_SNAPSHOT_RETENTION\s*:")
    callers = []
    for rel in src_rs_files(repo):
        for fn in calls_by_function(read(repo, rel), "snapshot_automatically"):
            callers.append({"path": str(rel), "function": fn})
    assert callers, "nothing calls snapshot_automatically"
    checks = [
        {"kind": "text", "value": TARGET_CLI_LIB},
        {"kind": "line", "file": TARGET_CLI_LIB, "line": const_line, "tol": 2},
        {"kind": "text", "value": "AUTO_SNAPSHOT_RETENTION"},
        {"kind": "text", "value": "snapshot_automatically"},
        {"kind": "any_of", "values": [c["function"] for c in callers], "min": 1},
    ]
    return {
        "id": 8,
        "key": "r1-navigate-1",
        "repo": "R1",
        "kind": "navigate",
        "prompt": (
            "A snapshot taken automatically keeps only a bounded number of WAL "
            "archives. Where is that bound declared — file path and line number — "
            "what is it called, which function applies it to a store, and name one "
            "function that calls that one."
        ),
        "truth": {"const_file": TARGET_CLI_LIB, "const_line": const_line,
                  "const": "AUTO_SNAPSHOT_RETENTION",
                  "applies": "snapshot_automatically", "callers": callers},
        "checks": checks,
        "extras": {"kind": "none"},
    }


def r1_navigate_2(repo: Path) -> dict:
    """Where the short MCP tool listing is decided and where its switch is parsed."""
    list_line = line_of(repo, TARGET_MCP, r"\bfn\s+tools_list\b")
    flag_line = line_of(repo, TARGET_CLI_LIB, r'contains\(&"--all-tools"\)')
    return {
        "id": 9,
        "key": "r1-navigate-2",
        "repo": "R1",
        "kind": "navigate",
        "prompt": (
            "The MCP server answers to more tools than it lists by default. Which "
            "function builds the listing and decides how much of it to show — give "
            "its file path and line number — what is the command-line switch that "
            "asks for the full listing, and in which file and at which line is that "
            "switch parsed?"
        ),
        "truth": {"list_file": TARGET_MCP, "list_line": list_line,
                  "list_fn": "tools_list", "flag": "--all-tools",
                  "flag_file": TARGET_CLI_LIB, "flag_line": flag_line},
        "checks": [
            {"kind": "text", "value": TARGET_MCP},
            {"kind": "line", "file": TARGET_MCP, "line": list_line, "tol": 2},
            {"kind": "text", "value": "tools_list"},
            {"kind": "text", "value": "--all-tools"},
            {"kind": "text", "value": TARGET_CLI_LIB},
            {"kind": "line", "file": TARGET_CLI_LIB, "line": flag_line, "tol": 2},
        ],
        "extras": {"kind": "none"},
    }


def build_r1(repo: Path) -> list[dict]:
    """The ten R1 tasks in the §3.2 mix: 3 change, 2 blast, 2 localize,
    2 navigate, 1 history."""
    return [
        r1_change_1(repo),
        r1_change_2(repo),
        r1_change_3(repo),
        r1_blast_1(repo),
        r1_blast_2(repo),
        r1_localize_1(repo),
        r1_localize_2(repo),
        r1_navigate_1(repo),
        r1_navigate_2(repo),
        history_task(repo, task_id=10, key="r1-history-1", repo_key="R1",
                     path=TARGET_INSTALL),
    ]


def build(repo: Path, repo2: Path | None = None) -> dict:
    tasks = build_r1(repo)
    head2 = None
    if repo2 is not None:
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        from truth_r2 import build_r2  # noqa: E402

        tasks += build_r2(repo2)
        head2 = git(repo2, "rev-parse", "HEAD").strip()
    for t in tasks:
        prefix = CHANGE_PREFIX if t["kind"] == "change" else PREFIX
        t["full_prompt"] = prefix + "\n\n" + t["prompt"]
    return {
        "repo": str(repo),
        "head": git(repo, "rev-parse", "HEAD").strip(),
        "head_short": git(repo, "rev-parse", "--short", "HEAD").strip(),
        "repo2": str(repo2) if repo2 else None,
        "head2": head2,
        "prefix": PREFIX,
        "change_prefix": CHANGE_PREFIX,
        "tasks": tasks,
    }


# --------------------------------------------------------------------------
# grader
# --------------------------------------------------------------------------

def _has_line(answer: str, n: int, tol: int) -> bool:
    nums = {int(m) for m in re.findall(r"\b\d{1,6}\b", answer)}
    return any(v in nums for v in range(n - tol, n + tol + 1))


MIN_SHA_PREFIX = 6

# What one near-miss costs a `set` answer. Four of them cancel a perfect
# recall, so an answer that shotguns the plausible candidates scores nothing.
SET_PENALTY = 0.25


def _sha_tokens(answer: str) -> set[str]:
    """Every hex run in the answer that is long enough to name a commit."""
    return {m.lower() for m in
            re.findall(rf"\b[0-9a-f]{{{MIN_SHA_PREFIX},40}}\b", answer.lower())}


def _sha_match(truth: str, tokens: set[str]) -> str | None:
    """The token that names `truth`, accepting any unambiguous prefix.

    A 7-char short SHA cited as 6 chars, or spelled out in full, is the same
    commit. Matching is prefix-either-way, never a bare substring.
    """
    t = truth.lower()
    for tok in sorted(tokens, key=len, reverse=True):
        if t.startswith(tok) or tok.startswith(t):
            return tok
    return None


def unit_passed(u: dict) -> bool:
    """Whether one graded unit was fully satisfied.

    Change-task units carry a bool in `matched`; answer-task units carry the
    unit's score in `got` and, for the kinds that can say so, the text that
    matched.
    """
    if isinstance(u.get("matched"), bool):
        return u["matched"]
    got = u.get("got")
    return isinstance(got, (int, float)) and got >= 1.0


def grade(task: dict, answer: str, diff_files: set[str] | None = None,
          verify_rc: int | None = None) -> dict:
    """Score one cell.

    Change-and-pass tasks are graded on the work, not the prose: the named
    tests must pass and the diff must reach every file in the truth set.
    Every other kind is graded on the answer text, unchanged from 0.6.1.
    """
    if task.get("kind") == "change":
        want = set(task["truth"]["files"])
        if verify_rc != 0 or not diff_files:            # None included
            return {"score": 0.0,
                    "units": [{"fact": "tests pass", "got": verify_rc,
                               "matched": False}],
                    "wrong_extra": 0}
        hit = len(want & diff_files)
        units = [{"fact": f, "got": f in diff_files, "matched": f in diff_files}
                 for f in sorted(want)]
        return {"score": round(hit / len(want), 4), "units": units,
                "wrong_extra": len(diff_files - want)}

    text = answer or ""
    low = text.lower()
    tokens = _sha_tokens(text)
    units: list[dict] = []
    forbidden_hits = 0

    def unit(fact: str, got: float, matched: str | None = None) -> None:
        u = {"fact": fact, "got": got}
        if matched is not None:
            u["matched"] = matched
        units.append(u)

    for c in task["checks"]:
        kind = c["kind"]
        if kind == "text":
            unit(c["value"], 1.0 if c["value"].lower() in low else 0.0)
        elif kind == "line":
            ok = _has_line(text, c["line"], c.get("tol", 2))
            unit(f"{c['file']}:{c['line']}", 1.0 if ok else 0.0)
        elif kind == "sha":
            hit = _sha_match(c["value"], tokens)
            unit(c["value"], 1.0 if hit else 0.0, hit)
        elif kind == "author":
            # One person, several git names for one email. Any recorded name
            # counts, and we record which one the answer actually used.
            hit = next((v for v in c["values"] if v.lower() in low), None)
            unit(f"author<{c['email']}>", 1.0 if hit else 0.0, hit)
        elif kind == "any_of":
            need = c.get("min", 1)
            hits = [v for v in c["values"] if v.lower() in low]
            unit(f"any_of>={need}", min(len(hits), need) / need,
                 ", ".join(hits[:need]) if hits else None)
        elif kind == "any_of_sha":
            need = c.get("min", 1)
            hits = [v for v in c["values"] if _sha_match(v, tokens)]
            unit(f"any_of_sha>={need}", min(len(hits), need) / need,
                 ", ".join(hits[:need]) if hits else None)
        elif kind == "set":
            # A whole answer set: recall over the truth, less a penalty for
            # every near-miss the answer names. An arm that lists half the
            # world scores no better than one that lists half the truth.
            values, forbid = c["values"], c.get("forbid", [])
            hits = [v for v in values if v.lower() in low]
            bad = [v for v in forbid if v.lower() in low]
            forbidden_hits += len(bad)
            got = (len(hits) / len(values)) - SET_PENALTY * len(bad) if values else 0.0
            fact = f"set {len(hits)}/{len(values)}"
            if bad:
                fact += f" -{len(bad)}"
            unit(fact, max(0.0, round(got, 4)),
                 ", ".join(hits[:8]) if hits else None)
    score = sum(u["got"] for u in units) / len(units) if units else 0.0
    extras = forbidden_hits
    ex = task.get("extras", {})
    if ex.get("kind") == "sha":
        allowed = {a.lower() for a in ex.get("allowed", [])}
        extras += len([c for c in tokens
                       if not any(a.startswith(c) or c.startswith(a)
                                  for a in allowed)])
    return {
        "score": round(score, 4),
        "units": units,
        "wrong_extra": extras,
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--repo2", default=None,
                    help="clone of the second subject repository (see truth_r2)")
    ap.add_argument("--out", default=str(Path(__file__).parent / "tasks.json"))
    a = ap.parse_args()
    data = build(Path(a.repo).resolve(),
                 Path(a.repo2).resolve() if a.repo2 else None)
    Path(a.out).write_text(json.dumps(data, indent=2) + "\n")
    print(f"wrote {a.out} for HEAD {data['head_short']}")
    for t in data["tasks"]:
        print(f"  task {t['id']:2d} {t['key']:16s} {t['kind']:9s} "
              f"{len(t['checks'])} checks")
    return 0


if __name__ == "__main__":
    sys.exit(main())

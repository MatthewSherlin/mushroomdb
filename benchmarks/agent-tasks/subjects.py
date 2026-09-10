#!/usr/bin/env python3
"""The arms, the clones they run in, and the scratch layout around them.

One place owns where a cell's working directory comes from: the per-arm clones
of both subject repositories, the install that makes an arm a graph arm, the
per-cell worktree a change task is given, and the restore that hands the next
cell a clean tree.

Nothing here runs a session; `run.py` does that and `report.py` renders it.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
MUSHROOMDB = REPO / "target" / "release" / "mushroomdb"
SCRATCH = Path(os.environ.get("TMPDIR", "/tmp")) / "agent-bench"
SUBJECT_A = SCRATCH / "subject"
SUBJECT_B = SCRATCH / "subject-mdb"
SUBJECT_D = SCRATCH / "subject-cli"      # install --delivery cli, no MCP server
SUBJECT_E = SCRATCH / "subject-mdb-i"    # B's install, plus --intercept-grep
SUBJECT_F = SCRATCH / "subject-cli-i"    # D's install, plus --intercept-grep
EMPTY_MCP = SCRATCH / "empty-mcp.json"

# R2: the second subject repository (see truth_r2.py). One local clone at the
# pinned SHA is the source for the arms' own clones, so a run costs one fetch.
R2_SRC = SCRATCH / "r2-src"
SUBJECT2_A = SCRATCH / "subject2"
SUBJECT2_B = SCRATCH / "subject2-mdb"
SUBJECT2_D = SCRATCH / "subject2-cli"
SUBJECT2_E = SCRATCH / "subject2-mdb-i"
SUBJECT2_F = SCRATCH / "subject2-cli-i"
R2_VENV = SCRATCH / "venv-r2"            # what `python` means in an R2 verify

# The association suite (v0.6.3 §3): one built world in three forms, each its
# own self-contained directory. There is no repository and no clone here — a
# cell is given a copy of one of these three directories.
ASSOC_BUILD = SCRATCH / "assoc-build"
SUBJECT_P = ASSOC_BUILD / "files"        # entities/*.json, changes.jsonl, roles
SUBJECT_Q = ASSOC_BUILD / "sqlite"       # world.sqlite + its schema
SUBJECT_R = ASSOC_BUILD / "graph"        # the store, and the install that reaches it
ASSOC_SUBJECTS = {"P": SUBJECT_P, "Q": SUBJECT_Q, "R": SUBJECT_R}
ASSOC_ARMS = frozenset(ASSOC_SUBJECTS)

# Everything the graph subject may hold once it is provisioned. Three files the
# builder wrote, the store, and the two artefacts `install --delivery mcp`
# leaves. Anything else is data the other two arms were not given.
ASSOC_GRAPH_CONTENTS = ("world.mushroomdb", "README.md", "days.json",
                        "schema.json", ".mcp.json", ".claude")

SUBJECTS = {
    ("A", "R1"): SUBJECT_A, ("B", "R1"): SUBJECT_B,
    ("C", "R1"): SUBJECT_B, ("D", "R1"): SUBJECT_D,
    ("E", "R1"): SUBJECT_E, ("F", "R1"): SUBJECT_F,
    ("A", "R2"): SUBJECT2_A, ("B", "R2"): SUBJECT2_B,
    ("C", "R2"): SUBJECT2_B, ("D", "R2"): SUBJECT2_D,
    ("E", "R2"): SUBJECT2_E, ("F", "R2"): SUBJECT2_F,
}

# One target directory for every cargo invocation of a run — the agent's own
# `cargo test` and the harness's verify command, in every cell. A cold build
# would cost more than the whole session.
CARGO_TARGET = SCRATCH / "target-shared"
CELLS = SCRATCH / "cells"

# What an install leaves in a subject clone. A worktree gets none of it (it is
# untracked and excluded), so a change cell on a graph arm is handed a copy.
INSTALL_ARTIFACTS = (".mcp.json", ".claude", "mushroom-memory")

sys.path.insert(0, str(HERE))
import truth_r2 as TRUTH_R2                      # noqa: E402

ARM_LABEL = {
    "A": "stock",
    "B": "mushroomdb installed",
    "C": "mushroomdb, /mushroom invoked",
    "D": "mushroomdb, cli delivery (no MCP)",
    "E": "mushroomdb installed + grep redirect",
    "F": "mushroomdb cli delivery + grep redirect",
    "P": "files + grep",
    "Q": "sqlite",
    "R": "graph",
}

# What each arm actually is, in one line, for the provenance block of a
# summary. Keyed exactly like ARM_LABEL, and rendered only for the arms a run
# has rows for — a summary that describes four arms and reports two describes
# a run that did not happen.
ARM_PROVENANCE = {
    "A": "no MCP server, no project skill, no hooks; plain prompt",
    "B": "`mushroomdb install` — MCP server + project skill + SessionStart "
         "brief + prompt and edit hooks; plain prompt",
    "C": "arm B's clone and install exactly; prompt prefixed with "
         "`/mushroom `",
    "D": "`mushroomdb install --delivery cli` — the skill teaches the binary, "
         "no MCP server; plain prompt",
    "E": "`mushroomdb install --intercept-grep` — arm B's install plus the "
         "PreToolUse redirect from `Grep` to `explore`; plain prompt",
    "F": "`mushroomdb install --delivery cli --intercept-grep` — arm D's "
         "install plus that redirect; plain prompt",
    "P": "the world as `entities/*.json`, `changes.jsonl`, `roles.json` and a "
         "README describing the rules; no MCP server; plain prompt",
    "Q": "the same world as `world.sqlite` (`sqlite3` on PATH) with the same "
         "README and its schema; no MCP server; plain prompt",
    "R": "the same world as a mushroomdb store — `install --delivery mcp "
         "--db ./world.mushroomdb` into a directory holding nothing else, so "
         "the store is the only data path; plain prompt",
}

# The arms whose session is given the MCP tool. Kept beside the arms rather
# than read off `cell_command`, which lives in `run.py` and imports this file.
MCP_ARMS = frozenset({"B", "C", "E", "R"})

# Both arms get exactly these tools. `Bash` is unqualified on purpose: the
# per-command form `Bash(git:*)` denies every pipeline (`git log ... | sort |
# uniq -c`), which is the natural way to answer the co-change and
# most-imported questions, so both arms burned all 12 turns on denials and
# returned nothing. See agent-eval-pilot.md, defect 1.
BASE_TOOLS = [
    "Read",
    "Grep",
    "Glob",
    "Bash",
    "Edit",
    "Write",
]
MCP_TOOL = "mcp__mushroomdb"
CELL_TIMEOUT_S = 900
DEFAULT_MAX_TURNS = 30


def sh(cmd: list[str], cwd: Path | None = None, timeout: int = 900) -> str:
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed ({p.returncode}):\n{p.stderr}")
    return p.stdout


def ensure_binary() -> None:
    if MUSHROOMDB.exists():
        return
    print("building release binary...")
    sh(["cargo", "build", "--release", "-p", "mushroomdb-cli"], cwd=REPO, timeout=3600)
    if not MUSHROOMDB.exists():
        sh(["cargo", "build", "--release"], cwd=REPO, timeout=3600)


# --------------------------------------------------------------------------
# subject clones
# --------------------------------------------------------------------------


def ensure_subject(subject: Path, source: Path, sha: str | None = None) -> None:
    """Clone `source` into `subject` if missing, and put it on `sha`.

    Both subjects are pinned, not followed: `tasks.json` names the commit its
    ground truth was computed from, and a clone that has drifted (a new commit
    on the branch R1 was cloned from, a fetched R2) is fetched and reset back
    to it. A run measures the repository the task set describes or it measures
    nothing.
    """
    if not subject.exists():
        print(f"cloning subject -> {subject}")
        sh(["git", "clone", "-q", str(source), str(subject)])
    if not sha:
        return
    known = subprocess.run(["git", "cat-file", "-e", f"{sha}^{{commit}}"],
                           cwd=subject, capture_output=True).returncode == 0
    if not known:
        sh(["git", "fetch", "-q", str(source)], cwd=subject)
    if sh(["git", "rev-parse", "HEAD"], cwd=subject).strip() != sha:
        print(f"resetting {subject.name} -> {sha[:12]}")
        sh(["git", "reset", "--hard", "-q", sha], cwd=subject)


def install_subject(subject: Path, extra_install_args: list[str],
                    source: Path = REPO, sha: str | None = None) -> None:
    """Clone `subject` if missing, install mushroomdb into it (idempotent),
    hide the install's dirty-tree artifacts, and ingest its store."""
    ensure_subject(subject, source, sha)
    if not (subject / ".claude").exists():
        print(f"installing mushroomdb into {subject.name}")
        print(sh([
            str(MUSHROOMDB), "install", "--project",
            "--platform", "claude-code",
            "--command", str(MUSHROOMDB),
            "--no-prewarm", "--no-git-hooks",
            *extra_install_args,
        ], cwd=subject))
    # The install dirties the working tree (.mcp.json, .claude/, .gitignore).
    # A dirty tree makes the UserPromptSubmit recall hook report the install
    # artifacts instead of the question, so hide them the way a real project
    # would have them committed or ignored -- without adding a commit, which
    # would shift the "last N commits" windows the ground truth depends on.
    exclude = subject / ".git" / "info" / "exclude"
    marker = "mushroom-memory/"
    if marker not in exclude.read_text():
        sh(["git", "checkout", "--", ".gitignore"], cwd=subject)
        with exclude.open("a") as fh:
            fh.write("\n.mcp.json\n.claude/\nmushroom-memory/\n")
    dirty = sh(["git", "status", "--porcelain"], cwd=subject).strip()
    if dirty:
        raise SystemExit(f"{subject.name} clone is not clean after install:\n{dirty}")

    if not (subject / "mushroom-memory").exists():
        print(f"ingesting {subject.name} store")
        print(sh([str(MUSHROOMDB), "ingest-git", "./mushroom-memory", "."],
                 cwd=subject))


def delivery_flag_exists() -> bool:
    """Whether this binary's `install` understands `--delivery`.

    The flag ships (§4.5), so this is true of any current build. It stays
    because the harness is also pointed at older binaries — a release under
    comparison, a bisect — and asking one of those for `--delivery cli` would
    fail the install; skipping the arm reports that instead of dying.
    """
    p = subprocess.run([str(MUSHROOMDB), "install", "--help"],
                       capture_output=True, text=True, timeout=60)
    return "--delivery" in (p.stdout + p.stderr)


def ensure_r2_venv() -> None:
    """The interpreter an R2 change task's `python -m pytest` resolves to.

    Built from the subject's own locked test dependency group, outside the
    clone so no subject tree is dirtied and every per-cell worktree shares it.
    The subject package itself is deliberately not installed: `python -m
    pytest` puts the working directory first on `sys.path`, so a cell tests
    the source the agent just edited, not a copy in site-packages.
    """
    if (R2_VENV / "bin" / "python").exists():
        return
    if shutil.which("uv") is None:
        raise SystemExit("uv is required to build the R2 test venv "
                         "(https://docs.astral.sh/uv/)")
    print(f"building R2 test venv -> {R2_VENV}")
    env = dict(os.environ, UV_PROJECT_ENVIRONMENT=str(R2_VENV))
    p = subprocess.run(
        ["uv", "sync", "--frozen", "--no-install-project",
         "--group", TRUTH_R2.TEST_DEP_GROUP, "--python", "3.12"],
        cwd=R2_SRC, env=env, capture_output=True, text=True, timeout=1800)
    if p.returncode != 0:
        raise SystemExit(f"uv sync failed:\n{p.stderr}")


# --------------------------------------------------------------------------
# the association suite: one built world, three subject directories
# --------------------------------------------------------------------------

ASSOC_SEED = 20260910
ASSOC_SCALE = 2000
ASSOC_BUILD_TIMEOUT_S = 3600


def build_association_world(force: bool = False) -> Path:
    """The generator's three forms under `ASSOC_BUILD`, built once.

    Deterministic in `ASSOC_SEED`/`ASSOC_SCALE`, and expensive (5-6 minutes at
    2,000 nodes), so it is rebuilt only when a form is missing or `--force-setup`
    asks. `association/tasks.json` records the world's digest; a rebuild that
    moved a fact would be caught there, not here.
    """
    from association.build import form_paths
    where = form_paths(ASSOC_BUILD)
    if force:
        shutil.rmtree(ASSOC_BUILD, ignore_errors=True)
    if all(where[f].exists() for f in ("files", "sqlite", "graph")):
        print(f"association world already built at {ASSOC_BUILD}; keeping it")
        return ASSOC_BUILD
    print(f"building the association world -> {ASSOC_BUILD} (5-6 minutes)")
    sh([sys.executable, str(HERE / "association" / "build.py"),
        "--seed", str(ASSOC_SEED), "--scale", str(ASSOC_SCALE),
        "--out", str(ASSOC_BUILD), "--binary", str(MUSHROOMDB)],
       cwd=HERE, timeout=ASSOC_BUILD_TIMEOUT_S)
    return ASSOC_BUILD


def install_association_graph(graph: Path) -> None:
    """Make the store arm R's only data path (§5), and prove it.

    `install --delivery mcp` into the graph form's own directory: an MCP entry
    pinned to the store with `--db` (never `--auto`, which would walk up and
    find another store), the skill, the hooks, and no git hooks — the directory
    is not a repository. The `.gitignore` the install writes for the store is
    removed again: nothing here is versioned, and it would be a seventh file in
    a directory whose contents this asserts.

    Idempotent, and it fails loudly rather than handing a run a subject that
    is not what the summary will say it was.
    """
    graph = Path(graph)
    if not (graph / ".mcp.json").exists():
        print(f"installing mushroomdb into {graph}")
        print(sh([
            str(MUSHROOMDB), "install", "--project",
            "--platform", "claude-code",
            "--command", str(MUSHROOMDB),
            "--no-prewarm", "--no-git-hooks",
            "--delivery", "mcp",
            "--db", f"./{ASSOC_GRAPH_CONTENTS[0]}",
        ], cwd=graph))
    stray = graph / ".gitignore"
    if stray.exists():
        stray.unlink()
    present = {p.name for p in graph.iterdir()}
    if present != set(ASSOC_GRAPH_CONTENTS):
        raise SystemExit(
            f"the graph subject holds {sorted(present)}, expected "
            f"{sorted(ASSOC_GRAPH_CONTENTS)}")
    p = subprocess.run([str(MUSHROOMDB), "doctor", "--project",
                        "--platform", "claude-code"],
                       cwd=graph, capture_output=True, text=True, timeout=300)
    if p.returncode != 0:
        raise SystemExit(f"doctor failed in {graph}:\n{p.stdout}\n{p.stderr}")


def setup_association(force: bool = False) -> set[str]:
    """Provision arms P, Q and R. Returns the arms it got.

    No clones, no worktrees, no ground-truth regeneration: the three forms are
    the subjects, and `association/tasks.json` is built by `build_tasks.py`
    against the world's digest rather than by this function.
    """
    ensure_binary()
    SCRATCH.mkdir(parents=True, exist_ok=True)
    EMPTY_MCP.write_text('{"mcpServers":{}}\n')
    shutil.rmtree(CELLS, ignore_errors=True)
    build_association_world(force)
    install_association_graph(SUBJECT_R)
    for arm, subject in sorted(ASSOC_SUBJECTS.items()):
        if not subject.exists():
            raise SystemExit(f"arm {arm}: no subject at {subject}")
        print(f"arm {arm}: {subject}")
    print("setup complete")
    return set(ASSOC_ARMS)


def setup(force: bool = False, arms: set[str] | None = None,
          suite: str = "code") -> set[str]:
    """Provision what the requested arms of this suite need.

    The association suite has its own provisioning entirely — see
    `setup_association`.
    """
    if suite == "association":
        return setup_association(force)
    return setup_code(force, arms)


def setup_code(force: bool = False, arms: set[str] | None = None) -> set[str]:
    """Provision the clones the requested arms need. Returns the arms it got.

    The task set is the pin: `tasks.json` names the commit of each subject its
    truth was computed from, and setup puts the clones there. It is rebuilt
    only when it is missing or `--force-setup` asks for it — never because a
    clone's HEAD moved, which would silently retire the pilot's measurements.
    """
    if arms is None:
        arms = {"A", "B", "C", "D"}
    ensure_binary()
    SCRATCH.mkdir(parents=True, exist_ok=True)
    CARGO_TARGET.mkdir(parents=True, exist_ok=True)
    EMPTY_MCP.write_text('{"mcpServers":{}}\n')

    want_d = "D" in arms
    if want_d and not delivery_flag_exists():
        print("arm D: this binary's install has no --delivery flag; "
              "skipping its subjects")
        want_d = False
    want_e = "E" in arms
    want_f = "F" in arms
    if want_f and not delivery_flag_exists():
        print("arm F: this binary's install has no --delivery flag; "
              "skipping its subjects")
        want_f = False

    tasks_path = HERE / "tasks.json"
    pinned = json.loads(tasks_path.read_text()) if tasks_path.exists() else None
    rebuild = force or pinned is None
    r1_sha = (sh(["git", "rev-parse", "HEAD"], cwd=REPO).strip()
              if rebuild else pinned["head"])
    r2_sha = TRUTH_R2.R2["sha"]

    if force:
        dirs = [SUBJECT_A, SUBJECT_B, SUBJECT2_A, SUBJECT2_B]
        if want_d:
            dirs += [SUBJECT_D, SUBJECT2_D]
        if want_e:
            dirs += [SUBJECT_E, SUBJECT2_E]
        if want_f:
            dirs += [SUBJECT_F, SUBJECT2_F]
        for d in dirs:
            shutil.rmtree(d, ignore_errors=True)
    shutil.rmtree(CELLS, ignore_errors=True)

    ensure_subject(SUBJECT_A, REPO, r1_sha)
    # arm A must stay clean: no .mcp.json, no .claude/
    for stray in (SUBJECT_A / ".mcp.json", SUBJECT_A / ".claude"):
        if stray.exists():
            raise SystemExit(f"arm A clone is contaminated: {stray}")

    install_subject(SUBJECT_B, [], REPO, r1_sha)
    if want_d:
        install_subject(SUBJECT_D, ["--delivery", "cli"], REPO, r1_sha)
    if want_e:
        install_subject(SUBJECT_E, ["--intercept-grep"], REPO, r1_sha)
    if want_f:
        install_subject(SUBJECT_F, ["--delivery", "cli", "--intercept-grep"],
                        REPO, r1_sha)

    # R2: one pinned clone feeds the arms' clones, and the venv the change
    # tasks test in.
    TRUTH_R2.ensure_clone(R2_SRC)
    ensure_subject(SUBJECT2_A, R2_SRC, r2_sha)
    for stray in (SUBJECT2_A / ".mcp.json", SUBJECT2_A / ".claude"):
        if stray.exists():
            raise SystemExit(f"arm A R2 clone is contaminated: {stray}")
    install_subject(SUBJECT2_B, [], R2_SRC, r2_sha)
    if want_d:
        install_subject(SUBJECT2_D, ["--delivery", "cli"], R2_SRC, r2_sha)
    if want_e:
        install_subject(SUBJECT2_E, ["--intercept-grep"], R2_SRC, r2_sha)
    if want_f:
        install_subject(SUBJECT2_F, ["--delivery", "cli", "--intercept-grep"],
                        R2_SRC, r2_sha)
    ensure_r2_venv()

    if rebuild:
        print("regenerating ground truth from the arm A clones")
        sh([sys.executable, str(HERE / "ground_truth.py"),
            "--repo", str(SUBJECT_A), "--repo2", str(SUBJECT2_A),
            "--out", str(tasks_path)])
    else:
        print(f"tasks.json pins {r1_sha[:12]} / {r2_sha[:12]}; keeping it")
    print("setup complete")
    gated = {"D": want_d, "E": want_e, "F": want_f}
    provisioned = {a for a in arms if a not in gated} | {a for a, ok in gated.items() if ok}
    return provisioned


# --------------------------------------------------------------------------
# what a cell runs in
# --------------------------------------------------------------------------


def subject_root(arm: str, task: dict) -> Path:
    """Where this arm's data lives. The association arms have one directory
    each and no repository, so the task's `repo` never enters into it."""
    if arm in ASSOC_ARMS:
        return ASSOC_SUBJECTS[arm]
    return SUBJECTS[(arm, task.get("repo", "R1"))]


def assoc_cell_dir(arm: str) -> Path:
    """Where an association cell for this arm runs.

    One path per arm, reused by every cell of that arm: cells run strictly
    sequentially, and `make_cell_copy` replaces the tree before each one.
    """
    return CELLS / f"assoc-{arm}"


def make_cell_copy(subject: Path, dest: Path) -> Path:
    """A fresh copy of a subject directory for one cell.

    The association subjects are not git repositories, so there is no worktree
    to add and no `git clean` to undo a cell's scratch files — the copy is both.
    `symlinks=False` on purpose: a link out of the tree would let one cell's
    writes reach the subject the next cell is copied from.
    """
    shutil.rmtree(dest, ignore_errors=True)
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(subject, dest, symlinks=False)
    return dest


def child_env(task: dict | None = None) -> dict[str, str]:
    env = dict(os.environ)
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT"):
        env.pop(k, None)
    # Every cargo invocation of a run — the agent's own and the harness's —
    # shares one target directory, so only the first pays for a build.
    env["CARGO_TARGET_DIR"] = str(CARGO_TARGET)
    if task is not None and task.get("repo") == "R2":
        # `python` means the subject's own test venv, for the agent and for
        # the verify command alike.
        env["VIRTUAL_ENV"] = str(R2_VENV)
        env["PATH"] = f"{R2_VENV / 'bin'}{os.pathsep}{env.get('PATH', '')}"
    return env


def changed_files(cwd: Path) -> set[str]:
    """Every tracked file the working tree changed, plus every new file.

    Ignored paths stay out, which is how the install artifacts a graph cell
    was handed do not count as the agent's work.
    """
    out = sh(["git", "status", "--porcelain", "--untracked-files=all"], cwd=cwd)
    files: set[str] = set()
    for line in out.splitlines():
        path = line[3:]
        if not path:
            continue
        if " -> " in path:            # renames report `old -> new`
            path = path.split(" -> ", 1)[1]
        files.add(path.strip().strip('"'))
    return files


def restore_subject(cwd: Path) -> bool:
    """Put a shared clone back the way the next cell needs it.

    Returns whether the cell had left anything behind. Non-change cells are
    given `Edit` and `Write` like every other cell, and one that edits the
    clone would hand the next cell — and the next arm — a different
    repository. Ignored paths are left alone on purpose: that is the install,
    and the store.
    """
    dirty = changed_files(cwd)
    if dirty:
        sh(["git", "checkout", "--", "."], cwd=cwd)
        sh(["git", "clean", "-fdq"], cwd=cwd)
    return bool(dirty)


def cell_worktree(arm: str, task: dict) -> Path:
    """Where a change cell for this repository runs, regardless of arm.

    One path per repository, reused by every cell of every arm: cargo
    fingerprints are keyed on the absolute path of the crate, so a distinct
    path per arm (as well as per cell) grew the shared target directory into
    tens of gigabytes over a full run. Cells run strictly sequentially in
    `main()` — never two arms' worktrees at once — so one path per repo is
    safe: `make_worktree` removes and re-adds it fresh before every cell.
    """
    return CELLS / task.get("repo", "R1")


def make_worktree(subject: Path, dest: Path) -> Path:
    """A fresh checkout of the subject's HEAD, carrying the install artifacts.

    Removed and re-added for every cell, so the reused path is never a reused
    tree: the agent always starts from the pinned commit.

    A worktree gets tracked files only. Arms B, C and D need the MCP config,
    the skill and the store to be in the cell too, or the change tasks would
    measure a stock session wearing another arm's name.
    """
    drop_worktree(subject, dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    sh(["git", "worktree", "add", "--detach", "-q", str(dest), "HEAD"], cwd=subject)
    for name in INSTALL_ARTIFACTS:
        src = subject / name
        if src.is_dir():
            shutil.copytree(src, dest / name)
        elif src.exists():
            shutil.copy2(src, dest / name)
    return dest


def drop_worktree(subject: Path, dest: Path) -> None:
    if dest.exists():
        try:
            sh(["git", "worktree", "remove", "--force", str(dest)], cwd=subject)
        except Exception:                               # noqa: BLE001
            shutil.rmtree(dest, ignore_errors=True)
    subprocess.run(["git", "worktree", "prune"], cwd=subject, capture_output=True)

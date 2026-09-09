#!/usr/bin/env python3
"""Ground truth for R2, the second benchmark subject.

R2 is FastAPI at a pinned tag: MIT, Python, 543 non-test source files, and a
test suite that runs in about 30 seconds — inside the §3.1 envelope of 300-800
source files and five minutes. The extractor reads it: after `ingest-git`,
every one of the 424 non-test source files that defines a top-level symbol has
symbols in the graph (`fastapi/` itself: 28 of 28; the 19 files without symbols
are re-export shims that define none).

Same five task kinds as R1, same shapes, computed from the clone by grep and
git alone — never from the graph store.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

R2 = {
    "url": "https://github.com/fastapi/fastapi",
    "sha": "95f8322ee1dcda7ceace7b1c4f6c9915b36d748f",   # tag 0.141.1
    "kind": "python",
    "tag": "0.141.1",
    "license": "MIT",
    "package": "fastapi",
}

# The venv `run.py` builds for the change tasks; `python` in a verify command
# resolves to it because the cell's PATH names it first.
TEST_DEP_GROUP = "tests"


def ensure_clone(dest: Path) -> Path:
    """Clone R2 at its pinned SHA if `dest` is not already that checkout."""
    if not (dest / ".git").exists():
        subprocess.run(["git", "clone", "-q", R2["url"], str(dest)], check=True)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=dest, check=True,
                          capture_output=True, text=True).stdout.strip()
    if head != R2["sha"]:
        subprocess.run(["git", "checkout", "-q", R2["sha"]], cwd=dest, check=True)
    return dest


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------

PKG = R2["package"]


def read(repo: Path, rel: str) -> str:
    return (repo / rel).read_text(encoding="utf-8", errors="replace")


def pkg_files(repo: Path) -> list[str]:
    """Every .py file of the library itself, sorted. Tests and docs stay out."""
    return sorted(str(p.relative_to(repo)) for p in (repo / PKG).rglob("*.py"))


def line_of(repo: Path, rel: str, pattern: str) -> int:
    pat = re.compile(pattern)
    for i, line in enumerate(read(repo, rel).splitlines(), start=1):
        if pat.search(line):
            return i
    raise AssertionError(f"{pattern!r} not found in {rel}")


NESTED_DEF = re.compile(r"^\s*(?:async def|def|class)\s+([A-Za-z_]\w*)")


IMPORT_LINE = re.compile(r"^\s*(?:from\s+\S+\s+)?import\b")


def files_using(repo: Path, name: str) -> list[str]:
    """Library files that use `name` without defining it.

    A file whose every mention is an import only re-exports the symbol: a
    change to its signature never reaches it, and counting it would be the
    same over-counting the 0.6.1 grader was faulted for.
    """
    pat = re.compile(r"\b" + re.escape(name) + r"\b")
    owner = re.compile(r"^(?:async def|def|class)\s+" + re.escape(name) + r"\b", re.M)
    out = []
    for rel in pkg_files(repo):
        text = read(repo, rel)
        if owner.search(text):
            continue
        if any(pat.search(ln) and not IMPORT_LINE.match(ln)
               for ln in text.splitlines()):
            out.append(rel)
    return out


def calls_by_function(text: str, name: str) -> list[str]:
    """Names of the functions or classes in `text` that call `name`."""
    found: list[str] = []
    current: str | None = None
    call = re.compile(r"\b" + re.escape(name) + r"\s*\(")
    for line in text.splitlines():
        m = NESTED_DEF.match(line)
        if m:
            if m.group(1) == name:
                current = None
                continue
            current = m.group(1)
        if current and call.search(line) and current not in found:
            found.append(current)
    return found


def defining_file(repo: Path, name: str) -> str:
    owners = [rel for rel in pkg_files(repo)
              if re.search(r"^(?:async def|def|class)\s+" + re.escape(name) + r"\b",
                           read(repo, rel), re.M)]
    assert len(owners) == 1, f"{name} is defined in {owners}"
    return owners[0]


def pytest_cmd(test_file: str) -> dict:
    return {"cmd": ["python", "-m", "pytest", test_file, "-q", "-x"]}


# --------------------------------------------------------------------------
# tasks
# --------------------------------------------------------------------------


def r2_change_1(repo: Path) -> dict:
    """Make a compile-time cache bound settable at run time.

    Three memoised helpers share the constant, and an `lru_cache` cannot be
    resized in place, so the change is a rebuild rather than an assignment.
    """
    const = "_CALLABLE_CLASSIFICATION_CACHE_SIZE"
    files = [f"{PKG}/dependencies/models.py", "tests/test_dependency_models.py"]
    for f in files:
        assert (repo / f).exists(), f
    text = read(repo, files[0])
    assert text.count(f"@lru_cache(maxsize={const})") == 3, "the cache shape moved"
    return {
        "id": 11,
        "key": "r2-change-1",
        "repo": "R2",
        "kind": "change",
        "prompt": (
            f"Three helpers in `{PKG}/dependencies/models.py` are memoised with "
            f"`@lru_cache(maxsize={const})`. Make that bound settable at run "
            "time: add a `set_callable_classification_cache_size(size: int)` "
            "function to the same module that rebuilds all three caches at the "
            "new size and leaves them empty — an `lru_cache` cannot be resized in "
            "place — keeping 4096 as the default, and make sure the module's own "
            "callers go through whatever the current wrappers are. Add a test to "
            "`tests/test_dependency_models.py` that after setting the size to 1 "
            "the classification of two different callables leaves a cache holding "
            "one entry (`cache_info()`).\n\n"
            "Test command: `python -m pytest tests/test_dependency_models.py -q -x`"
        ),
        "truth": {"files": files},
        "checks": [],
        "extras": {"kind": "none"},
        "verify": pytest_cmd("tests/test_dependency_models.py"),
    }


def r2_change_2(repo: Path) -> dict:
    """Make the SSE keep-alive interval a per-response setting."""
    files = [f"{PKG}/routing.py", f"{PKG}/sse.py", "tests/test_sse.py"]
    for f in files:
        assert (repo / f).exists(), f
    assert "_PING_INTERVAL" in read(repo, f"{PKG}/sse.py")
    assert "_PING_INTERVAL" in read(repo, f"{PKG}/routing.py")
    return {
        "id": 12,
        "key": "r2-change-2",
        "repo": "R2",
        "kind": "change",
        "prompt": (
            f"Server-sent-event responses ping an idle connection on a fixed "
            f"interval: the module-level `_PING_INTERVAL` in `{PKG}/sse.py`, read "
            f"by the streaming request handler in `{PKG}/routing.py`. Make it a "
            "per-response setting: add a `ping_interval: float | None = None` "
            "argument to `EventSourceResponse.__init__` and keep it on the "
            "instance; have the streaming handler use the response's value when it "
            "has one and the module constant otherwise; add a test to "
            "`tests/test_sse.py` that a response with a short interval pings "
            "sooner than the default would.\n\n"
            "Test command: `python -m pytest tests/test_sse.py -q -x`"
        ),
        "truth": {"files": files},
        "checks": [],
        "extras": {"kind": "none"},
        "verify": pytest_cmd("tests/test_sse.py"),
    }


def r2_change_3(repo: Path) -> dict:
    """Mirror an option one docs handler has onto the other."""
    files = [f"{PKG}/applications.py", f"{PKG}/openapi/docs.py",
             "tests/test_local_docs.py"]
    for f in files:
        assert (repo / f).exists(), f
    assert "swagger_ui_parameters" in read(repo, f"{PKG}/openapi/docs.py")
    assert "redoc_ui_parameters" not in read(repo, f"{PKG}/openapi/docs.py")
    return {
        "id": 13,
        "key": "r2-change-3",
        "repo": "R2",
        "kind": "change",
        "prompt": (
            "`get_swagger_ui_html` takes a `swagger_ui_parameters` mapping that "
            "the application threads through from its own constructor argument, "
            "and writes those keys into the page it renders. `get_redoc_html` has "
            "no equivalent. Add one: a `redoc_ui_parameters: dict[str, Any] | None "
            "= None` argument on `get_redoc_html` whose keys are written onto the "
            "rendered Redoc element, a matching `redoc_ui_parameters` argument on "
            "the application that is stored and passed to it by the built-in "
            "`/redoc` route, and a test in `tests/test_local_docs.py` that a value "
            "passed there reaches the rendered HTML.\n\n"
            "Test command: `python -m pytest tests/test_local_docs.py -q -x`"
        ),
        "truth": {"files": files},
        "checks": [],
        "extras": {"kind": "none"},
        "verify": pytest_cmd("tests/test_local_docs.py"),
    }


def r2_blast_1(repo: Path) -> dict:
    """Blast radius of `lenient_issubclass`, two call sites per file.

    The file list is one grep; naming two of the functions each file calls it
    from is a read of every one of them.
    """
    name = "lenient_issubclass"
    owner = defining_file(repo, name)
    rows = []
    for rel in files_using(repo, name):
        fns = calls_by_function(read(repo, rel), name)
        if fns:
            rows.append({"path": rel, "functions": fns})
    assert len(rows) >= 4, rows
    checks: list[dict] = []
    for r in rows:
        checks.append({"kind": "text", "value": r["path"]})
        checks.append({"kind": "any_of", "values": r["functions"],
                       "min": min(2, len(r["functions"]))})
    return {
        "id": 14,
        "key": "r2-blast-1",
        "repo": "R2",
        "kind": "blast",
        "prompt": (
            f"`{name}` is defined in `{owner}`. If its signature changed, which "
            f"other files under `{PKG}/` would need attention, and which "
            "functions in them call it? Give one line per file: the file path, "
            "then up to two functions in that file that call it."
        ),
        "truth": {"callers": rows, "definition": owner},
        "checks": checks,
        "extras": {"kind": "none"},
    }


def r2_blast_2(repo: Path) -> dict:
    name = "jsonable_encoder"
    owner = defining_file(repo, name)
    rows = []
    for rel in files_using(repo, name):
        fns = calls_by_function(read(repo, rel), name)
        if fns:
            rows.append({"path": rel, "functions": fns})
    assert len(rows) >= 3, rows
    checks: list[dict] = []
    for r in rows:
        checks.append({"kind": "text", "value": r["path"]})
        checks.append({"kind": "any_of", "values": r["functions"], "min": 1})
    return {
        "id": 15,
        "key": "r2-blast-2",
        "repo": "R2",
        "kind": "blast",
        "prompt": (
            f"`{name}` is defined in `{owner}`. If its signature changed, which "
            f"other files under `{PKG}/` would need attention, and in which "
            "function or method does each of them call it? Give one line per file: "
            "the file path, then the name of the calling function."
        ),
        "truth": {"callers": rows, "definition": owner},
        "checks": checks,
        "extras": {"kind": "none"},
    }


def r2_localize_1(repo: Path) -> dict:
    """Symptom from `65e42bd5e Fix handling sequences with nested Annotated
    types`: the two predicates, and the two places that ask them."""
    shared = defining_file(repo, "field_annotation_is_sequence")
    assert shared == defining_file(repo, "field_annotation_is_scalar_sequence")
    askers: list[str] = []
    for name in ("field_annotation_is_sequence", "field_annotation_is_scalar_sequence"):
        for rel in files_using(repo, name):
            askers += calls_by_function(read(repo, rel), name)
    askers = sorted(set(askers))
    assert len(askers) >= 2, askers
    return {
        "id": 16,
        "key": "r2-localize-1",
        "repo": "R2",
        "kind": "localize",
        "prompt": (
            "A path operation declares a query parameter as a list whose element "
            "type carries its own metadata annotation — an annotated type nested "
            "inside the sequence. The parameter stops behaving like a sequence: a "
            "repeated query key arrives as a single value instead of a list, and "
            "no error is raised. Which file holds the two predicates that decide "
            "whether an annotation is a sequence and whether it is a sequence of "
            "scalars? Name the file, both predicates, and the two functions "
            "elsewhere in the package that ask them while a parameter is analysed."
        ),
        "truth": {"file": shared,
                  "predicates": ["field_annotation_is_sequence",
                                 "field_annotation_is_scalar_sequence"],
                  "askers": askers},
        "checks": [
            {"kind": "text", "value": shared},
            {"kind": "text", "value": "field_annotation_is_sequence"},
            {"kind": "text", "value": "field_annotation_is_scalar_sequence"},
            {"kind": "any_of", "values": askers, "min": min(2, len(askers))},
        ],
        "extras": {"kind": "none"},
    }


def r2_localize_2(repo: Path) -> dict:
    """Symptom from `ad03e117c Fix stream item type lost when using
    include_router()`."""
    rel = f"{PKG}/routing.py"
    setter = "_populate_api_route_state"
    fn_line = line_of(repo, rel, rf"^def {setter}\b")
    callers = calls_by_function(read(repo, rel), setter)
    assert callers, f"nothing calls {setter}"
    return {
        "id": 17,
        "key": "r2-localize-2",
        "repo": "R2",
        "kind": "localize",
        "prompt": (
            "A router declares a path operation that streams items of a declared "
            "type. Mounted directly on the application it serialises them with "
            "that type; mounted through another router, the declared item type is "
            "lost and the stream is serialised without it, silently. Which file "
            "holds the state that carries that type onto a route, what is the "
            "route attribute called, which function sets it, and name a function "
            "that calls that one. Give the file path, the attribute, and the two "
            "function names."
        ),
        "truth": {"file": rel, "attribute": "stream_item_type", "setter": setter,
                  "setter_line": fn_line, "callers": callers},
        "checks": [
            {"kind": "text", "value": rel},
            {"kind": "text", "value": "stream_item_type"},
            {"kind": "text", "value": setter},
            {"kind": "any_of", "values": callers, "min": 1},
        ],
        "extras": {"kind": "none"},
    }


def r2_navigate_1(repo: Path) -> dict:
    """The path a declared response model takes into the generated document.

    Four names over three files, and two of them are reached through a package
    re-export rather than where they are imported from.
    """
    doc = f"{PKG}/openapi/utils.py"
    consts = f"{PKG}/openapi/constants.py"
    schema_fn = "get_schema_from_model_field"
    defs_fn = "get_definitions"
    owner = defining_file(repo, schema_fn)
    assert owner == defining_file(repo, defs_fn), "the two helpers moved apart"
    assert "REF_PREFIX" in read(repo, consts)
    path_line = line_of(repo, doc, r"^def get_openapi_path\b")
    return {
        "id": 18,
        "key": "r2-navigate-1",
        "repo": "R2",
        "kind": "navigate",
        "prompt": (
            "Trace how a route's declared response model reaches the generated "
            "OpenAPI document. Name, with the file each one lives in: the "
            "function that builds one path's entry in the document; the helper it "
            "calls to turn a model field into a JSON-Schema fragment, and the "
            "file that actually defines that helper rather than the one it is "
            "imported from; the function that collects every model's definitions "
            "once for the whole document; and the module-level constant that "
            "names the prefix those fragments refer to each other by."
        ),
        "truth": {"path_builder": "get_openapi_path", "path_builder_file": doc,
                  "path_builder_line": path_line, "schema_helper": schema_fn,
                  "definitions": defs_fn, "helpers_file": owner,
                  "prefix": "REF_PREFIX", "prefix_file": consts},
        "checks": [
            {"kind": "text", "value": "get_openapi_path"},
            {"kind": "text", "value": doc},
            {"kind": "text", "value": schema_fn},
            {"kind": "text", "value": owner},
            {"kind": "text", "value": defs_fn},
            {"kind": "text", "value": "REF_PREFIX"},
            {"kind": "text", "value": consts},
        ],
        "extras": {"kind": "none"},
    }


def r2_navigate_2(repo: Path) -> dict:
    rel = f"{PKG}/routing.py"
    owner = defining_file(repo, "get_value_or_default")
    use_line = line_of(repo, rel, r"default_response_class=get_value_or_default")
    return {
        "id": 19,
        "key": "r2-navigate-2",
        "repo": "R2",
        "kind": "navigate",
        "prompt": (
            "A router included in another router does not always name its own "
            "response class; when it does not, the parent's choice is used, and a "
            "sentinel tells the two apart. Which function resolves that — give its "
            "name and the file it is defined in — where is it applied to the "
            "response class (file path and line number), and what is the sentinel "
            "type called?"
        ),
        "truth": {"resolver": "get_value_or_default", "resolver_file": owner,
                  "use_file": rel, "use_line": use_line,
                  "sentinel": "DefaultPlaceholder"},
        "checks": [
            {"kind": "text", "value": "get_value_or_default"},
            {"kind": "text", "value": owner},
            {"kind": "text", "value": rel},
            {"kind": "line", "file": rel, "line": use_line, "tol": 3},
            {"kind": "text", "value": "DefaultPlaceholder"},
        ],
        "extras": {"kind": "none"},
    }


def build_r2(repo: Path) -> list[dict]:
    """The ten R2 tasks in the §3.2 mix: 3 change, 2 blast, 2 localize,
    2 navigate, 1 history."""
    from ground_truth import history_task  # noqa: E402  (same directory)

    return [
        r2_change_1(repo),
        r2_change_2(repo),
        r2_change_3(repo),
        r2_blast_1(repo),
        r2_blast_2(repo),
        r2_localize_1(repo),
        r2_localize_2(repo),
        r2_navigate_1(repo),
        r2_navigate_2(repo),
        # A wider window than R1's: this repository takes many more commits to
        # accumulate a co-change signal that is not a two-way tie.
        history_task(repo, task_id=20, key="r2-history-1", repo_key="R2",
                     path=f"{PKG}/routing.py", window=2000),
    ]

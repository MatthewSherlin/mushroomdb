# Docs overhaul, pull request 3a — the generated reference: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Three reference pages — the CLI, the MCP tools and the Python API — are generated from the code, and CI fails when one falls behind it.

**Architecture:** One script, `scripts/gen-reference.py`, renders each page from a hand-written template in `scripts/reference/` plus a source of truth: `mushroomdb --help`, the live `tools/list` reply, and `bindings/python/mushroomdb.pyi`. `--check` compares the result with the committed page. The Python page needs no binary and is checked with the other docs gates; the other two are checked in the CI job that already builds the binary.

**Tech Stack:** Python 3 standard library only (`ast`, `argparse`, `subprocess`, `unittest`), the existing `scripts/measure-tool-listing.py` for the MCP handshake, GitHub Actions.

**Spec:** `docs/roadmap/docs-overhaul-spec.md`, "Pull request 3 — the reference".

## Global Constraints

- Standard library only. No new dependency.
- A generated page is never edited by hand: its words live in `scripts/reference/<page>.md.tmpl`, its facts come from the code.
- Output must not depend on the Python version that runs the generator. Signatures are copied from the stub's source text, never re-printed with `ast.unparse`.
- `docs/site/*.md` stays flat; every new page is in `docs/site/SUMMARY.md`, `docs/site/index.md` and the `SITE_DOCS` list in `scripts/gen-llms-full.sh`.
- Nothing hand-written is deleted. The CLI table's prose moves into the CLI template; `mcp.md` and the Python how-to in `api.md` are left as they are.
- Every existing gate stays green: `build-site.sh`, `test-build-site.sh`, `test-check-links.sh`, `check-links.py`, `check-claims.sh`, `gen-llms-full.sh --check`, `bump-version.sh --check`, and `cargo test -p mushroomdb-server --test doc_strings`.
- Commits use conventional prefixes, lower case, in the repository's sentence style. Commit as the repository's configured git user, with no co-author trailer.

**Three deviations from the spec, for the owner to accept or change.**

1. **Pull request 3 is split in two.** This plan is 3a, the three generated pages. 3b is the written ones — the HTTP page checked against the router, TypeScript, Rust, Concepts and Vector search — and gets its own plan. Each half is reviewable alone.
2. **The generated Python page is `python-api.md`, beside the how-to, not instead of it.** The spec replaces `api.md`'s Python half with a generated page. That half is 600 lines of worked examples, which a page made from signatures and docstrings does not replace. It stays until 3b splits `api.md`; only its stale "Exposed surface" list, which names about 20 of 69 methods, gives way to a link.
3. **`mcp.md` keeps its tool tables for now.** They carry hand-written detail the server's own descriptions do not (the `remember` row runs to about 350 words). The generated `mcp-tools.md` is added beside them; pull request 4 removes the overlap.

## Review Focus

1. **A subcommand is added to the CLI's usage and nobody touches the docs.** Expected: the check fails and names the subcommand. Pinned in Task 2.
2. **A method is added to the Python stub and nobody assigns it a section.** Expected: the generator fails and names the method. Pinned in Task 1.
3. **A tool description or docstring contains `|`, a newline, or a bare `<T>`.** The first two break a Markdown table; the third is read by mdBook as an unclosed HTML tag. Expected: the page renders them as text. Pinned in Tasks 1 and 3.
4. **A tool argument's schema has no `type`** (it uses `anyOf` or `enum`, as `was_linked`'s `at_commit` does). Expected: the type column says what is accepted, not `None`. Pinned in Task 3.
5. **The generator is run for a binary-backed page with no `--binary`.** Expected: one line saying what it needs, exit 1, no traceback. Pinned in Task 2.

---

### Task 1: the generator, and the Python API page

**Files:**
- Create: `scripts/gen-reference.py`
- Create: `scripts/reference/python-api.md.tmpl`
- Create: `scripts/tests/test_gen_reference.py`
- Create: `docs/site/python-api.md` (generated)
- Modify: `docs/site/SUMMARY.md`, `docs/site/index.md`, `scripts/gen-llms-full.sh`
- Modify: `docs/site/api.md` (the "Exposed surface" section, about lines 1646–1662)
- Modify: `.github/workflows/ci.yml` (`plugin-validate` job)
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: nothing.
- Produces, in `scripts/gen-reference.py`: `class Problem(Exception)`; `fence(text: str, lang: str = "text") -> str`; `escape_html(text: str) -> str`; `cell(text: str) -> str`; `parse_stub(source: str) -> dict` with keys `aliases`, `errors`, `methods`; `render_python(template: str, stub: dict, groups: dict[str, list[str]] = GROUPS) -> str`; `generate(page: str, binary: str | None) -> str`; `main(argv: list[str]) -> int`. Tasks 2 and 3 add `render_cli` and `render_mcp` and extend `generate`. The command line is `gen-reference.py [--check] [--binary PATH] [PAGE ...]`, pages `cli`, `mcp-tools`, `python-api`, all three when none is named.

- [ ] **Step 1: Write the failing tests**

Create `scripts/tests/test_gen_reference.py`:

```python
"""Tests for scripts/gen-reference.py. Run: python3 -m unittest scripts/tests/test_gen_reference.py"""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
spec = importlib.util.spec_from_file_location("gen_reference", ROOT / "scripts" / "gen-reference.py")
gen = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gen)

STUB = '''
Row = dict[str, Any]
"""One result row."""

class MushroomError(RuntimeError):
    """Base class."""

    code: str | None

class KeyNotFound(MushroomError):
    """No node with this key.

    More detail that is not the summary.
    """

    code: str
    key: str

class GraphDb:
    """A store."""

    @staticmethod
    def open(path: str, read_only: bool = False) -> GraphDb:
        """Open the store at `path`."""

    def find(
        self,
        field: str,
        k: int = 10,
    ) -> list[tuple[str, float]]:
        """Find things.

        Returns a Vec<u32> of hits, or `Option<f64>` when asked.
        """

    def __enter__(self) -> GraphDb: ...
'''

TEMPLATE = "# T\n\n{{types}}\n\n{{methods}}\n\n{{errors}}\n"


class Helpers(unittest.TestCase):
    def test_a_bare_angle_bracket_is_escaped_and_one_in_code_is_not(self):
        self.assertEqual(gen.escape_html("a Vec<u32> and `Option<f64>`"), "a Vec\\<u32> and `Option<f64>`")

    def test_a_fenced_block_and_a_code_span_that_wraps_are_left_alone(self):
        fenced = "before <a>\n\n```python\nx: list[Vec<u32>] = []\n```\n\nrun `apply <db>\n--now` after"
        self.assertEqual(
            gen.escape_html(fenced),
            "before \\<a>\n\n```python\nx: list[Vec<u32>] = []\n```\n\nrun `apply <db>\n--now` after",
        )

    def test_a_table_cell_keeps_its_pipes_and_loses_its_newlines(self):
        self.assertEqual(gen.cell("one | two\n  three"), "one \\| two three")

    def test_a_fence_names_its_language(self):
        self.assertEqual(gen.fence("x", "python"), "```python\nx\n```")


class Stub(unittest.TestCase):
    def setUp(self):
        self.stub = gen.parse_stub(STUB)

    def test_methods_are_read_in_order_and_dunders_are_left_out(self):
        self.assertEqual([m[0] for m in self.stub["methods"]], ["open", "find"])

    def test_a_signature_is_the_stubs_own_text_with_its_decorator(self):
        self.assertEqual(self.stub["methods"][0][1], "@staticmethod\ndef open(path: str, read_only: bool = False) -> GraphDb")

    def test_a_signature_that_wraps_keeps_its_lines(self):
        self.assertEqual(
            self.stub["methods"][1][1],
            "def find(\n    self,\n    field: str,\n    k: int = 10,\n) -> list[tuple[str, float]]",
        )

    def test_an_error_lists_its_fields_without_code(self):
        self.assertEqual(self.stub["errors"][1][0], "KeyNotFound")
        self.assertEqual(self.stub["errors"][1][3], [("key", "str")])

    def test_an_alias_carries_the_docstring_under_it(self):
        self.assertEqual(self.stub["aliases"], [("Row", "dict[str, Any]", "One result row.")])


class Python(unittest.TestCase):
    def setUp(self):
        self.stub = gen.parse_stub(STUB)

    def test_the_page_has_each_method_under_its_group(self):
        page = gen.render_python(TEMPLATE, self.stub, {"Opening": ["open"], "Reading": ["find"]})
        self.assertIn("## Opening\n\n### `open`", page)
        self.assertIn("```python\n@staticmethod\ndef open(path: str, read_only: bool = False) -> GraphDb\n```", page)
        self.assertIn("Returns a Vec\\<u32> of hits", page)

    def test_a_method_in_no_group_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_python(TEMPLATE, self.stub, {"Opening": ["open"]})
        self.assertIn("find", str(raised.exception))

    def test_a_group_naming_a_method_the_stub_lacks_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_python(TEMPLATE, self.stub, {"All": ["open", "find", "gone"]})
        self.assertIn("gone", str(raised.exception))

    def test_an_error_row_is_its_summary_line_only(self):
        page = gen.render_python(TEMPLATE, self.stub, {"All": ["open", "find"]})
        self.assertIn("| `KeyNotFound` | `key: str` | No node with this key. |", page)
        self.assertNotIn("More detail", page)

    def test_a_template_missing_a_marker_fails(self):
        with self.assertRaises(gen.Problem):
            gen.render_python("# T\n\n{{methods}}\n", self.stub, {"All": ["open", "find"]})


class RealTree(unittest.TestCase):
    def test_every_method_in_the_real_stub_has_a_group(self):
        stub = gen.parse_stub((ROOT / "bindings" / "python" / "mushroomdb.pyi").read_text(encoding="utf-8"))
        names = [m[0] for m in stub["methods"]]
        grouped = [n for members in gen.GROUPS.values() for n in members]
        self.assertEqual(sorted(names), sorted(grouped))

    def test_the_committed_python_page_is_current(self):
        self.assertEqual(gen.main(["--check", "python-api"]), 0)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python3 -m unittest scripts/tests/test_gen_reference.py`
Expected: an error at import — `FileNotFoundError` for `scripts/gen-reference.py`.

- [ ] **Step 3: Write the generator**

Create `scripts/gen-reference.py`:

```python
#!/usr/bin/env python3
"""gen-reference.py — the reference pages that are generated, and their check.

    python3 scripts/gen-reference.py [--check] [--binary PATH] [PAGE ...]

Pages, and what each is generated from:

    cli         `mushroomdb --help`                       (needs --binary)
    mcp-tools   the live `tools/list` reply, both listings (needs --binary)
    python-api  bindings/python/mushroomdb.pyi

Each page is a hand-written template, scripts/reference/<page>.md.tmpl, with
the facts filled in. The words are the template's; a signature, an argument or
a subcommand is the code's, and cannot be written wrong in a page because it is
never written in one.

With no PAGE, all three. `--check` writes nothing and exits 1 if a page in
docs/site is not what the generator would write — and, before that, if the
code has grown something the template has no place for: a subcommand with no
`{{usage NAME}}` line, a stub method in no group.

Standard library only, and the output does not depend on the Python version:
signatures are copied from the stub's own text, never re-printed.
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEMPLATES = ROOT / "scripts" / "reference"
SITE = ROOT / "docs" / "site"
STUB = ROOT / "bindings" / "python" / "mushroomdb.pyi"

PAGES = ("cli", "mcp-tools", "python-api")
BINARY_PAGES = ("cli", "mcp-tools")


class Problem(Exception):
    """Something a person has to fix: said in one line, with no traceback."""


def fence(text: str, lang: str = "text") -> str:
    return f"```{lang}\n{text}\n```"


def escape_html(text: str) -> str:
    """`text` with every `<` outside a code span escaped.

    mdBook reads a bare `Vec<u32>` in prose as an unclosed HTML tag. Inside
    backticks it is already literal, and escaping it there would print the
    backslash.
    """
    parts = re.split(r"(`[^`]*`)", text)
    return "".join(p if p.startswith("`") else p.replace("<", "\\<") for p in parts)


def cell(text: str) -> str:
    """`text` as one Markdown table cell: one line, its pipes escaped."""
    return re.sub(r"\s*\n\s*", " ", text).replace("|", "\\|").strip()


def fill(template: str, values: dict[str, str]) -> str:
    """`template` with each `{{name}}` line replaced, every name required."""
    for name, value in values.items():
        marker = "{{" + name + "}}"
        if marker not in template:
            raise Problem(f"the template has no `{marker}` line")
        template = template.replace(marker, value)
    return template


# ── Python API ────────────────────────────────────────────────────────────────

#: The page's sections, and the stub methods under each. The stub is one flat
#: class; a reader needs it in parts. A method the stub gains has to be put in
#: one, and until it is the generator fails naming it.
GROUPS: dict[str, list[str]] = {
    "Opening and closing": ["open", "scoped", "refresh", "snapshot", "restore", "close"],
    "Writing": [
        "insert_node", "upsert_node", "set_prop", "set_props_many", "remove_prop", "delete_node",
        "rename_node", "insert_edge", "insert_edge_upsert", "delete_edge", "ingest_batch", "batch_edges",
    ],
    "Querying": [
        "query", "query_with_params", "query_write", "query_at", "neighbors", "node_info",
        "node_edges", "get_edge_prop", "degree", "degrees", "stats", "roles",
    ],
    "Rules": [
        "create_rule", "rules", "delete_rule", "rebuild_rule", "suggest_rules", "explain", "has_vector_rule",
    ],
    "History": [
        "node_history", "edge_history", "was_linked", "edges_at", "what_if_set_prop",
        "wal_total_commits", "record_commits_at", "resolve_date", "commit_time_ms",
    ],
    "Search and indexes": [
        "find_similar", "pairwise_similar", "search", "search_hybrid",
        "enable_fulltext", "disable_fulltext", "is_fulltext_enabled", "fulltext_pairs",
        "enable_index", "disable_index", "is_index_enabled",
        "enable_multiplicity", "is_multiplicity_enabled",
    ],
    "Graph algorithms": ["pagerank", "connected_components", "degree_centrality", "communities"],
    "Memory": ["remember", "recall", "upsert_entity", "forget", "schema_report", "identity_clusters"],
}


def parse_stub(source: str) -> dict:
    """The stub's type aliases, exception classes and `GraphDb` methods.

    aliases: (name, value, docstring)
    errors:  (name, base, docstring, [(field, annotation)]) — `code` left out,
             every class has it
    methods: (name, signature, docstring) — `__dunder__` methods left out
    """
    tree = ast.parse(source)
    lines = source.splitlines()
    segment = lambda node: ast.get_source_segment(source, node) or ""
    aliases, errors, methods = [], [], []
    for i, node in enumerate(tree.body):
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            after = tree.body[i + 1] if i + 1 < len(tree.body) else None
            doc = ""
            if isinstance(after, ast.Expr) and isinstance(after.value, ast.Constant) and isinstance(after.value.value, str):
                doc = after.value.value.strip()
            aliases.append((node.targets[0].id, segment(node.value), doc))
        elif isinstance(node, ast.ClassDef) and node.name == "GraphDb":
            for fn in node.body:
                if not isinstance(fn, ast.FunctionDef) or fn.name.startswith("__"):
                    continue
                first = min([fn.lineno] + [d.lineno for d in fn.decorator_list])
                text = "\n".join(lines[first - 1 : fn.body[0].lineno - 1])
                signature = re.sub(r"(?m)^    ", "", text).rstrip().removesuffix(":")
                methods.append((fn.name, signature, ast.get_docstring(fn) or ""))
        elif isinstance(node, ast.ClassDef):
            fields = [
                (f.target.id, segment(f.annotation))
                for f in node.body
                if isinstance(f, ast.AnnAssign) and isinstance(f.target, ast.Name) and f.target.id != "code"
            ]
            base = segment(node.bases[0]) if node.bases else ""
            errors.append((node.name, base, ast.get_docstring(node) or "", fields))
    return {"aliases": aliases, "errors": errors, "methods": methods}


def render_python(template: str, stub: dict, groups: dict[str, list[str]] = GROUPS) -> str:
    by_name = {m[0]: m for m in stub["methods"]}
    grouped = [name for members in groups.values() for name in members]
    ungrouped = [name for name in by_name if name not in grouped]
    unknown = [name for name in grouped if name not in by_name]
    if ungrouped:
        raise Problem(
            "the stub has methods in no section of the Python page — add each to GROUPS in "
            "scripts/gen-reference.py: " + ", ".join(ungrouped)
        )
    if unknown:
        raise Problem("GROUPS in scripts/gen-reference.py names methods the stub does not have: " + ", ".join(unknown))

    types = "\n\n".join(
        f"### `{name}`\n\n{fence(f'{name} = {value}', 'python')}\n\n{escape_html(doc)}".rstrip()
        for name, value, doc in stub["aliases"]
    )
    sections = []
    for title, members in groups.items():
        parts = [f"## {title}"]
        for name in members:
            _, signature, doc = by_name[name]
            parts.append(f"### `{name}`\n\n{fence(signature, 'python')}\n\n{escape_html(doc)}".rstrip())
        sections.append("\n\n".join(parts))
    rows = ["| Class | Fields | Raised when |", "|---|---|---|"]
    for name, _base, doc, fields in stub["errors"]:
        shown = ", ".join(f"`{field}: {annotation}`" for field, annotation in fields) or "—"
        summary = doc.split("\n\n", 1)[0]
        rows.append(f"| `{name}` | {cell(shown)} | {cell(escape_html(summary))} |")
    return fill(template, {"types": types, "methods": "\n\n".join(sections), "errors": "\n".join(rows)})


# ── The command ───────────────────────────────────────────────────────────────


def generate(page: str, binary: str | None) -> str:
    template = (TEMPLATES / f"{page}.md.tmpl").read_text(encoding="utf-8")
    if page == "python-api":
        return render_python(template, parse_stub(STUB.read_text(encoding="utf-8")))
    raise Problem(f"no generator for the page {page!r}")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="Generate, or check, the generated reference pages.")
    ap.add_argument("pages", nargs="*", metavar="PAGE", help="any of: " + ", ".join(PAGES) + " (default: all)")
    ap.add_argument("--check", action="store_true", help="write nothing; exit 1 if a page is stale")
    ap.add_argument("--binary", help="path to a built mushroomdb, for the pages that need one")
    args = ap.parse_args(argv)
    stale = False
    try:
        for page in args.pages or PAGES:
            if page not in PAGES:
                raise Problem(f"unknown page {page!r}; the pages are: " + ", ".join(PAGES))
            if page in BINARY_PAGES and not args.binary:
                raise Problem(f"the page {page!r} is generated from the binary: pass --binary <path to mushroomdb>")
            text = generate(page, args.binary)
            out = SITE / f"{page}.md"
            if not args.check:
                out.write_text(text, encoding="utf-8")
                print(f"gen-reference.py: wrote docs/site/{page}.md")
            elif not out.exists() or out.read_text(encoding="utf-8") != text:
                print(
                    f"gen-reference.py: docs/site/{page}.md is not what the generator writes. "
                    f"Regenerate it with scripts/gen-reference.py and commit the result.",
                    file=sys.stderr,
                )
                stale = True
            else:
                print(f"gen-reference.py: OK — docs/site/{page}.md is current")
    except Problem as problem:
        print(f"gen-reference.py: {problem}", file=sys.stderr)
        return 1
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
```

Make it executable: `chmod +x scripts/gen-reference.py`.

- [ ] **Step 4: Write the Python page's template**

Create `scripts/reference/python-api.md.tmpl`:

```markdown
<!-- Generated by scripts/gen-reference.py from bindings/python/mushroomdb.pyi.
     Do not edit this page: its words are in scripts/reference/python-api.md.tmpl,
     and its signatures and docstrings are the stub's. -->

# Python API

Every method of `mushroomdb.GraphDb`, with its signature and its docstring, as the type stub
gives them. Install it with `pip install mushroomdb`.

```python
import mushroomdb

db = mushroomdb.GraphDb.open("./memory")
db.insert_node("Person", "ada", {"name": "Ada", "skills": ["rust", "graphs"]})
print(db.query("MATCH (p:Person) RETURN p.name AS name"))
db.close()
```

This page is the reference. For worked examples — rules, vector search, scoped handles,
concurrency — see the Python sections of the [API guide](api.md#python-bindings).

A handle is also a context manager: `with mushroomdb.GraphDb.open(path) as db:` closes it on
exit.

## Types

{{types}}

{{methods}}

## Errors

Every error the engine raises is a subclass of `MushroomError`, which subclasses
`RuntimeError`. Each carries `code`, a stable snake_case string, and the fields below as
attributes. Classes may be added; a `code` is never respelled.

{{errors}}
```

- [ ] **Step 5: Run the unit tests, generate the page, run them again**

Run: `python3 -m unittest scripts/tests/test_gen_reference.py`
Expected: every test passes except `test_the_committed_python_page_is_current`, which fails because `docs/site/python-api.md` does not exist yet.

Run: `python3 scripts/gen-reference.py python-api`
Expected: `gen-reference.py: wrote docs/site/python-api.md`.

Run: `python3 -m unittest scripts/tests/test_gen_reference.py`
Expected: `OK`, 16 tests.

Check the page by eye: `grep -c '^### ' docs/site/python-api.md` prints 72 (69 methods and 3 types), and `grep -c '^| `' docs/site/python-api.md` prints 22 (the exception classes).

- [ ] **Step 6: Prove the two failure modes on the real tree**

```bash
python3 - <<'EOF'
import importlib.util
spec = importlib.util.spec_from_file_location("g", "scripts/gen-reference.py")
g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)
g.GROUPS["Memory"].remove("forget")
print(g.main(["--check", "python-api"]))
EOF
```

Expected: `gen-reference.py: the stub has methods in no section of the Python page — add each to GROUPS in scripts/gen-reference.py: forget`, then `1`.

Then: `printf '\n' >> docs/site/python-api.md && python3 scripts/gen-reference.py --check python-api; echo "exit=$?"; python3 scripts/gen-reference.py python-api`
Expected: `docs/site/python-api.md is not what the generator writes`, `exit=1`, and the last command restores the page.

- [ ] **Step 7: Put the page in the sidebar, the index and `llms-full.txt`**

In `docs/site/SUMMARY.md`, after the line `- [HTTP and Python API](api.md)`, add:

```markdown
- [Python API](python-api.md)
```

In `docs/site/index.md`, after the line `- [CLI reference](cli.md)`, add:

```markdown
- [Python API](python-api.md) — every method of `GraphDb`, generated from the type stub
```

In `scripts/gen-llms-full.sh`, in `SITE_DOCS`, after the line `    cli.md`, add `    python-api.md`.

Replace the "Exposed surface" section of `docs/site/api.md` — from the line `### Exposed surface` to the end of the file — with:

```markdown
### Exposed surface

Every method the binding exposes, with its signature and docstring, is in the
[Python API](python-api.md) reference, which is generated from the type stub.
```

Run: `bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check`
Expected: the second command exits 0.

- [ ] **Step 8: Build the site and run the gates**

Run:

```bash
bash scripts/build-site.sh
python3 scripts/check-links.py
bash scripts/check-claims.sh
cargo test -p mushroomdb-server --test doc_strings
```

Expected: `build-site.sh: OK — 33 pages`, and the other three green. If `build-site.sh` reports an mdBook warning naming `python-api.md`, a docstring has HTML-like text `escape_html` does not cover: add the failing text as a case to `test_a_bare_angle_bracket_is_escaped_and_one_in_code_is_not`, fix `escape_html`, and regenerate. If `doc_strings` fails, a refusal string it pins lived only in the removed "Exposed surface" text: restore that sentence under the new section.

- [ ] **Step 9: Check it in CI**

In `.github/workflows/ci.yml`, in the `plugin-validate` job, after the step `- run: bash scripts/gen-llms-full.sh --check`, add:

```yaml
      # The reference pages that are generated: the generator's own tests, and
      # the Python page against the stub. The CLI and MCP pages need the
      # binary, and are checked in mcp-handshake, which builds it.
      - run: python3 -m unittest scripts/tests/test_gen_reference.py
      - run: python3 scripts/gen-reference.py --check python-api
```

- [ ] **Step 10: Commit**

```bash
git add scripts/gen-reference.py scripts/reference/python-api.md.tmpl scripts/tests/test_gen_reference.py \
        docs/site/python-api.md docs/site/SUMMARY.md docs/site/index.md docs/site/api.md \
        scripts/gen-llms-full.sh llms-full.txt .github/workflows/ci.yml
git commit -m "docs(reference): the python api page is generated from the stub, and checked against it"
```

---

### Task 2: the CLI page is generated from `--help`

**Files:**
- Modify: `scripts/gen-reference.py` (add `parse_usage`, `render_cli`; extend `generate`)
- Modify: `scripts/tests/test_gen_reference.py`
- Create: `scripts/reference/cli.md.tmpl`
- Modify: `docs/site/cli.md` (becomes generated)
- Modify: `.github/workflows/ci.yml` (`mcp-handshake` job)
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: `Problem`, `fence`, `fill`, `generate`, `main` from Task 1.
- Produces: `parse_usage(help_text: str) -> tuple[dict[str, list[str]], str]` — the usage entries by subcommand name, in order, and the notes printed under them; `render_cli(template: str, help_text: str) -> str`. Template markers: one `{{usage NAME}}` line per subcommand, and `{{notes}}`.

- [ ] **Step 1: Build the binary the page is generated from**

Run: `cargo build -p mushroomdb-cli --bin mushroomdb && target/debug/mushroomdb --version`
Expected: `mushroomdb 0.7.1`. Every `--binary` below is `target/debug/mushroomdb`.

- [ ] **Step 2: Write the failing tests**

In `scripts/tests/test_gen_reference.py`, before the `RealTree` class, add:

```python
HELP = """mushroomdb — embedded graph database

Usage:
  mushroomdb serve <db-dir> [--addr 127.0.0.1:8080]
                     --addr is where it listens
  mushroomdb schema apply <db-dir> <schema.json>
  mushroomdb schema apply <db-dir> --memory-defaults
                                   applies the built-in memory schema
  mushroomdb --version

Default serve address is 127.0.0.1:8080.
A second line of notes.
"""

CLI_TEMPLATE = "# CLI\n\n{{usage serve}}\n\nServes.\n\n{{usage schema}}\n\n{{usage --version}}\n\n{{notes}}\n"


class Cli(unittest.TestCase):
    def test_entries_are_read_by_subcommand_with_their_continuation_lines(self):
        entries, _ = gen.parse_usage(HELP)
        self.assertEqual(list(entries), ["serve", "schema", "--version"])
        self.assertEqual(
            entries["serve"],
            ["mushroomdb serve <db-dir> [--addr 127.0.0.1:8080]\n                   --addr is where it listens"],
        )

    def test_a_subcommand_listed_twice_keeps_both_forms(self):
        entries, _ = gen.parse_usage(HELP)
        self.assertEqual(len(entries["schema"]), 2)
        self.assertTrue(entries["schema"][1].endswith("applies the built-in memory schema"))

    def test_the_notes_are_what_follows_the_usage(self):
        _, notes = gen.parse_usage(HELP)
        self.assertEqual(notes, "Default serve address is 127.0.0.1:8080.\nA second line of notes.")

    def test_help_with_no_usage_line_fails(self):
        with self.assertRaises(gen.Problem):
            gen.parse_usage("mushroomdb\n")

    def test_the_page_carries_each_usage_in_a_text_block(self):
        page = gen.render_cli(CLI_TEMPLATE, HELP)
        self.assertIn("```text\nmushroomdb schema apply <db-dir> <schema.json>\nmushroomdb schema apply <db-dir> --memory-defaults\n", page)
        self.assertIn("Serves.", page)
        self.assertIn("```text\nDefault serve address is 127.0.0.1:8080.\nA second line of notes.\n```", page)
        self.assertNotIn("{{", page)

    def test_a_subcommand_the_template_omits_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_cli(CLI_TEMPLATE.replace("{{usage schema}}\n\n", ""), HELP)
        self.assertIn("schema", str(raised.exception))

    def test_a_marker_for_a_subcommand_the_cli_lacks_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_cli(CLI_TEMPLATE + "\n{{usage gone}}\n", HELP)
        self.assertIn("gone", str(raised.exception))

    def test_a_binary_backed_page_without_a_binary_says_what_it_needs(self):
        self.assertEqual(gen.main(["--check", "cli"]), 1)
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `python3 -m unittest scripts/tests/test_gen_reference.py`
Expected: seven errors, `module 'gen_reference' has no attribute 'parse_usage'` / `'render_cli'`. `test_a_binary_backed_page_without_a_binary_says_what_it_needs` already passes: Task 1's `main` refuses it.

- [ ] **Step 4: Add the CLI functions**

In `scripts/gen-reference.py`, add `import subprocess` beside the other imports, and insert this section above the line `# ── Python API`:

```python
# ── CLI ───────────────────────────────────────────────────────────────────────

USAGE_ENTRY = re.compile(r"^  mushroomdb (\S+)")
USAGE_MARKER = re.compile(r"(?m)^\{\{usage (\S+)\}\}$")


def parse_usage(help_text: str) -> tuple[dict[str, list[str]], str]:
    """The usage entries by subcommand, in order, and the notes under them.

    The help is one block: a `Usage:` line, then an entry per form of each
    subcommand — `  mushroomdb NAME ...` and any deeper-indented lines that
    explain it — up to the first blank line. What follows is notes.
    A subcommand with several forms (`schema apply`, `algo`) has several
    entries under one name.
    """
    lines = [line.rstrip() for line in help_text.splitlines()]
    if "Usage:" not in lines:
        raise Problem("the help text has no `Usage:` line")
    blocks: dict[str, list[list[str]]] = {}
    current: list[str] | None = None
    i = lines.index("Usage:") + 1
    while i < len(lines) and lines[i]:
        match = USAGE_ENTRY.match(lines[i])
        if match:
            current = [lines[i][2:]]
            blocks.setdefault(match.group(1), []).append(current)
        elif current is None:
            raise Problem(f"a usage line comes before any subcommand: {lines[i]!r}")
        else:
            current.append(lines[i][2:])
        i += 1
    notes = "\n".join(lines[i:]).strip("\n")
    return {name: ["\n".join(block) for block in forms] for name, forms in blocks.items()}, notes


def render_cli(template: str, help_text: str) -> str:
    entries, notes = parse_usage(help_text)
    named = USAGE_MARKER.findall(template)
    missing = [name for name in entries if name not in named]
    unknown = [name for name in named if name not in entries]
    if missing:
        raise Problem(
            "the CLI lists subcommands scripts/reference/cli.md.tmpl has no `{{usage NAME}}` line for: "
            + ", ".join(missing)
        )
    if unknown:
        raise Problem(
            "scripts/reference/cli.md.tmpl has `{{usage NAME}}` lines for subcommands the CLI does not list: "
            + ", ".join(unknown)
        )
    page = USAGE_MARKER.sub(lambda m: fence("\n".join(entries[m.group(1)])), template)
    return fill(page, {"notes": fence(notes)})
```

In `generate`, before the final `raise Problem(...)`, add:

```python
    if page == "cli":
        done = subprocess.run([binary, "--help"], capture_output=True, text=True, timeout=60, check=True)
        return render_cli(template, done.stdout)
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `python3 -m unittest scripts/tests/test_gen_reference.py`
Expected: `OK`, 24 tests.

- [ ] **Step 6: Turn the hand-written table into the template**

`docs/site/cli.md` is a table moved out of the README: one row a subcommand, its second cell prose that `--help` does not carry (what `doctor` checks, how to quote a query, what `verify` exits with). This one-off script keeps every cell as a paragraph under its subcommand and puts a `{{usage NAME}}` line above it. Run it once from the repository root; it is not committed.

```bash
python3 - <<'EOF'
import re
from pathlib import Path

GROUPS = {
    "Set up an assistant": ["install", "uninstall", "disable", "enable", "doctor"],
    "Serve a store": ["serve", "mcp"],
    "Ask a store": ["query", "asof", "why", "stats", "suggest", "algo"],
    "Build and maintain a store": ["demo", "ingest-git", "schema", "build-index", "snapshot", "verify", "migrate", "backup", "export"],
    "Hook bodies": ["recall", "brief"],
    "Version and help": ["--version", "--help"],
}
INTRO = {
    "Hook bodies": "`install` wires these two into the assistant; they are not meant to be typed.",
}
EXTRA = {"--help": "Print the usage this page is generated from, and exit."}

source = Path("docs/site/cli.md").read_text(encoding="utf-8")
prose: dict[str, list[str]] = {}
for line in source.splitlines():
    row = re.match(r"^\| `mushroomdb (\S+?)[ `].*?` \| (.*) \|$", line)
    if row:
        prose.setdefault(row.group(1), []).append(row.group(2).replace("\\|", "|"))
after_table = source.split("\n\n**Concurrency:**", 1)[1]
concurrency = "**Concurrency:**" + after_table.split("\n\nFull HTTP endpoint reference", 1)[0]

named = [name for members in GROUPS.values() for name in members]
assert sorted(prose) == sorted(n for n in named if n != "--help"), (sorted(prose), sorted(named))

out = [
    "<!-- Generated by scripts/gen-reference.py from `mushroomdb --help`.",
    "     Do not edit this page: its words are in scripts/reference/cli.md.tmpl,",
    "     and each usage block is the binary's own. -->",
    "",
    "# CLI reference",
    "",
    "Every subcommand of the `mushroomdb` binary. Each usage block is what `mushroomdb --help`",
    "prints for it; the text under it is what the help leaves out.",
]
for title, members in GROUPS.items():
    out += ["", f"## {title}"]
    if title in INTRO:
        out += ["", INTRO[title]]
    for name in members:
        out += ["", f"### `{name}`", "", "{{usage " + name + "}}"]
        for paragraph in prose.get(name, [EXTRA.get(name, "")]):
            out += ["", paragraph]
out += [
    "", "## Defaults and resolution", "",
    "The notes `--help` prints under its usage:", "", "{{notes}}",
    "", "## Running several at once", "", concurrency.replace("**Concurrency:** ", "", 1),
    "", "The HTTP endpoints `serve` answers are in the [API reference](api.md).", "",
]
Path("scripts/reference").mkdir(exist_ok=True)
Path("scripts/reference/cli.md.tmpl").write_text("\n".join(out), encoding="utf-8")
print("subcommands:", len(named), "paragraphs:", sum(len(v) for v in prose.values()))
EOF
```

Expected: `subcommands: 26 paragraphs: 26` (the table has 26 rows over 25 subcommands — `algo` has two — and `--help`, the 26th subcommand, takes its line from `EXTRA`). If the `assert` fails it prints the two name lists: a row the pattern missed, or a subcommand with no row.

- [ ] **Step 7: Generate the page, and read it**

Run: `python3 scripts/gen-reference.py --binary target/debug/mushroomdb cli`
Expected: `gen-reference.py: wrote docs/site/cli.md`. If it instead names subcommands with no `{{usage NAME}}` line, the binary lists one the table never had: add a `### ` section for it to `scripts/reference/cli.md.tmpl` by hand, with one sentence saying what it does, and regenerate.

Read `docs/site/cli.md` top to bottom once. Every sentence of the old table must be there (`git diff --stat docs/site/cli.md` shows the table's 26 lines gone; `git diff docs/site/cli.md | grep -c '^+'` is well over 100). Check three by eye: `doctor`'s list of what it verifies, `query`'s quoting rule, `verify`'s `exit 2`.

- [ ] **Step 8: Prove the failure mode on the real tree**

```bash
sed -i.bak '/^{{usage doctor}}$/d' scripts/reference/cli.md.tmpl
python3 scripts/gen-reference.py --check --binary target/debug/mushroomdb cli; echo "exit=$?"
mv scripts/reference/cli.md.tmpl.bak scripts/reference/cli.md.tmpl
python3 scripts/gen-reference.py --check --binary target/debug/mushroomdb cli
```

Expected: the first check prints `the CLI lists subcommands scripts/reference/cli.md.tmpl has no {{usage NAME}} line for: doctor` and `exit=1`; the last prints `OK — docs/site/cli.md is current`.

- [ ] **Step 9: Regenerate `llms-full.txt`, build, run the gates**

```bash
bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check
bash scripts/build-site.sh
python3 scripts/check-links.py
bash scripts/check-claims.sh
```

Expected: all green; `build-site.sh: OK — 33 pages`. An mdBook warning naming `cli.md` means a table cell held a bare `<placeholder>` that the table's backticks used to protect: put that phrase in backticks in `scripts/reference/cli.md.tmpl` and regenerate.

- [ ] **Step 10: Check it in CI, where the binary is**

In `.github/workflows/ci.yml`, in the `mcp-handshake` job, directly after the step `- run: cargo build -p mushroomdb-cli --bin mushroomdb`, add:

```yaml
      # The CLI and MCP reference pages are generated from this binary. A
      # subcommand or a tool the pages do not have fails here, by name.
      - run: python3 scripts/gen-reference.py --check --binary target/debug/mushroomdb cli
```

- [ ] **Step 11: Commit**

```bash
git add scripts/gen-reference.py scripts/tests/test_gen_reference.py scripts/reference/cli.md.tmpl \
        docs/site/cli.md llms-full.txt .github/workflows/ci.yml
git commit -m "docs(reference): the cli page's usage is the binary's own, and a new subcommand fails the check"
```

---

### Task 3: the MCP tools page is generated from the live listing

**Files:**
- Modify: `scripts/gen-reference.py` (add `schema_type`, `render_tool`, `render_mcp`, `load_tools`; extend `generate`)
- Modify: `scripts/tests/test_gen_reference.py`
- Create: `scripts/reference/mcp-tools.md.tmpl`
- Create: `docs/site/mcp-tools.md` (generated)
- Modify: `docs/site/SUMMARY.md`, `docs/site/index.md`, `docs/site/mcp.md`, `scripts/gen-llms-full.sh`, `README.md`, `CONTRIBUTING.md`, `.github/workflows/ci.yml`
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: `Problem`, `cell`, `escape_html`, `fill`, `generate` from Task 1; `tools_list(binary: str, all_tools: bool) -> list[dict]` from `scripts/measure-tool-listing.py`, unchanged.
- Produces: `schema_type(prop: dict) -> str`; `render_tool(tool: dict, listed: bool) -> str`; `render_mcp(template: str, default_tools: list[dict], all_tools: list[dict]) -> str`. Template markers: `{{counts}}`, `{{task_tools}}`, `{{graph_tools}}`.

- [ ] **Step 1: Write the failing tests**

In `scripts/tests/test_gen_reference.py`, before the `RealTree` class, add:

```python
RECALL = {
    "name": "recall",
    "description": "What do I already know about this — ranked nodes.\nSecond line.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "topic": {"type": "string", "description": "Free text | any words."},
            "json": {"type": "boolean"},
        },
        "required": ["topic"],
    },
}
WAS_LINKED = {
    "name": "was_linked",
    "description": "Advanced: Were A and B linked on DATE.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "at_commit": {"anyOf": [{"type": "string"}, {"type": "integer"}], "description": "When."},
            "dir": {"enum": ["out", "in"]},
            "keys": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["at_commit"],
    },
}
STATS = {"name": "stats", "description": "Advanced: Live counts.", "inputSchema": {"type": "object", "properties": {}}}
MCP_TEMPLATE = "# Tools\n\n{{counts}}\n\n## Task\n\n{{task_tools}}\n\n## Graph\n\n{{graph_tools}}\n"


class Mcp(unittest.TestCase):
    def test_a_type_is_what_the_schema_accepts(self):
        self.assertEqual(gen.schema_type({"type": "string"}), "string")
        self.assertEqual(gen.schema_type({"anyOf": [{"type": "string"}, {"type": "integer"}]}), "string or integer")
        self.assertEqual(gen.schema_type({"enum": ["out", "in"]}), 'one of `"out"`, `"in"`')
        self.assertEqual(gen.schema_type({"type": "array", "items": {"type": "string"}}), "array of string")
        self.assertEqual(gen.schema_type({}), "any")

    def test_a_tool_has_its_arguments_in_a_table(self):
        text = gen.render_tool(RECALL, listed=True)
        self.assertIn("### `recall`", text)
        self.assertIn("| `topic` | string | yes | Free text \\| any words. |", text)
        self.assertIn("| `json` | boolean | no |  |", text)
        self.assertNotIn("--all-tools", text)

    def test_the_advanced_prefix_is_not_printed(self):
        text = gen.render_tool(WAS_LINKED, listed=True)
        self.assertIn("\nWere A and B linked on DATE.\n", text)
        self.assertIn("| `at_commit` | string or integer | yes | When. |", text)

    def test_an_unlisted_tool_says_how_to_list_it(self):
        self.assertIn("`--all-tools`", gen.render_tool(STATS, listed=False))

    def test_a_tool_with_no_arguments_says_so(self):
        self.assertIn("Takes no arguments.", gen.render_tool(STATS, listed=True))

    def test_the_page_sorts_tools_by_the_surface_they_belong_to(self):
        page = gen.render_mcp(MCP_TEMPLATE, [RECALL, WAS_LINKED], [RECALL, WAS_LINKED, STATS])
        self.assertIn("The server answers 3 tools and lists 2 of them by default.", page)
        task, graph = page.split("## Graph")
        self.assertIn("### `recall`", task)
        self.assertNotIn("### `was_linked`", task)
        self.assertIn("### `was_linked`", graph)
        self.assertIn("### `stats`", graph)
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python3 -m unittest scripts/tests/test_gen_reference.py`
Expected: six errors, `has no attribute 'schema_type'` / `'render_tool'` / `'render_mcp'`.

- [ ] **Step 3: Add the MCP functions**

In `scripts/gen-reference.py`, add `import importlib.util` and `import json` beside the other imports, and insert this section above the line `# ── Python API`:

```python
# ── MCP tools ─────────────────────────────────────────────────────────────────

#: A graph tool's description opens with this, so an assistant knows the task
#: tools are the front door. On a page that has a heading for each surface it
#: is noise.
ADVANCED = "Advanced: "


def schema_type(prop: dict) -> str:
    """What a JSON Schema property accepts, in words."""
    if "enum" in prop:
        return "one of " + ", ".join(f"`{json.dumps(value)}`" for value in prop["enum"])
    if "anyOf" in prop:
        return " or ".join(schema_type(option) for option in prop["anyOf"])
    kind = prop.get("type")
    if kind == "array":
        return "array of " + schema_type(prop.get("items", {}))
    if isinstance(kind, list):
        return " or ".join(kind)
    return kind or "any"


def render_tool(tool: dict, listed: bool) -> str:
    lines = [f"### `{tool['name']}`", "", escape_html(tool["description"].removeprefix(ADVANCED).strip()), ""]
    if not listed:
        lines += ["Listed only with `--all-tools`; the server answers it either way.", ""]
    schema = tool.get("inputSchema", {})
    properties = schema.get("properties", {})
    required = set(schema.get("required", []))
    if not properties:
        lines += ["Takes no arguments.", ""]
    else:
        lines += ["| Argument | Type | Required | What it is |", "|---|---|---|---|"]
        for name, prop in properties.items():
            needed = "yes" if name in required else "no"
            about = cell(escape_html(prop.get("description", "")))
            lines.append(f"| `{name}` | {cell(schema_type(prop))} | {needed} | {about} |")
        lines.append("")
    return "\n".join(lines)


def render_mcp(template: str, default_tools: list[dict], all_tools: list[dict]) -> str:
    listed = {tool["name"] for tool in default_tools}
    render = lambda tools: "\n".join(render_tool(tool, tool["name"] in listed) for tool in tools).rstrip()
    task = [tool for tool in all_tools if not tool["description"].startswith(ADVANCED)]
    graph = [tool for tool in all_tools if tool["description"].startswith(ADVANCED)]
    counts = f"The server answers {len(all_tools)} tools and lists {len(listed)} of them by default."
    return fill(template, {"counts": counts, "task_tools": render(task), "graph_tools": render(graph)})


def load_tools(binary: str, all_tools: bool) -> list[dict]:
    """The `tools` array of a real `mushroomdb mcp` on an empty store."""
    path = ROOT / "scripts" / "measure-tool-listing.py"
    spec = importlib.util.spec_from_file_location("measure_tool_listing", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.tools_list(binary, all_tools)
```

In `generate`, before the final `raise Problem(...)`, add:

```python
    if page == "mcp-tools":
        return render_mcp(template, load_tools(binary, False), load_tools(binary, True))
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python3 -m unittest scripts/tests/test_gen_reference.py`
Expected: `OK`, 30 tests.

- [ ] **Step 5: Write the template**

Create `scripts/reference/mcp-tools.md.tmpl`:

```markdown
<!-- Generated by scripts/gen-reference.py from the server's own `tools/list` reply.
     Do not edit this page: its words are in scripts/reference/mcp-tools.md.tmpl,
     and each description and argument is what the server sends a client. -->

# MCP tools

Every tool `mushroomdb mcp` serves, with the description and the arguments the server itself
sends to a client. {{counts}} `mushroomdb mcp <db> --all-tools` lists the rest.

This page is the reference. [Agent memory](mcp.md) is the guide: how to register the server,
a full session from `remember` to `explain_association`, and what each answer looks like.

An argument marked required must be sent; the others have defaults the description names.
Nothing here is an access boundary: `role`, `mask` and `namespace` ask to be answered as a
caller, and the stdio server holds no one to them. [Access control](masks.md) says where the
boundary is.

## Task tools

These answer a question in prose in one call, on any store, and are what the `/mushroom` skill
reaches for. Each also takes `json: true` for the raw report.

{{task_tools}}

## Graph tools

These reach the store directly. In the listing a client receives, each description opens with
`Advanced:`, so an assistant knows the task tools come first.

{{graph_tools}}
```

- [ ] **Step 6: Generate the page, and read it**

Run: `python3 scripts/gen-reference.py --binary target/debug/mushroomdb mcp-tools`
Expected: `gen-reference.py: wrote docs/site/mcp-tools.md`.

Check: `grep -c '^### ' docs/site/mcp-tools.md` prints 25; `grep -c 'Listed only with' docs/site/mcp-tools.md` prints 2 (`explain` and `rename_node`); the second sentence of the page reads `The server answers 25 tools and lists 23 of them by default.` Read `was_linked`'s table: `at_commit` is `string or integer`, required.

- [ ] **Step 7: Put the page in the sidebar, the index, the guide and the README**

In `docs/site/SUMMARY.md`, after the line `- [CLI](cli.md)`, add:

```markdown
- [MCP tools](mcp-tools.md)
```

In `docs/site/index.md`, after the line `- [CLI reference](cli.md)`, add:

```markdown
- [MCP tools](mcp-tools.md) — every tool with its arguments, generated from the server's own listing
```

In `scripts/gen-llms-full.sh`, in `SITE_DOCS`, after the line `    mcp.md`, add `    mcp-tools.md`.

In `docs/site/mcp.md`, directly under the heading `## Tool reference`, add:

```markdown
Every tool's arguments, with their types and which are required, are in the generated
[MCP tools](mcp-tools.md) reference. The tables here say what each tool is for.

```

In `README.md`, replace the line beginning `- **Reference:**` with:

```markdown
- **Reference:** [CLI](https://matthewsherlin.github.io/mushroomdb/docs/cli.html) · [MCP tools](https://matthewsherlin.github.io/mushroomdb/docs/mcp-tools.html) · [Python API](https://matthewsherlin.github.io/mushroomdb/docs/python-api.html) · [HTTP and Python guide](https://matthewsherlin.github.io/mushroomdb/docs/api.html) · [TypeScript client](clients/typescript/README.md)
```

In `CONTRIBUTING.md`, at the end of the section `### Plugin gate` (before the next `### ` heading), add:

```markdown
Three reference pages are generated — `docs/site/cli.md`, `mcp-tools.md` and `python-api.md` —
and CI fails when one is stale. After changing the CLI's usage, an MCP tool's description or
schema, or the Python stub, regenerate and commit:

```bash
cargo build -p mushroomdb-cli --bin mushroomdb
python3 scripts/gen-reference.py --binary target/debug/mushroomdb
bash scripts/gen-llms-full.sh
```

Their words live in `scripts/reference/*.md.tmpl`. A new subcommand needs a `{{usage NAME}}`
line there; a new stub method needs a section in `GROUPS` in `scripts/gen-reference.py`.
```

- [ ] **Step 8: Regenerate `llms-full.txt`, build, run every gate**

```bash
bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check
bash scripts/build-site.sh
bash scripts/tests/test-build-site.sh
bash scripts/tests/test-check-links.sh
python3 scripts/check-links.py
bash scripts/check-claims.sh
bash scripts/bump-version.sh --check
python3 -m unittest scripts/tests/test_gen_reference.py
python3 scripts/gen-reference.py --check --binary target/debug/mushroomdb
cargo test -p mushroomdb-server --test doc_strings
```

Expected: all green; `build-site.sh: OK — 34 pages`; the unit tests `OK`, 30 tests; the generator prints `OK — … is current` three times. An mdBook warning naming `mcp-tools.md` means a description has HTML-like text outside backticks that `escape_html` missed: add the text as a test case, fix, regenerate.

- [ ] **Step 9: Check it in CI**

In `.github/workflows/ci.yml`, in the `mcp-handshake` job, replace the line added in Task 2,

```yaml
      - run: python3 scripts/gen-reference.py --check --binary target/debug/mushroomdb cli
```

with:

```yaml
      - run: python3 scripts/gen-reference.py --check --binary target/debug/mushroomdb cli mcp-tools
```

- [ ] **Step 10: Look at the three pages rendered**

```bash
python3 -m http.server 8765 --directory _site >/dev/null 2>&1 &
```

Open `http://localhost:8765/docs/cli.html`, `mcp-tools.html` and `python-api.html`. Check: the sidebar shows each page's sections; a usage block does not wrap mid-flag on a desktop width and scrolls sideways on a phone width instead of pushing the page wider; an argument table's description column is readable; a wrapped Python signature keeps its line breaks. Stop the server: `kill %1`.

- [ ] **Step 11: Commit**

```bash
git add scripts/gen-reference.py scripts/tests/test_gen_reference.py scripts/reference/mcp-tools.md.tmpl \
        docs/site/mcp-tools.md docs/site/SUMMARY.md docs/site/index.md docs/site/mcp.md \
        scripts/gen-llms-full.sh llms-full.txt README.md CONTRIBUTING.md .github/workflows/ci.yml
git commit -m "docs(reference): the mcp tools page is the server's own listing, with every argument"
```

- [ ] **Step 12: After the pull request's CI**

`plugin-validate` runs the unit tests and the Python page's check; `mcp-handshake` runs the other two against a binary built on Linux. A red `mcp-handshake` naming `cli.md` or `mcp-tools.md` with a binary that built cleanly means the page differs between the macOS and Linux builds: read the job's diff before regenerating — the help text and the tool listing are not expected to vary by platform, so a difference is a finding, not noise.

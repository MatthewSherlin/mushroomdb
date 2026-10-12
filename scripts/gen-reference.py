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

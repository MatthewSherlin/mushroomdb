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
import importlib.util
import json
import re
import subprocess
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


# ── CLI ───────────────────────────────────────────────────────────────────────

USAGE_ENTRY = re.compile(r"^  mushroomdb (\S+)")
#: A line that names a subcommand, at any indent. Inside an entry it is a
#: subcommand indented wrongly; under the usage it is one cut off by a blank line.
USAGE_STRAY = re.compile(r"^\s*mushroomdb \S")
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
        elif USAGE_STRAY.match(lines[i]):
            raise Problem(
                "a subcommand in the help is not indented by two spaces, so it would be read as part of "
                f"the one above it: {lines[i].strip()!r}"
            )
        else:
            current.append(lines[i][2:])
        i += 1
    for line in lines[i:]:
        if USAGE_STRAY.match(line):
            raise Problem(
                "there is a blank line inside the help's usage block; everything after it would be read "
                f"as notes, starting with: {line.strip()!r}"
            )
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


def argument_rows(properties: dict, required: set[str], prefix: str) -> list[str]:
    """One table row an argument, and under an object — or an array of them —
    a row for each of its fields: `entities[].key`, `fact.subject`."""
    rows = []
    for name, prop in properties.items():
        needed = "yes" if name in required else "no"
        about = cell(escape_html(prop.get("description", "")))
        rows.append(f"| `{prefix}{name}` | {cell(schema_type(prop))} | {needed} | {about} |")
        inner, mark = (prop.get("items", {}), "[].") if prop.get("type") == "array" else (prop, ".")
        if isinstance(inner, dict) and inner.get("properties"):
            rows += argument_rows(inner["properties"], set(inner.get("required", [])), f"{prefix}{name}{mark}")
    return rows


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
        lines += argument_rows(properties, required, "")
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

    Anything else the stub holds is refused by name. A page that quietly left
    out a module-level function, an `async def` or the second of two
    overloads, or listed a `TypedDict` among the errors, would be a reference
    that is wrong without being stale.
    """
    tree = ast.parse(source)
    lines = source.splitlines()
    segment = lambda node: ast.get_source_segment(source, node) or ""
    aliases, errors, methods = [], [], []
    error_names = {"MushroomError"}
    for i, node in enumerate(tree.body):
        if isinstance(node, (ast.Import, ast.ImportFrom)):
            continue
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant) and isinstance(node.value.value, str):
            continue  # the module docstring, or the docstring under an alias
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            after = tree.body[i + 1] if i + 1 < len(tree.body) else None
            doc = ""
            if isinstance(after, ast.Expr) and isinstance(after.value, ast.Constant) and isinstance(after.value.value, str):
                doc = after.value.value.strip()
            aliases.append((node.targets[0].id, segment(node.value), doc))
        elif isinstance(node, ast.ClassDef) and node.name == "GraphDb":
            for fn in node.body:
                if isinstance(fn, ast.AsyncFunctionDef):
                    raise Problem(f"the stub declares `async def {fn.name}`, which the Python page has no form for")
                if not isinstance(fn, ast.FunctionDef) or fn.name.startswith("__"):
                    continue
                if any(fn.name == seen for seen, _, _ in methods):
                    raise Problem(f"the stub declares `{fn.name}` more than once, and the Python page has one entry a name")
                first = min([fn.lineno] + [d.lineno for d in fn.decorator_list])
                body = fn.body[0]
                # Up to where the body starts: the line above it, or — for
                # `def f(self) -> None: ...` — the column it starts at.
                text = "\n".join(lines[first - 1 : body.lineno - 1] + [lines[body.lineno - 1][: body.col_offset]])
                signature = re.sub(r"(?m)^    ", "", text).rstrip().removesuffix(":")
                methods.append((fn.name, signature, ast.get_docstring(fn) or ""))
        elif isinstance(node, ast.ClassDef):
            base = segment(node.bases[0]) if node.bases else ""
            if node.name not in error_names and base not in error_names:
                raise Problem(
                    f"the stub declares the class `{node.name}`, which is neither `GraphDb` nor an error "
                    "under `MushroomError`; the Python page has no section for it"
                )
            error_names.add(node.name)
            fields = [
                (f.target.id, segment(f.annotation))
                for f in node.body
                if isinstance(f, ast.AnnAssign) and isinstance(f.target, ast.Name) and f.target.id != "code"
            ]
            errors.append((node.name, base, ast.get_docstring(node) or "", fields))
        else:
            name = getattr(node, "name", None) or type(node).__name__
            raise Problem(f"the stub has a top-level `{name}` the Python page has no section for (line {node.lineno})")
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
    if page == "cli":
        done = subprocess.run([binary, "--help"], capture_output=True, text=True, timeout=60, check=True)
        return render_cli(template, done.stdout)
    if page == "mcp-tools":
        return render_mcp(template, load_tools(binary, False), load_tools(binary, True))
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
    except (OSError, subprocess.SubprocessError) as error:
        # A binary that is not there or exits non-zero, a template that is missing.
        print(f"gen-reference.py: {error}", file=sys.stderr)
        return 1
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

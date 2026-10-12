# Docs overhaul, pull request 3c — Concepts, Vector search, Rust and TypeScript: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Four new pages — how the engine works, vector search in one place, embedding it in Rust, and the TypeScript client — each with examples that a check runs or compiles.

**Architecture:** Every example on these pages is tied to something CI executes. Shell examples are run by a new script against a real `demo` store, and the output a page shows must be output the command printed. Rust examples are files under `crates/core-api/examples/` that CI builds and runs, quoted on the page between markers a test compares. The TypeScript page's method table is compared with the client class by a test in the client's own suite.

**Tech Stack:** Markdown; Python 3 standard library (`scripts/check-doc-examples.py`, `unittest`); Rust examples and one integration test in `crates/core-api`; one vitest test in `clients/typescript`; GitHub Actions.

**Spec:** `docs/roadmap/docs-overhaul-spec.md`, "Pull request 3 — the reference" and "The site's shape".

## Global Constraints

- A page states a fact once, from the code. Where existing pages disagree with each other, the new page takes the code's side; the stale pages are listed in this plan's last section for the tone pull request and are not edited here, apart from the two Rust sections that move.
- Every command a page shows is run by the check; every Rust block is compiled; nothing is shown as output that the command did not print.
- No new dependency in any crate or package.
- `docs/site/*.md` stays flat; every new page is in `SUMMARY.md`, `index.md` and `SITE_DOCS`.
- A heading that contains `{braces}` is written as a code span (`build-site.sh` refuses it otherwise).
- Nothing committed names another product as a comparison.
- Every existing gate stays green: `build-site.sh`, `test-build-site.sh`, `test-check-links.sh`, `check-links.py`, `check-claims.sh`, `gen-llms-full.sh --check`, `gen-reference.py --check`, `bump-version.sh --check`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, and `npm test` in `clients/typescript`.
- Commits use conventional prefixes, lower case, in the repository's sentence style, as the configured git user, with no co-author trailer.

## Review Focus

1. **A page shows output the command no longer prints.** Expected: the example check fails and names the page, the command and the missing line. Pinned in Task 1.
2. **A page's command exits non-zero.** Expected: the check fails with the command and what it printed. Pinned in Task 1.
3. **A Rust example on the page is edited and the file is not, or the reverse.** Expected: `cargo test` fails and names the example. Pinned in Task 4.
4. **A method is added to the TypeScript client and not to the page.** Expected: the client's `npm test` fails and names the method. Pinned in Task 5.
5. **A statement on a new page that is wrong against the code.** No test catches prose. The final review checks each page's claims against the source and a running store, as the HTTP page's review did. Named here so that it is done.

---

### Task 1: a check that runs a page's examples

**Files:**
- Create: `scripts/check-doc-examples.py`
- Create: `scripts/tests/test_check_doc_examples.py`
- Modify: `.github/workflows/ci.yml` (`plugin-validate` for the unit tests; `mcp-handshake` for the run, which needs the binary)

**Interfaces:**
- Consumes: nothing.
- Produces: `check-doc-examples.py --binary PATH PAGE...`. For each page, in order: every fenced block tagged `sh` is run line by line in one fresh temporary directory per page, each line that starts with `mushroomdb ` having that word replaced by the binary's path; a line that does not start with `mushroomdb ` is refused. When a `text` block follows an `sh` block directly, every non-blank line of it must be a line of what that `sh` block printed, compared with leading and trailing space removed; a line that is exactly `…` is skipped. An `sh` block may opt out with the info string `sh no-run`. Functions: `blocks(markdown: str) -> list[tuple[str, int, list[str]]]` (info string, 1-based line number, lines); `examples(markdown: str) -> list[tuple[int, list[str], list[str]]]` (line, commands, expected lines); `main(argv) -> int`. Tasks 2 and 3 add their pages to the CI line.

- [ ] **Step 1: Write the failing tests**

Create `scripts/tests/test_check_doc_examples.py`:

```python
"""Tests for scripts/check-doc-examples.py. Run: python3 -m unittest scripts/tests/test_check_doc_examples.py"""

from __future__ import annotations

import importlib.util
import stat
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
spec = importlib.util.spec_from_file_location("check_doc_examples", ROOT / "scripts" / "check-doc-examples.py")
chk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(chk)

F = "```"
PAGE = f"""# Page

{F}sh
mushroomdb demo ./db
mushroomdb stats ./db
{F}

{F}text
nodes: 60 live, 0 tombstoned
…
rules: 7
{F}

Prose in between.

{F}sh no-run
mushroomdb serve ./db
{F}

{F}sh
mushroomdb why ./db a b
{F}

{F}python
print("not a shell block")
{F}
"""


def fake_binary(directory: Path, script: str) -> str:
    """An executable that stands in for mushroomdb."""
    path = directory / "fake-mushroomdb"
    path.write_text("#!/bin/sh\n" + script, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return str(path)


class Parsing(unittest.TestCase):
    def test_blocks_carry_their_info_string_and_line(self):
        found = chk.blocks(PAGE)
        self.assertEqual([b[0] for b in found], ["sh", "text", "sh no-run", "sh", "python"])
        self.assertEqual(found[0][1], 3)
        self.assertEqual(found[0][2], ["mushroomdb demo ./db", "mushroomdb stats ./db"])

    def test_an_example_is_an_sh_block_and_the_text_block_right_after_it(self):
        found = chk.examples(PAGE)
        self.assertEqual(len(found), 2)
        self.assertEqual(found[0][1], ["mushroomdb demo ./db", "mushroomdb stats ./db"])
        self.assertEqual(found[0][2], ["nodes: 60 live, 0 tombstoned", "rules: 7"])
        self.assertEqual(found[1][2], [])

    def test_a_block_marked_no_run_is_not_an_example(self):
        self.assertNotIn(["mushroomdb serve ./db"], [e[1] for e in chk.examples(PAGE)])

    def test_a_text_block_after_prose_belongs_to_no_command(self):
        page = f"{F}sh\nmushroomdb stats ./db\n{F}\n\nWhich prints:\n\n{F}text\nnodes: 1\n{F}\n"
        self.assertEqual(chk.examples(page)[0][2], [])

    def test_a_command_that_is_not_mushroomdb_is_refused(self):
        with self.assertRaises(chk.Problem) as raised:
            chk.examples(f"{F}sh\nrm -rf ./db\n{F}\n")
        self.assertIn("rm -rf ./db", str(raised.exception))


class Running(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)

    def page(self, text: str) -> str:
        path = self.dir / "page.md"
        path.write_text(text, encoding="utf-8")
        return str(path)

    def test_output_the_page_shows_and_the_command_prints_passes(self):
        binary = fake_binary(self.dir, 'echo "  nodes: 60 live, 0 tombstoned"; echo "edges: 334"\n')
        page = self.page(f"{F}sh\nmushroomdb stats ./db\n{F}\n\n{F}text\nnodes: 60 live, 0 tombstoned\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 0)

    def test_output_the_command_does_not_print_fails(self):
        binary = fake_binary(self.dir, 'echo "nodes: 61 live, 0 tombstoned"\n')
        page = self.page(f"{F}sh\nmushroomdb stats ./db\n{F}\n\n{F}text\nnodes: 60 live, 0 tombstoned\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 1)

    def test_a_command_that_exits_non_zero_fails(self):
        binary = fake_binary(self.dir, "echo broken >&2; exit 3\n")
        page = self.page(f"{F}sh\nmushroomdb stats ./db\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 1)

    def test_quoted_arguments_reach_the_binary_as_one_argument(self):
        binary = fake_binary(self.dir, 'echo "argc=$#"; echo "last=$3"\n')
        page = self.page(
            f"{F}sh\nmushroomdb query ./db \"MATCH (p {{id: 'a'}}) RETURN p\"\n{F}\n\n"
            f"{F}text\nargc=3\nlast=MATCH (p {{id: 'a'}}) RETURN p\n{F}\n"
        )
        self.assertEqual(chk.main(["--binary", binary, page]), 0)

    def test_commands_on_one_page_share_a_directory(self):
        binary = fake_binary(self.dir, 'if [ "$1" = demo ]; then mkdir "$2"; else ls -d "$2"; fi\n')
        page = self.page(f"{F}sh\nmushroomdb demo ./db\n{F}\n\n{F}sh\nmushroomdb stats ./db\n{F}\n\n{F}text\n./db\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 0)

    def test_a_page_that_is_not_there_is_one_line_and_not_a_traceback(self):
        self.assertEqual(chk.main(["--binary", "/bin/echo", str(self.dir / "nope.md")]), 1)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python3 -m unittest scripts/tests/test_check_doc_examples.py`
Expected: an error at import — `FileNotFoundError` for `scripts/check-doc-examples.py`.

- [ ] **Step 3: Write the check**

Create `scripts/check-doc-examples.py`:

```python
#!/usr/bin/env python3
"""check-doc-examples.py — the commands a docs page shows are run, and the
output it shows is output they printed.

    python3 scripts/check-doc-examples.py --binary PATH PAGE...

In each page, every fenced block tagged `sh` is an example. Its lines are run
in order, in one empty temporary directory a page, so a page's commands build
on each other as a reader's would. Each line must start with `mushroomdb `,
which is replaced by PATH: a docs page that asked a reader to run anything
else would be run here too, and this check will not run it.

When a block tagged `text` follows an `sh` block directly, it is what the page
says those commands print. Every non-blank line of it has to be a line the
commands did print, compared without leading or trailing space. A page may
show part of the output; it may not show a line that was not printed. A line
that is exactly `…` stands for output left out.

A block tagged `sh no-run` is shown and not run: a server that never exits, a
command that needs the network.

Exits 1 on the first failure in each page, naming the page, the line of the
block, the command, and either what it printed or the line it did not print.
"""

from __future__ import annotations

import argparse
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

PROGRAM = "mushroomdb"
ELISION = "…"


class Problem(Exception):
    """Something a person has to fix: said in one line, with no traceback."""


def blocks(markdown: str) -> list[tuple[str, int, list[str]]]:
    """Every fenced block: (info string, 1-based line of its opening fence, lines)."""
    found: list[tuple[str, int, list[str]]] = []
    info: str | None = None
    start = 0
    body: list[str] = []
    for number, line in enumerate(markdown.splitlines(), 1):
        stripped = line.strip()
        if info is None:
            if stripped.startswith("```"):
                info, start, body = stripped[3:].strip(), number, []
        elif stripped == "```":
            found.append((info, start, body))
            info = None
        else:
            body.append(line)
    return found


def examples(markdown: str) -> list[tuple[int, list[str], list[str]]]:
    """(line, commands, expected output lines) for each `sh` block.

    The expected lines are those of a `text` block that starts within two
    lines of the `sh` block's closing fence — that is, directly after it, with
    at most a blank line between.
    """
    found = []
    all_blocks = blocks(markdown)
    for index, (info, start, body) in enumerate(all_blocks):
        if info != "sh":
            continue
        commands = [line.strip() for line in body if line.strip()]
        for command in commands:
            if not command.startswith(PROGRAM + " "):
                raise Problem(f"line {start}: an `sh` example runs something other than {PROGRAM}: {command!r}")
        expected: list[str] = []
        end = start + len(body) + 1
        if index + 1 < len(all_blocks):
            next_info, next_start, next_body = all_blocks[index + 1]
            if next_info == "text" and next_start <= end + 2:
                expected = [l.strip() for l in next_body if l.strip() and l.strip() != ELISION]
        found.append((start, commands, expected))
    return found


def run_page(page: Path, binary: str) -> None:
    text = page.read_text(encoding="utf-8")
    with tempfile.TemporaryDirectory() as cwd:
        for start, commands, expected in examples(text):
            printed: list[str] = []
            for command in commands:
                argv = [binary] + shlex.split(command)[1:]
                done = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, encoding="utf-8", timeout=300)
                if done.returncode != 0:
                    raise Problem(
                        f"line {start}: `{command}` exited {done.returncode}:\n{(done.stdout + done.stderr).strip()}"
                    )
                printed += [line.strip() for line in (done.stdout + done.stderr).splitlines()]
            for line in expected:
                if line not in printed:
                    raise Problem(
                        f"line {start}: the page shows a line the commands did not print: {line!r}\n"
                        "what they printed:\n" + "\n".join(printed)
                    )


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="Run the mushroomdb commands a docs page shows.")
    ap.add_argument("--binary", required=True, help="path to a built mushroomdb")
    ap.add_argument("pages", nargs="+", metavar="PAGE")
    args = ap.parse_args(argv)
    binary = str(Path(args.binary).resolve())
    failed = False
    for name in args.pages:
        try:
            run_page(Path(name), binary)
            print(f"check-doc-examples.py: OK — {name}")
        except Problem as problem:
            print(f"check-doc-examples.py: {name}: {problem}", file=sys.stderr)
            failed = True
        except (OSError, subprocess.SubprocessError) as error:
            print(f"check-doc-examples.py: {name}: {error}", file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
```

Make it executable: `chmod +x scripts/check-doc-examples.py`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python3 -m unittest scripts/tests/test_check_doc_examples.py`
Expected: `OK`, 11 tests.

- [ ] **Step 5: Run the unit tests in CI**

In `.github/workflows/ci.yml`, in the `plugin-validate` job, after the step `- run: python3 scripts/gen-reference.py --check python-api`, add:

```yaml
      # The check that runs the commands a docs page shows. Its own tests run
      # here; the run against a real binary is in mcp-handshake.
      - run: python3 -m unittest scripts/tests/test_check_doc_examples.py
```

- [ ] **Step 6: Commit**

```bash
git add scripts/check-doc-examples.py scripts/tests/test_check_doc_examples.py .github/workflows/ci.yml
git commit -m "test(docs): a check that runs the commands a page shows and compares what they print"
```

---

### Task 2: the Concepts page

**Files:**
- Create: `docs/site/concepts.md`
- Modify: `docs/site/SUMMARY.md`, `docs/site/index.md`, `scripts/gen-llms-full.sh`, `README.md`, `docs/landing/index.html`, `.github/workflows/ci.yml`
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: `check-doc-examples.py` from Task 1.
- Produces: `docs/site/concepts.md`, with these `##` headings that Tasks 3–5 link to: `A store is a directory`, `Nodes`, `Edges`, `Rules`, `Why two things are related`, `Time`, `Who can see what`, `Memory`, `Search`, `Words the other pages use`.

- [ ] **Step 1: Write the page**

Create `docs/site/concepts.md` with exactly this text. Its commands were run against `mushroomdb demo` on 2026-10-12, and the outputs are what they printed.

````markdown
# Concepts

How mushroomdb works, in the order you meet it. Every command on this page runs against the
small graph `mushroomdb demo` writes, and the output shown is what it prints.

## A store is a directory

There is no server to start and nothing to connect to. A store is a directory, and a program
opens it.

```sh
mushroomdb demo ./db
mushroomdb stats ./db
```

```text
nodes: 60 live, 0 tombstoned
edges: 334
history: complete (nothing pruned)
rules: 7
```

`demo` wrote ten organisations, twenty projects, thirty people, and seven rules. It wrote none
of the 334 edges: the rules did.

Inside the directory, `wal.bin` is the log of every write. `snapshot.bin`, once you take a
snapshot, is the whole store as one file that opens without being read through. `LOCK` is how
processes take turns: one writer at a time, any number of readers, and that holds across
processes as well as inside one.

## Nodes

A node has a **key**, a **label** and **properties**. The key is unique across the whole store,
not per label. A node has exactly one label, fixed when it is created. A property is an integer,
a float, a string, a boolean, a list or a map.

In a query, a node's key is written as its `id` property: `CREATE (n:Person {id: 'ada'})`
makes the node whose key is `ada`, and `MATCH (p:Person {id: 'person-01'})` finds one by key.
`key(n)` returns it.

Deleting a node **tombstones** it: it leaves the graph, and stays in the history.

## Edges

An edge has a **type**, a node it leaves and a node it reaches. It is directed, and there is at
most one of a type between the same two nodes in the same direction.

An edge gets there in one of two ways. You write it: a **hand-written** edge. Or a rule derives
it: a **derived** edge, which the rule owns. You cannot delete a derived edge yourself; the
store refuses. You change the data, or the rule, and the edge follows.

## Rules

A rule says which nodes should be linked. It names a source label, a destination label, a
**predicate** that compares a node of one with a node of the other, and the type of edge to
write when the predicate holds. There are six predicates:

| Predicate | Links two nodes when |
|---|---|
| `KeyMatch` | a field of one holds the other's key |
| `FieldEqual` | both hold the same value in a field |
| `Overlap` | their lists share enough items |
| `NumericWithin` | two numbers are within a tolerance |
| `GeoRadius` | two points are within a distance |
| `VectorSimilar` | two vectors are similar enough |

`All` and `Any` combine them. A rule can store the **score** of each match on its edge, and can
keep only the best few matches for each source node.

The demo's `skill_fit` rule links a person to every project that shares at least half of their
skills:

```sh
mushroomdb query ./db "MATCH (p:Person {id: 'person-01'})-[r:FIT]->(x:Project) RETURN x, r.score AS score ORDER BY score DESC, x"
```

```text
columns: x, score
x=proj-01  score=1.0
x=proj-02  score=0.5
x=proj-20  score=0.5
```

A rule is evaluated when it is created, against everything already in the store — the
**backfill** — and then on every insert and every property change, inside the write that made
the change. When a match stops holding, the rule **retracts** its edge in that same write.
Nothing is re-indexed afterwards and no job runs later. [Rules](rules.md) has each predicate
in full.

## Why two things are related

Every derived edge can say where it came from.

```sh
mushroomdb why ./db person-01 proj-01
```

```text
mushroomdb explain — person-01 ↔ proj-01: 2 relationship(s)
PROJECT via rule auto_fk_person_project_id (score 1.00) — key_match on project_id [project_id: proj-01]
FIT via rule skill_fit (score 1.00) — overlap on skills >= 0.5 [skills: s01, s02, s03]
```

Each line is one edge: its type, the rule that derived it, the score, the predicate, and in
brackets the **evidence** — the values the two nodes actually share. An assistant asked "why
are these two related?" can quote that instead of guessing.

Now change the person's skills, and ask again:

```sh
mushroomdb query ./db "MATCH (p:Person {id: 'person-01'}) SET p.skills = ['s20', 's21', 's22']"
mushroomdb why ./db person-01 proj-01
```

```text
mushroomdb explain — person-01 ↔ proj-01: 1 relationship(s)
PROJECT via rule auto_fk_person_project_id (score 1.00) — key_match on project_id [project_id: proj-01]
```

The `FIT` edge is gone. Nobody deleted it. The one `SET` made it untrue, and the rule retracted
it before the write returned.

## Time

A write does not replace what was there. It is appended to the log as a **commit**, and commits
are numbered from 0. So the store can be read as it was at any commit it still has — or on a
date, which resolves to the last commit at or before it.

The demo was 26 commits, 0 to 25. The `SET` above came after them, as two: the write itself,
and a record of what the rules did because of it. Read the graph as of commit 25, and the edge
is there:

```sh
mushroomdb asof ./db --commit 25 --query "MATCH (p:Person {id: 'person-01'})-[r:FIT]->(x:Project) RETURN x, r.score AS score ORDER BY score DESC, x"
```

```text
as-of commit 25 of 28
x=proj-01  score=1.0
x=proj-02  score=0.5
x=proj-20  score=0.5
```

History can be pruned, and a store says so when it has been. The oldest commit it still holds
is its **floor**; a read from before the floor is refused by name, never answered with a guess.
[Time travel](timetravel.md) has the other ways to ask: what edges a node had on a date,
whether two nodes were linked then, and every change to one node.

One thing that looks like time travel and is not: **what-if** computes the edges a property
change would gain and lose, and writes nothing.

## Who can see what

One store can answer different callers differently.

- A **namespace** is a tenancy boundary. A node is put in one when it is created and stays
  there; a node in none is in `default`. A hand-written edge cannot cross from one namespace to
  another.
- A **role** names what a caller may see — by label, by key, by a property test — and
  optionally what it may write. Roles are part of the store.
- A **mask** is a list of keys, for one call: only these.
- A **scope** binds a role, a namespace or a list of keys to a handle, so that every read
  through it obeys them. What a scope hides behaves exactly as if it did not exist.

Where this is enforced matters. Over HTTP, a role-bound token is held to its role by the
server. Over MCP and inside your own process, a role or a mask is a way of asking to be
answered as someone, and nothing stops a caller asking for more.
[Access control](masks.md) has the whole model.

## Memory

On top of the graph is a small surface for an assistant's memory, and it is how most people
meet mushroomdb.

- **`remember`** writes a note, the entities it names, and facts among them, in one commit.
  An entity it has never heard of is created as a **provisional** one, marked as such, rather
  than refused; it stops being provisional when something describes it.
- **`recall`** takes a question in ordinary words and returns what the store holds about it.
- **`forget`** removes a node, a property or a hand-written fact from the graph. It does not
  erase history: that takes pruning.

Entities are ordinary nodes — `Person`, `Org`, `Project`, `Concept`, `Event` — and notes are
`Note` nodes linked to what they are about. So rules apply to memory as they do to anything
else: when enough entities share a field, the store can propose a rule that links them, and
once you accept it every later `remember` maintains those links. [Agent memory](mcp.md) walks through a session.

## Search

Three ways to find a node without knowing its key.

- **Full-text.** You declare which fields are searchable, by label and field. `recall` and
  `search` rank matches in them.
- **Vector.** `find_similar` returns the nodes whose vector is nearest a query vector. You
  supply the vectors; the store does not compute them. [Vector search](vector.md)
- **Hybrid.** Both at once, fused into one ranking.

## Words the other pages use

| Word | Meaning |
|---|---|
| WAL | The write-ahead log, `wal.bin`: every commit since the last snapshot |
| commit | One write, as appended to the log; numbered from 0 |
| snapshot | The whole store written as one file, so that opening it does not replay the log |
| archive | A stretch of the log set aside by a snapshot, so that history still reaches it |
| floor, horizon | The oldest commit a store still holds; 0 when nothing has been pruned |
| backfill | A new rule's first evaluation, against what is already in the store |
| fires, retracts | A rule derives an edge; a rule removes one that stopped being true |
| tripped | Of a rule: it reached its ceiling on derived edges and stopped adding more |
| auto-FK | A `KeyMatch` rule made for you at ingest, when a field ending in `_id` holds another label's keys |
| top-k, `max_edges` | Keep only the k best-scoring edges for each source node |
| task tools, graph tools | Over MCP: the eleven tools that answer a question in prose, and the fourteen that reach the store directly |
| the brief | The store's schema in one block, given to an assistant when a session starts |
| the digest | The short text a task tool answers with |
| provisional | Of an entity: created because something mentioned it, before anything described it |
| tombstone | A deleted node: out of the graph, still in the history |
| never-widen | A mask, a role and a scope combine by intersection: adding one can only hide more |
| approximate | Of a vector rule: finds candidates through an index instead of comparing every pair |
````

- [ ] **Step 2: Run the page's examples**

Run: `cargo build -p mushroomdb-cli --bin mushroomdb && python3 scripts/check-doc-examples.py --binary target/debug/mushroomdb docs/site/concepts.md`
Expected: `check-doc-examples.py: OK — docs/site/concepts.md`. A failure prints the command and what it really printed: correct the page to the output, never the reverse. If `as-of commit 25 of 28` fails, the demo no longer writes 26 commits: run `mushroomdb demo ./x && mushroomdb asof ./x --commit 999` to read the range from its error, and change the number and the sentence above the example together.

- [ ] **Step 3: Prove the check catches a wrong page**

```bash
sed -i.bak 's/^x=proj-02  score=0.5$/x=proj-02  score=0.6/' docs/site/concepts.md
python3 scripts/check-doc-examples.py --binary target/debug/mushroomdb docs/site/concepts.md; echo "exit=$?"
mv docs/site/concepts.md.bak docs/site/concepts.md
```

Expected: `the page shows a line the commands did not print: 'x=proj-02  score=0.6'` and `exit=1`.

- [ ] **Step 4: Put the page where a reader starts**

In `docs/site/SUMMARY.md`, in the `# Start` group, after the line `- [Install](install.md)`, add `- [Concepts](concepts.md)`.

In `docs/site/index.md`, after the line beginning `- [Quickstart](quickstart.md)`, add:

```markdown
- [Concepts](concepts.md) — how it works: stores, nodes, rules, explain, time, access, memory
```

In `scripts/gen-llms-full.sh`, in `SITE_DOCS`, after the line `    install.md`, add `    concepts.md`.

In `README.md`, in the line beginning `- **Start:**`, add ` · [Concepts](https://matthewsherlin.github.io/mushroomdb/docs/concepts.html)` after the Install link.

In `docs/landing/index.html`, in the top bar, after the line `<a href="docs/quickstart.html">Quickstart</a>`, add `<a href="docs/concepts.html">Concepts</a>`.

- [ ] **Step 5: Run the examples in CI**

In `.github/workflows/ci.yml`, in the `mcp-handshake` job, after the `gen-reference.py --check` step, add:

```yaml
      # The commands the docs show, run against this binary on a demo store.
      - run: python3 scripts/check-doc-examples.py --binary target/debug/mushroomdb docs/site/concepts.md
```

- [ ] **Step 6: Regenerate, build, run the gates**

```bash
bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check
bash scripts/build-site.sh
python3 scripts/check-links.py
bash scripts/check-claims.sh
```

Expected: all green; `build-site.sh: OK — 37 pages`. Open `_site/docs/concepts.html` and read it once top to bottom at a phone width as well as a desktop one: the long query lines must scroll inside their block.

- [ ] **Step 7: Commit**

```bash
git add docs/site/concepts.md docs/site/SUMMARY.md docs/site/index.md scripts/gen-llms-full.sh llms-full.txt \
        README.md docs/landing/index.html .github/workflows/ci.yml
git commit -m "docs(concepts): how it works on one page, with every command run"
```

---

### Task 3: the Vector search page

**Files:**
- Create: `docs/site/vector.md`
- Modify: `docs/site/SUMMARY.md`, `docs/site/index.md`, `scripts/gen-llms-full.sh`, `.github/workflows/ci.yml`
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: `check-doc-examples.py` from Task 1; `concepts.md#search` from Task 2.
- Produces: `docs/site/vector.md`.

- [ ] **Step 1: Verify the Python example before writing it down**

The page has one Python example. Run it against the published wheel, in a scratch directory outside the repository:

```bash
python3 -m venv /tmp/vec-venv && /tmp/vec-venv/bin/pip install -q mushroomdb==0.7.1
cd "$(mktemp -d)" && /tmp/vec-venv/bin/python - <<'EOF'
from mushroomdb import GraphDb
db = GraphDb.open("./vecdb")
db.insert_node("Item", "a", {"embedding": [1.0, 0.0]})
db.insert_node("Item", "b", {"embedding": [0.9, 0.1]})
db.insert_node("Item", "c", {"embedding": [-1.0, 0.0]})
db.create_rule({
    "name": "similar_items", "src_label": "Item", "dst_label": "Item",
    "predicate": {"VectorSimilar": {"field": "embedding", "min": 0.8}},
    "edge_type": "SIMILAR", "weight_prop": "score",
})
print(db.neighbors("a", "SIMILAR", "out"))
print(db.find_similar("embedding", [1.0, 0.0], label="Item", k=3))
print(db.find_similar("embedding", [1.0, 0.0], label="Item", k=3, min=-1.0))
print(db.has_vector_rule("embedding"))
EOF
```

Expected: `['b']`; two hits, `a` at 1.0 and `b` near 0.994; three hits with `c` at -1.0; `False`. If `create_rule` refuses the dictionary for a missing key, add the keys it names (`max_edges`, `approximate`) with `None` and `False`, and use the form that ran. Write the page's example and its printed results from what this run printed.

- [ ] **Step 2: Write the page**

Create `docs/site/vector.md`. The sections, in order, and what each must say — every statement here was read from the source on 2026-10-12, at the place given:

1. **Title and opening.** `# Vector search`. Two sentences: a vector is an ordinary list-of-numbers property on a node; the store compares vectors and never computes them, so you bring embeddings from whatever model you use.

2. **`## Two things you can do with a vector`.** (a) Declare a `VectorSimilar` rule, and every pair of nodes whose vectors are similar enough is linked by a derived edge, kept current on every write. (b) Ask `find_similar` for the nodes nearest a query vector. They are independent: a search needs no rule, and a rule needs no search.

3. **`## A rule that links similar nodes`.** The demo store has one. Show, as an `sh` example with its `text` output:

   ```sh
   mushroomdb demo ./db
   mushroomdb query ./db "MATCH (p:Person {id: 'person-01'})-[r:SIMILAR]->(q:Person) RETURN q, r.score AS score ORDER BY score DESC, q LIMIT 3"
   ```

   with the three rows that prints (`q=person-11  score=1.0`, `q=person-21  score=1.0`, and the third as printed). Then: similarity is cosine, the only metric (`crates/core-rules/src/def.rs`); a rule's `min` must be above 0 and at most 1 (`def.rs:344-348`); the score is stored under the rule's `weight_prop`; with no `max_edges` a scored rule keeps the 32 best matches for each source node (`DEFAULT_SCORED_TOP_K`, `def.rs:218`); a rule between a label and itself links both directions. Then the Python example from Step 1 with what it printed, and the same rule as a `POST /rules` body. Two vectors of different lengths, a list with a non-number in it, and an all-zero vector produce no edge and no error (`def.rs:468-475`).

4. **`## Searching`.** A table of the three calls and where each lives:

   | Call | What it does | Python | MCP | HTTP |
   |---|---|---|---|---|
   | find similar | The `k` nodes nearest a query vector | `find_similar` | `find_similar` | `POST /find_similar` |
   | pairwise similar | Among keys you name, which are most alike; always exact | `pairwise_similar` | `pairwise_similar` | — |
   | hybrid | Full-text and vector results fused into one ranking | `search_hybrid` | `hybrid_search` | — |

   Then: `k` defaults to 10. `min` defaults to **0.8 on every surface** (`FIND_SIMILAR_DEFAULT_MIN`, `crates/core-api/src/lib.rs:55`), so a search that names no `min` returns only close matches and can return none; pass `min=0.0`, or lower, to see everything ranked. Scores are cosine similarity, from -1 to 1. Over MCP, `find_similar` has a second form: give it a `key` and an `edge_type` and it reads that node's derived edges instead of searching. Link each call to its entry in [Python API](python-api.md), [MCP tools](mcp-tools.md) and [HTTP API](http.md#post-find_similar).

5. **`## Exact and approximate`.** By default a search compares the query with every vector: exact, and linear in the number of nodes. It uses an index only when a rule over that field was declared with `approximate: true`; `has_vector_rule(field)` says whether one was. The demo's rule is not approximate, so searches on the demo are exact scans. With an index: `exact=True` forces the exact scan, and so does a `where` filter (`skip_hnsw = exact || where_.is_some()`, `crates/core-api/src/db.rs`). A mask or a scoped handle does **not**: the search stays approximate and widens until it has `k` visible hits, falling back to an exact scan of the visible nodes when widening is not enough. Say plainly what approximate costs: the index finds near neighbours, not provably the nearest, so use `exact=True` when a missed neighbour would matter.

6. **`## The index`.** It is an HNSW graph (`crates/core-rules/src/hnsw.rs`), built in the tree, with no dependency. Defaults: `m` 16, `m0` 64, `ef_construction` 200, `ef_search` 400 (`hnsw.rs:219-228`). They are store-wide and set only through the environment variable `MUSHROOMDB_HNSW_PARAMS=m,m0,ef_construction,ef_search`, read once at start; there is no per-rule setting. A rule created over more than 2,048 vectors builds its index in slices (`HNSW_BUILD_BATCH`, `crates/core-rules/src/index.rs:102`): `create_rule` returns before the edges exist, and they arrive in later commits. `mushroomdb build-index <db-dir>` drives a build to completion before traffic arrives. The index is saved in the snapshot, so a store opened from a snapshot does not rebuild it.

7. **`## What has been measured`.** CI holds the index to a recall floor on every pull request (job `recall-gates`): on 5,000 vectors of dimension 1536, recall at 10 must be at least 0.90 for every query and 0.95 on average, and measures 1.00 and 1.00. Say what that is and is not: uniform random unit vectors on a fixed seed, which is the easy case; on clustered vectors with clusters wider than the index's neighbour lists, the floor is 0.70 minimum and 0.95 mean, and it measures 0.80 and 0.995. Memory: the index holds its own 32-bit copy of each vector, about 6.7 kB a node at dimension 1536, beside the 64-bit values stored as properties. Largest index built: 50,000 vectors. Link [Benchmarks](benchmarks.md) and [Known limits](limits.md).

8. **`## Hybrid search`.** It needs the text field to be declared full-text (`enable_fulltext(label, field)`, or a schema); without that the text side returns nothing. It takes the top `4 × k` from each side and fuses them by reciprocal rank with a constant of 60 (`RRF_K`, `crates/core-api/src/db.rs`). An empty vector makes it a text-only search. It takes no `exact` argument and no filter. Link [Full-text search](fulltext.md).

9. **`## What it does not do`.** One short list: one metric, cosine; no embedding model; no quantisation; a filter on a search is one `eq` or `in` test on one field, and makes the search exact; index parameters are per store, not per rule.

Write it in the Concepts page's voice: present tense, what it does now, no version numbers, no file paths in the prose. The paths above are for checking, not for the page.

- [ ] **Step 3: Check every number on the page against the source**

For each of these, run the command and confirm the page says the same:

```bash
grep -n "FIND_SIMILAR_DEFAULT_MIN" crates/core-api/src/lib.rs
grep -n "const M\b\|const M0\|EF_CONSTRUCTION\|EF_SEARCH" crates/core-rules/src/hnsw.rs | head
grep -n "HNSW_BUILD_BATCH\|DEFAULT_SCORED_TOP_K" crates/core-rules/src/index.rs crates/core-rules/src/def.rs
grep -n "RRF_K\|4 \* k\|k \* 4" crates/core-api/src/db.rs | head
sed -n '/recall-gates:/,/format-compat:/p' .github/workflows/ci.yml | grep -n "0\.\(90\|95\|70\|80\|995\)\|1\.0000" | head -20
grep -n "6,663\|6663" docs/site/rules.md benchmarks/results/hnsw-scale-0.6.6.md | head -4
```

A number that does not match is changed on the page to what the source has.

- [ ] **Step 4: Run the page's example, wire the page in, add it to CI**

Run: `python3 scripts/check-doc-examples.py --binary target/debug/mushroomdb docs/site/vector.md`
Expected: `OK`.

In `docs/site/SUMMARY.md`, in `# Guides`, after `- [Full-text search](fulltext.md)`, add `- [Vector search](vector.md)`. In `docs/site/index.md`, after the line beginning `- [Full-Text Search](fulltext.md)`, add `- [Vector search](vector.md) — similarity rules, `find_similar`, exact and approximate, the index`. In `scripts/gen-llms-full.sh`, after `    fulltext.md`, add `    vector.md`. In `.github/workflows/ci.yml`, add ` docs/site/vector.md` to the end of the `check-doc-examples.py` line.

- [ ] **Step 5: Regenerate, build, gates, commit**

```bash
bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check
bash scripts/build-site.sh && python3 scripts/check-links.py && bash scripts/check-claims.sh
git add docs/site/vector.md docs/site/SUMMARY.md docs/site/index.md scripts/gen-llms-full.sh llms-full.txt .github/workflows/ci.yml
git commit -m "docs(vector): vector rules and search on one page, from the code"
```

Expected: all green; `build-site.sh: OK — 38 pages`.

---

### Task 4: the Rust page, with examples CI compiles and runs

**Files:**
- Create: `crates/core-api/examples/embed_basics.rs`, `crates/core-api/examples/embed_batch.rs`, `crates/core-api/examples/embed_shared.rs`
- Create: `crates/core-api/tests/doc_examples.rs`
- Create: `docs/site/rust.md`
- Modify: `docs/site/python.md` (the two "(Rust API)" sections leave), `docs/site/SUMMARY.md`, `docs/site/index.md`, `docs/site/api.md`, `scripts/gen-llms-full.sh`, `.github/workflows/ci.yml`
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: three examples, each with one region between the lines `// docs:start` and `// docs:end`; the test `the_rust_page_quotes_each_example_as_it_is`; `docs/site/rust.md`.

- [ ] **Step 1: Write the test that ties the page to the examples**

Create `crates/core-api/tests/doc_examples.rs`:

```rust
//! The Rust page quotes its examples from files that compile and run.
//!
//! Before this, nothing compiled a code block in the docs, and several had
//! stopped matching the API: a compare-and-set batch shown taking a closure,
//! a synchronous call shown with `.await`. Each example on
//! `docs/site/rust.md` is the region of a file under `examples/` between
//! `// docs:start` and `// docs:end`. CI builds and runs those files; this
//! test fails when the page's copy and the file's region differ.

use std::path::PathBuf;

const EXAMPLES: [&str; 3] = ["embed_basics", "embed_batch", "embed_shared"];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The lines between `// docs:start` and `// docs:end`, with the indent of
/// the first one removed from all of them.
fn quoted_region(source: &str, name: &str) -> String {
    let start = source
        .find("// docs:start")
        .unwrap_or_else(|| panic!("examples/{name}.rs has no `// docs:start` line"));
    let end = source
        .find("// docs:end")
        .unwrap_or_else(|| panic!("examples/{name}.rs has no `// docs:end` line"));
    let body: Vec<&str> = source[start..end].lines().skip(1).collect();
    let body = &body[..body.len().saturating_sub(1)];
    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    body.iter()
        .map(|l| if l.len() >= indent { &l[indent..] } else { l.trim_start() })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_region_is_dedented_and_excludes_its_markers() {
    let source = "fn main() {\n    // docs:start\n    let a = 1;\n\n    if a > 0 {\n        go();\n    }\n    // docs:end\n}\n";
    assert_eq!(
        quoted_region(source, "x"),
        "let a = 1;\n\nif a > 0 {\n    go();\n}"
    );
}

#[test]
fn the_rust_page_quotes_each_example_as_it_is() {
    let page_path = repo().join("docs/site/rust.md");
    let page = std::fs::read_to_string(&page_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", page_path.display()));
    for name in EXAMPLES {
        let path = repo().join(format!("crates/core-api/examples/{name}.rs"));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let region = quoted_region(&source, name);
        assert!(
            page.contains(&format!("```rust\n{region}\n```")),
            "docs/site/rust.md does not quote examples/{name}.rs as it is. The page's block must be exactly the lines between `// docs:start` and `// docs:end`:\n{region}"
        );
    }
}
```

Run: `cargo test -p mushroomdb --test doc_examples 2>&1 | tail -6`
Expected: `a_region_is_dedented_and_excludes_its_markers` passes; the other fails with `cannot read …/docs/site/rust.md`.

- [ ] **Step 2: Write the three examples**

Each is a complete program that opens a store in a temporary directory, does its work between the markers, asserts what the page claims, and removes the directory. Write them to compile against the signatures in `crates/core-api/src/db.rs` and `shared.rs` as they are today; the bodies below are the intent, and the compiler is the authority — where a call does not compile as written here, fix the example, not the API, and keep the assertions.

`crates/core-api/examples/embed_basics.rs` — open, insert, a rule, a query, explain, a retraction:

```rust
//! The smallest embedding of mushroomdb: nodes, one rule, a query, and why.
//! Quoted on docs/site/rust.md. Run: cargo run -p mushroomdb --example embed_basics

use core_api::{GraphDb, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

fn skills(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

fn main() -> core_api::Result<()> {
    let dir = std::env::temp_dir().join(format!("mushroomdb-embed-basics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // docs:start
    let mut db = GraphDb::open(&dir)?;

    db.insert_node("Org", "acme", vec![("skills".into(), skills(&["graph", "rust", "search"]))])?;
    db.insert_node("Person", "ada", vec![("skills".into(), skills(&["graph", "rust"]))])?;

    // Link every Person to every Org sharing at least half of their skills.
    db.create_rule(RuleDef {
        name: "skill_fit".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::Overlap { field: "skills".into(), min: 0.5 },
        edge_type: "FIT".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    })?;

    // The rule derived the edge; nobody inserted it.
    let rows = db.query(
        "MATCH (p:Person)-[r:FIT]->(o:Org) RETURN p, o, r.score AS score",
        &BTreeMap::new(),
    )?;
    assert_eq!(rows.len(), 1);

    // Why are they linked? The rule, the edge type and the score.
    let why = db.explain("ada", "acme")?;
    assert_eq!(why[0].rule, "skill_fit");

    // Change the property and the edge is retracted in the same write.
    db.set_prop("ada", "skills", skills(&["cooking"]))?;
    assert!(db.explain("ada", "acme")?.is_empty());
    // docs:end

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
```

`crates/core-api/examples/embed_batch.rs` — an atomic batch, and a compare-and-set batch that is refused when the node changed:

```rust
//! Several writes as one commit, and a write that is refused if a node moved.
//! Quoted on docs/site/rust.md. Run: cargo run -p mushroomdb --example embed_batch

use core_api::{BatchOp, GraphDb, GraphError, Precondition, Value};

fn main() -> core_api::Result<()> {
    let dir = std::env::temp_dir().join(format!("mushroomdb-embed-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir)?;

    // docs:start
    // One WAL frame, one fsync: after a crash, all of it is there or none.
    db.write_batch(|batch| {
        batch
            .insert_node("Person", "ada", vec![("name".into(), Value::Str("Ada".into()))])
            .insert_node("Person", "grace", vec![("name".into(), Value::Str("Grace".into()))])
            .insert_edge("KNOWS", "ada", "grace");
    })?;

    // Compare-and-set: write only if `ada` has not changed since we read it.
    let seen = db.last_changed("ada").expect("ada exists");
    db.set_prop("ada", "name", Value::Str("Ada L.".into()))?; // someone else writes first

    let refused = db.write_batch_cas(
        vec![Precondition::NodeUnchangedSince { key: "ada".into(), expected: seen }],
        vec![BatchOp::SetProp { key: "ada".into(), field: "name".into(), value: Value::Str("A.".into()) }],
    );
    assert!(matches!(refused, Err(GraphError::CasConflict { .. })));
    // docs:end

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
```

`crates/core-api/examples/embed_shared.rs` — one store shared between threads:

```rust
//! One store, several threads: `SharedDb`.
//! Quoted on docs/site/rust.md. Run: cargo run -p mushroomdb --example embed_shared

use core_api::{BatchOp, SharedDb, Value};
use std::collections::BTreeMap;

fn main() -> core_api::Result<()> {
    let dir = std::env::temp_dir().join(format!("mushroomdb-embed-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // docs:start
    let db = SharedDb::open(&dir)?;

    // Writers on any thread; concurrent batches share one fsync.
    let writers: Vec<_> = (0..4)
        .map(|n| {
            let db = db.clone();
            std::thread::spawn(move || {
                db.submit_batch(vec![BatchOp::InsertNode {
                    label: "Job".into(),
                    key: format!("job-{n}"),
                    props: vec![("n".into(), Value::Int(n))],
                }])
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer thread")?;
    }

    // Readers take a guard, never wait on each other, and see every commit.
    let rows = db.read().query("MATCH (j:Job) RETURN j", &BTreeMap::new())?;
    assert_eq!(rows.len(), 4);
    // docs:end

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
```

Run each, then the formatter and the lints:

```bash
for e in embed_basics embed_batch embed_shared; do cargo run -q -p mushroomdb --example $e && echo "$e ok"; done
cargo fmt --all
cargo clippy -p mushroomdb --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: `embed_basics ok`, `embed_batch ok`, `embed_shared ok`, and no warnings. `cargo fmt` will re-wrap the long lines; the page quotes the file as formatted. An assertion that fails is the example saying the page's claim is wrong: find out which, from the code, before changing either.

- [ ] **Step 3: Confirm what a user of the published crate writes**

The crate is published as `mushroomdb` and its library target is named `core_api`. In a scratch directory outside the repository:

```bash
cd "$(mktemp -d)" && cargo init -q --name probe && cargo add -q mushroomdb@0.7.1 \
  && printf 'use core_api::GraphDb;\nfn main() { let _ = GraphDb::open(std::path::Path::new("./x")); }\n' > src/main.rs \
  && cargo build -q 2>&1 | tail -3; echo "exit=$?"
```

Expected: `exit=0`, which confirms the import is `use core_api::…`. The page says so in its first section, in one plain sentence: the crate is installed as `mushroomdb` and imported as `core_api`. If it does not build, try `use mushroomdb::GraphDb;` and state whichever compiles.

- [ ] **Step 4: Write the page**

Create `docs/site/rust.md`. Sections, in order:

1. `# Rust`. Opening: mushroomdb is a Rust library first; the CLI, the servers and the Python module are built on the crate this page describes. `cargo add mushroomdb`; the import path from Step 3; Rust 1.92 or later; the full API is on [docs.rs](https://docs.rs/mushroomdb).
2. `## The shortest program` — the `embed_basics` region in a `rust` block, exactly as the file has it, then four sentences: `GraphDb::open` creates the directory if needed and holds the store's write lock for as long as the handle lives; properties are a `Vec<(String, Value)>`, and `Value` is `Int`, `Float`, `Str`, `Bool`, `List` or `Map`; `RuleDef` has no default, so every field is written out; `query` returns a `ResultSet` read by `len()`, `columns()` and `get(row, column)`.
3. `## Opening a store` — a table of the ways: `GraphDb::open(dir)`; `GraphDb::open_with_options(dir, OpenOptions { read_only: true, ..Default::default() })` for a handle that never takes the lock and refuses writes with `GraphError::ReadOnly`; `GraphDb::open_at(dir, commit)` for a read-only view at a past commit; `SharedDb::open(dir)` for several threads. One sentence on `OpenOptions`'s other two fields, `auto_migrate` and `repair_wal`, both true by default.
4. `## Several writes as one` — the `embed_batch` region, then: a batch is validated, then applied as one WAL frame with one fsync, so it is all-or-nothing across a crash; it is not isolated — a reader in the same process may see it part-applied; `write_batch_cas` takes its operations as a `Vec<BatchOp>`, not a closure, and fails with `GraphError::CasConflict` naming the key; create a rule in a batch of its own.
5. `## One store, several threads` — the `embed_shared` region, then: `SharedDb` is `Clone`, `Send` and `Sync`, and takes the cross-process lock for each write instead of for its lifetime, so other processes can write between yours; `read()` returns a guard and must not be held while calling the same handle again; `write()` waits two seconds for the lock; `submit_batch` blocks until the group's fsync; `reader()` returns a `ReaderSnapshot`, a view that does not move while you hold it.
6. `## Durability` — `FsyncPolicy` is `Strict` (the default: every commit is fsynced), `Batched` or `Relaxed`, set with `db.set_fsync_policy(..)`; `db.snapshot()` writes the store as one file so that the next open does not replay the log. Link [Durability](durability.md).
7. `## Errors` — every call returns `core_api::Result<T>`; the variants worth matching: `KeyNotFound`, `DuplicateKey`, `ReadOnly`, `Busy`, `CasConflict`, `QueryError`, `CommitOutOfRange`, `RuleOwned`. `Busy` means another process is writing right now, and retrying is always safe.
8. `## The rest` — one line each with a link: subscriptions (`subscribe_rule`, [Subscriptions](subscriptions.md)); schema as a value (`Schema`, `apply_schema`); materialized views ([Views](views.md)); graph algorithms ([Algorithms](algorithms.md)); history ([Time travel](timetravel.md)); roles and scopes ([Access control](masks.md)).

Run: `cargo test -p mushroomdb --test doc_examples 2>&1 | grep "test result"`
Expected: `ok. 2 passed`.

- [ ] **Step 5: Move the two Rust sections out of the Python guide**

`docs/site/python.md` has two sections that are not Python: `## Atomic write batches (Rust API)` and `## Compare-and-set write batches (Rust API)`. The second shows `write_batch_cas` taking a closure, which does not compile. Delete both sections from `python.md`. In their place leave one paragraph under a heading `## Atomic writes from Python`, keeping the sentences from the first section that are about Python — that `write_batch` is not exposed there, and that `ingest_batch` and `batch_edges` are the atomic Python writes — and ending with a link: "The Rust forms, including compare-and-set, are on the [Rust](rust.md#several-writes-as-one) page." Check that nothing else on the page linked to the two removed headings: `python3 scripts/check-links.py`.

- [ ] **Step 6: Wire the page in, and run the examples in CI**

In `docs/site/SUMMARY.md`, in `# Reference`, after `- [Python API](python-api.md)`, add `- [Rust](rust.md)`. In `docs/site/index.md`, after the line beginning `- [Python API](python-api.md)`, add `- [Rust](rust.md) — embedding the engine: the types you touch, with examples that are compiled`. In `docs/site/api.md`, add a row to the table: `| Rust, `cargo add mushroomdb` | [docs.rs](https://docs.rs/mushroomdb) | [Rust](rust.md) |`. In `scripts/gen-llms-full.sh`, after `    python-api.md`, add `    rust.md`.

In `.github/workflows/ci.yml`, in the `test` job, after the step `- run: cargo test --workspace` (find it with `grep -n "cargo test --workspace" .github/workflows/ci.yml`), add:

```yaml
      # The examples docs/site/rust.md quotes. They are built with every other
      # example; these lines run them, so each one's assertions are checked.
      - run: cargo run -q -p mushroomdb --example embed_basics
      - run: cargo run -q -p mushroomdb --example embed_batch
      - run: cargo run -q -p mushroomdb --example embed_shared
```

- [ ] **Step 7: Prove the tie, then run the gates**

```bash
sed -i.bak 's/assert_eq!(rows.len(), 4);/assert_eq!(rows.len(), 5);/' docs/site/rust.md
cargo test -p mushroomdb --test doc_examples 2>&1 | grep -c "does not quote examples/embed_shared.rs"
mv docs/site/rust.md.bak docs/site/rust.md
bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check
bash scripts/build-site.sh && python3 scripts/check-links.py && bash scripts/check-claims.sh
cargo fmt --all -- --check && cargo test -p mushroomdb --test doc_examples 2>&1 | grep "test result"
```

Expected: `1` from the grep, then all green; `build-site.sh: OK — 39 pages`.

- [ ] **Step 8: Commit**

```bash
git add crates/core-api/examples/embed_basics.rs crates/core-api/examples/embed_batch.rs \
        crates/core-api/examples/embed_shared.rs crates/core-api/tests/doc_examples.rs \
        docs/site/rust.md docs/site/python.md docs/site/api.md docs/site/SUMMARY.md docs/site/index.md \
        scripts/gen-llms-full.sh llms-full.txt .github/workflows/ci.yml
git commit -m "docs(rust): the page a rust user starts from, quoting examples that are built and run"
```

---

### Task 5: the TypeScript page

**Files:**
- Create: `docs/site/typescript.md`
- Create: `clients/typescript/tests/docs.test.ts`
- Modify: `clients/typescript/README.md`, `docs/site/api.md`, `docs/site/SUMMARY.md`, `docs/site/index.md`, `scripts/gen-llms-full.sh`, `README.md`
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: `docs/site/http.md`'s route headings (anchors of the form `#post-query`).
- Produces: `docs/site/typescript.md`, whose method table has one row for each public method of `MushroomClient`, written `` `client.<name>(` ``.

- [ ] **Step 1: Write the failing test**

Look at how the existing suite is set up first: `cat clients/typescript/vitest.config.* 2>/dev/null; sed -n 1,30p clients/typescript/tests/global-setup.ts`. The existing test file is an integration test that needs a running server. This one needs none; if the config's `globalSetup` starts a server for every test file, that is acceptable — the test simply does not use it.

Create `clients/typescript/tests/docs.test.ts`:

```ts
// The TypeScript page names every method the client has.
//
// docs/site/typescript.md has a table with one row a public method of
// MushroomClient, written `client.<name>(`. A method added to the class and
// not to the page fails here by name; so does a row for a method that is gone.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { MushroomClient } from "../src/index";

const here = dirname(fileURLToPath(import.meta.url));
const page = readFileSync(join(here, "../../../docs/site/typescript.md"), "utf8");

const methods = Object.getOwnPropertyNames(MushroomClient.prototype)
  .filter((name) => name !== "constructor" && !name.startsWith("_"))
  .filter((name) => name !== "fetchJson")
  .sort();

const documented = [...page.matchAll(/`client\.([A-Za-z]+)\(/g)].map((m) => m[1]);

describe("docs/site/typescript.md", () => {
  it("finds the client's methods at all", () => {
    expect(methods.length).toBeGreaterThanOrEqual(12);
  });

  it("has a row for every public method of MushroomClient", () => {
    expect(methods.filter((name) => !documented.includes(name))).toEqual([]);
  });

  it("has no row for a method the client does not have", () => {
    expect([...new Set(documented)].filter((name) => !methods.includes(name))).toEqual([]);
  });
});
```

`fetchJson` is declared `private` in TypeScript, which is not enforced at run time, so it is filtered by name. If `Object.getOwnPropertyNames` shows other private helpers, add them to that filter with a comment saying they are private.

Run: `cd clients/typescript && npm ci && npx vitest run tests/docs.test.ts 2>&1 | tail -12`
Expected: the file fails to load — `ENOENT … docs/site/typescript.md`.

- [ ] **Step 2: Write the page**

Create `docs/site/typescript.md`. Read `clients/typescript/src/client.ts` and `src/types.ts` first, for the exact option names. Sections:

1. `# TypeScript client`. `mushroomdb-client` is a typed wrapper over the [HTTP API](http.md) and its WebSocket streams. It talks to a running `mushroomdb serve`; it does not embed the engine. `npm install mushroomdb-client`. Node 18 or later; in Node before 21, install `ws` for subscriptions.
2. `## Quick start` — the client constructed with a token, one `query`, one parameterised `query`, one `explain`: lifted from the README's quick start, shortened to those three calls.
3. `## Methods` — a table, one row a method, columns: the call written as `` `client.query(` `` and so on; what it does in a phrase; the route it calls, linked to its section on the HTTP page. The twelve rows: `query` → `POST /query`; `ingest` → `POST /ingest`; `stats` → `GET /stats`; `suggest` → `GET /suggest`; `explain` → `GET /explain`; `createRule` → `POST /rules`; `node` → `GET /node/{key}`; `neighborhood` → `GET /node/{key}/neighborhood`; `algo` → `POST /algo/pagerank`, `/algo/wcc`, `/algo/degree`; `renameNode` → `POST /nodes/{key}/rename`; `upsertEdge` → `POST /edges/upsert`; `subscribe` → `GET /subscribe`. Confirm each against `client.ts` — the path string in each method — before writing its row.
4. `## What it does not wrap` — the routes with no method, listed plainly so nobody searches for them: `GET /health`, `GET /metrics`, `POST /find_similar`, `GET /node/{key}/edges`, `GET /node/{key}/history`, `GET /history/edge`, `GET /history/was_linked`, `POST /nodes`, `DELETE /node/{key}`, both property routes, `POST /edges`, `DELETE /edges/{etype}/{src}/{dst}`, `POST /backup`, `GET /watch`. Seventeen routes. For those, call the route with `fetch`; the request and response shapes are on the HTTP page. Derive the list by subtracting step 3's routes from the 29 on the HTTP page, and count it.
5. `## Errors` — what the client throws (`MushroomError`, with the status and the server's message), from the README's section.
6. `## Subscriptions` — the README's WebSocket example, and the sentence that a subscription sees only writes made through the server it is connected to.
7. One closing line linking the package's own [README](https://github.com/MatthewSherlin/mushroomdb/blob/main/clients/typescript/README.md) for every option of every method.

Run: `cd clients/typescript && npx vitest run tests/docs.test.ts 2>&1 | tail -8`
Expected: 3 tests passed.

- [ ] **Step 3: Bring the package README up to date**

In `clients/typescript/README.md`: replace the section `## Installation (from repo)` with `## Installation`, leading with `npm install mushroomdb-client` and keeping the from-repo form as a second, shorter paragraph for contributors; and add two sections to its API reference, after `client.createRule`, in the style of their neighbours: `` ### `client.renameNode(oldKey, newKey) → Promise<void>` `` — renames a node's key; 404 when the old key does not exist, 409 when the new one is taken — and `` ### `client.upsertEdge(edgeType, src, dst, placeholderLabel) → Promise<{ nodes_created, edge_inserted }>` `` — creates an edge and, with `placeholderLabel`, whichever endpoint is missing. Add a line under the title linking the site page.

- [ ] **Step 4: Wire the page in, prove the tie, run the gates**

In `docs/site/api.md`, change the TypeScript row's reference cell to `[TypeScript client](typescript.md) — every method, and what it does not wrap`. In `docs/site/SUMMARY.md`, in `# Reference`, after `- [Rust](rust.md)`, add `- [TypeScript client](typescript.md)`. In `docs/site/index.md`, after the Rust line, add `- [TypeScript client](typescript.md) — the HTTP client: its methods, and the routes it leaves to `fetch``. In `scripts/gen-llms-full.sh`, after `    rust.md`, add `    typescript.md`. In `README.md`, in the `- **Reference:**` line, replace `[TypeScript client](clients/typescript/README.md)` with `[Rust](https://matthewsherlin.github.io/mushroomdb/docs/rust.html) · [TypeScript client](https://matthewsherlin.github.io/mushroomdb/docs/typescript.html)`.

```bash
sed -i.bak 's/`client\.renameNode(/`client.renamedNode(/' docs/site/typescript.md
(cd clients/typescript && npx vitest run tests/docs.test.ts 2>&1 | grep -E "renameNode|renamedNode" | head -4)
mv docs/site/typescript.md.bak docs/site/typescript.md
bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check
bash scripts/build-site.sh && python3 scripts/check-links.py && bash scripts/check-claims.sh
(cd clients/typescript && npx tsc --noEmit -p . 2>&1 | tail -3)
```

Expected: the vitest output names `renameNode` as missing and `renamedNode` as unknown; then all green; `build-site.sh: OK — 40 pages`; the type check prints nothing. The full client suite (`npm test`) needs a built binary and is run by CI's `ts-client` job; run it locally too if `clients/typescript/tests/global-setup.ts` can find `target/debug/mushroomdb`.

- [ ] **Step 5: Commit**

```bash
git add docs/site/typescript.md clients/typescript/tests/docs.test.ts clients/typescript/README.md \
        docs/site/api.md docs/site/SUMMARY.md docs/site/index.md scripts/gen-llms-full.sh llms-full.txt README.md
git commit -m "docs(typescript): the client's methods, the routes it leaves alone, and a test that keeps the list honest"
```

---

## For the tone pull request: what this work found and did not fix

Stale against the code, each in a page this plan does not edit:

- `docs/site/timetravel.md`: `write_batch_cas` shown taking a closure (it takes `Vec<BatchOp>`); `submit_batch_cas(..).await?` under an "async API" heading (the method is synchronous); `let db` followed by a mutating call; "`GraphDb::snapshot()` truncates the WAL to empty", which is the Rust call's behaviour and not the CLI's default.
- `docs/site/subscriptions.md`: `subscribe_all_rules` and `subscribe_writes` shown without `?`; the event list omits the two query-row events.
- `docs/site/algorithms.md`: `write_scores` called on a view that is not `mut`.
- `docs/site/python.md`: `SnapshotOptions { keep_wal: true }` omits `archive_wal`.
- `docs/site/testing.md`: describes IVF-Flat as the approximate path; it is HNSW, and IVF is no longer used at run time.
- `docs/site/rules.md`: one example gives `VectorSimilar` a `dims` field that does not exist; "Blob version 3" is 4; the `RuleDef` listing omits the three `via_*` fields and calls `max_edges` a cap per rule where it is per source.
- `llms.txt`: recall "mean 0.998" has no source in the repository; its `search_hybrid` Rust call does not match the signature.
- `docs/site/service.md`: the store-file table and the backup's copy list omit `commit_times.bin`.
- `crates/core-api/examples/quickstart.rs` and `predicates_ii.rs`: their header comments say `cargo run -p core-api`; the package is `mushroomdb`.
- `docs/site/install.md`: "334 edges — 304 of them derived by seven rule sets". The seven rules' counts sum to 334: every edge in the demo is derived.

Worth raising as product questions, none changed here:

- A Rust user installs `mushroomdb` and imports `core_api`.
- A backup does not copy `commit_times.bin`, so a restored store may not be able to answer a date.
- The hybrid search docstring in the engine says the vector side returns nothing without a `label`; the code passes `None` through as "every label".

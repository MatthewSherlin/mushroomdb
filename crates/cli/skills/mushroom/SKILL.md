---
name: mushroom
description: Live code graph for this repo: what changes together, who owns what, why two things are linked (with the lines that prove it), 360° context on any symbol, durable notes. Trigger on: impact, blast radius, who owns, why related, what imports, what calls, co-change, history of, remember, recall, map.
---

# /mushroom

> **Alpha.** Local only. No data leaves your machine.

A live graph of this repository at `{{DB_PATH}}`: files, symbols, imports, calls, commits, authors, merged PRs and your notes. The tools print their evidence: quote it, never paraphrase, never assert a link no tool printed. Every answer opens with an untrusted-data marker, and means it.

## First minute

**1. If `{{DB_PATH}}` does not exist yet, build it once:**

```
{{BIN}} ingest-git '{{DB_PATH}}' . --prs --ensure-gitignore
```

`CO_CHANGED` / `IMPORTS` / `CALLS` edges are derived by rule; the store joins `.gitignore`.

**2. Take your bearings from the `SessionStart` brief** already in your context — size, central files, or the whole schema — not from a search.

**3. Call `explore <target>`** (depth `context`) on the first file or symbol the task names, and quote what it prints — before any `Grep`, file read or plan. On a memory store `explore` is unlisted; row 4 opens instead.
<!-- cli -->

Or through `Bash`, store first; `query` runs any Cypher, read or write:

```
{{BIN}} explore '{{DB_PATH}}' <target> [--depth context|impact|history|all] [--full]
{{BIN}} query '{{DB_PATH}}' '<cypher>'
```
<!-- /cli -->

## Task rules

The first row that matches the turn is what to call, before you answer.

1. **Anything cross-file — call `explore` before `Grep`.** One `target` (a path, a symbol key `path#name`, or a bare name — an ambiguous one returns candidates) and one `depth`:
   `context` (default) — where it is, its signature, call sites into it, callees, importers, co-change partners, commits, notes; `impact` — that plus the file's blast radius (**call it before you edit**); `history` — that plus the owner and what it changes with; `all` — all three.
2. **The user states a decision or a durable fact** → say the key back (`note:` plus 16 hex): `query` a `CREATE (n:Note {id: "note:…", text: "…"})`.
<!-- mcp -->
   Or `remember` — the `text`, and the existing keys it is `about` — on a store that lists it; a code-graph store does not.
<!-- /mcp -->
<!-- cli -->
   There is no `remember` subcommand.
<!-- /cli -->
3. **Commits have landed, or the brief reports an old sync** → `sync`: commits since the last sync, then files that differ from HEAD. On a code-graph store that is the git `post-commit` hook's job, not a listed tool.
4. **On a memory store** — entities, no repository — rows 1–3 do not apply. The brief prints one worked call per question kind against your own keys — copy them, do not probe Cypher for the schema.
<!-- mcp -->
   Fifteen tools are listed, one call each: **why** → `explain_association a b`; **relationships** → `node_edges a` (type, rule, score); **linked by all of** → `node_edges a all_of: [T, U]`; **as of** → `edges_at a <commit>`; **what if** → `what_if a <field> <value>`; **who may see** → `query` with a `role` from `roles.json`; **how many** → a counting Cypher. Since when → `node_history`, `edge_history`, `was_linked`; around it → `neighborhood`, `node_info`; like it → `find_similar`, `hybrid_search`.
<!-- /mcp -->
<!-- cli -->
   Those have no subcommand — `explain_association`, `node_edges`, `edges_at`, `what_if`, `node_history`, `was_linked`, `neighborhood` — and none is wired here: `why <a> <b>`, `asof --commit N --query`, `query` (no `role`). The rest need `--delivery mcp`.
<!-- /cli -->

`explore` composes `context`, `impact` (bare, it reads the diff) and `owners`; those, `why` (what links two keys, with evidence), `recall`, `map` and the rest stay served — `--all-tools` lists them.

### What runs without you

`SessionStart` put that brief in your context. `UserPromptSubmit` prints a recall digest when the prompt names an identifier and nothing otherwise; on a dirty tree it prints the diff instead: unmodified partners and importers, the owner, stale concepts. `PostToolUse` runs `touch` after an edit; the git `post-commit` hook runs a silenced `sync`.

## Learn

The `learn` pass — `/mushroom learn <path>` — turns prose (docs, ADRs) into `Concept` nodes: ≤ 20 documents a run, ≤ 5 concepts each, one row apiece (`id` `concept:<kebab-case-name>`, `name`, `summary` ≤ 300 chars, `source_files` verified with `query` and sorted, `source_hashes` in that order, `extracted_by`, `extracted_at`), written with `ingest_json`.

The `concept_sources` rule links each concept to its sources with `DESCRIBED_IN`; when a source's hash stops matching the concept is stale and the prompt hook says so. **Re-learn only those.**

## Advanced
<!-- mcp -->

`tools/list` follows the store: one built by `ingest-git` shows three — `explore`, `query` (Cypher, read or write) and `stats` — any other store shows the fifteen of row 4. All are served either way; `{{BIN}} mcp <db> --all-tools` lists the rest with schemas. **Never create a rule silently:** *propose* `create_rule` with its predicate and the edges it would derive, and wait for approval. When `ingest_json` skips a field with `ambiguous target labels`, declare one KeyMatch rule per target label instead. `mask` on `query` (and `find_similar`) is an **allow-list**, as `role` is: only those keys are visible, and writes are rejected while either is set. This server has **no auth** and both are cooperative — never a security boundary; real access control is `serve --role-token`.
<!-- /mcp -->

Never invent graph contents: if a call returns empty say so; if one fails show the error verbatim. `serve` browses the same store (`{{BIN}} serve '{{DB_PATH}}'`), and `doctor` checks the install.

More: [docs](https://github.com/MatthewSherlin/mushroomdb/tree/main/docs/site)

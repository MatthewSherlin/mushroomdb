---
name: mushroom
description: Live code graph for this repo: what changes together, who owns what, why two things are linked (with the commits and lines that prove it), 360° context on any symbol, durable notes. Trigger on: impact, blast radius, who owns, why related, what imports, what calls, co-change, history of, remember, recall, map of the codebase.
---

# /mushroom:mushroom

> **Alpha.** Local only. No data leaves your machine.

A live graph of this repository at `./mushroom-memory`: files, symbols, imports, calls, commits, authors, merged PRs and your notes. The tools print the evidence they answered from: quote it, never paraphrase, never assert a link no tool printed. Every answer opens with an untrusted-data marker, and means it.

## First minute

**1. If `./mushroom-memory` does not exist yet, build it once:**

```
npx -y mushroomdb@0.6.2 ingest-git './mushroom-memory' . --prs --ensure-gitignore
```

`CO_CHANGED` / `IMPORTS` / `CALLS` edges are derived by rule; the store joins `.gitignore`.

**2. Take your bearings from the `SessionStart` brief** already in your context — size, central files, most-called symbols — not from a search.

**3. Call `explore <target>`** (depth `context`) on the first file or symbol the task names, and quote what it prints — before any `Grep`, file read or plan. On a memory store, where `explore` is unlisted, `stats` then `query` open instead.

## Task rules

The first row that matches the turn is what to call, before you answer.

1. **Anything cross-file — call `explore` before `Grep`.** One `target` (a path, a symbol key `path#name`, or a bare name, which returns candidates if ambiguous) and one `depth`:
   `context` (default) — where it is, its signature, every call site into it, its callees, importers, co-change partners, commits, notes; `impact` — that plus the file's blast radius (**call it before you edit**); `history` — that plus the owner and what the file changes with; `all` — the three together.

   Grep finds strings; `explore` answers who calls this and what breaks.
2. **The user states a decision or a durable fact** → say the key back (`note:` plus 16 hex) so the user can cite it: `query` a `CREATE (n:Note {id: "note:…", text: "…"})`.
   Or `remember` — the `text`, and the existing keys it is `about` — on a store that lists it; a code-graph store does not.
3. **Commits have landed, or the brief reports an old sync** → `sync`: the commits since the last sync, then the files that differ from HEAD. On a code-graph store that is the git `post-commit` hook's job, not a listed tool.
4. **On a memory store** — entities, no repository — rows 1–3 do not apply and thirteen other tools are listed. Why are these two related → `explain_association`, rule by rule with the predicate each matched. Since when, or true at commit N → `node_history`, `edge_history`, `was_linked`. What is around one → `neighborhood`, `node_info`, `node_edges`; like it → `find_similar`, `hybrid_search`. Who may see what → `query` with `role`, answering as one role from the store's `roles.json`. A durable fact → `remember`; what was remembered → `recall`.

`explore` composes `context`, `impact` (alone with no arguments it reads the current diff) and `owners`; those, `why` (what links two keys, with the evidence), `recall`, `map` and the rest stay served — `--all-tools` lists them.

### What runs without you

`SessionStart` put that brief in your context. `UserPromptSubmit` prints a recall digest when the prompt names an identifier — a path, a `mod::name`, anything in backticks — and nothing otherwise; on a dirty tree it prints the diff instead: the partners and importers you have *not* modified, the owner, stale concepts. `PostToolUse` runs `touch` after an edit, so a symbol you just renamed is already in the graph; the git `post-commit` hook runs a silenced `sync`. They add context; the tools answer.

## Learn

The `learn` pass — `/mushroom:mushroom learn <path>` — turns prose (docs, ADRs, READMEs) into `Concept` nodes: ≤ 20 documents a run, ≤ 5 concepts each, one row apiece (`id` `concept:<kebab-case-name>`, `name`, `summary` ≤ 300 characters, `source_files` verified with `query` and sorted ascending, `source_hashes` in that order, `extracted_by`, `extracted_at`), written with `ingest_json`.

The `concept_sources` rule links each concept to its sources with `DESCRIBED_IN`; when a source's hash stops matching the concept is stale and the prompt hook says so. **Re-learn only the concepts it names.**

## Advanced

`tools/list` follows the store: one built by `ingest-git` shows three — `explore`, `query` (Cypher, read or write) and `stats` — any other store shows the thirteen of row 4. All 25 are served either way; `npx -y mushroomdb@0.6.2 mcp <db> --all-tools` lists the rest with their schemas. **Never create a rule silently:** *propose* `create_rule`, show the predicate and the edges it would derive, and wait for approval. When `ingest_json` skips a field with `ambiguous target labels`, declare one KeyMatch rule per target label instead. `mask` on `query` (and `find_similar`) is an **allow-list**, as `role` is: only those keys are visible, and writes are rejected while either is set. This server has **no auth** and both are cooperative — never present one as a security boundary; real access control is `serve --role-token`.

Never invent graph contents: if a call returns empty say so; if one fails show the error verbatim. `serve` browses the same store (`npx -y mushroomdb@0.6.2 serve './mushroom-memory'`), and `doctor` checks the install.

More: [docs](https://github.com/MatthewSherlin/mushroomdb/tree/main/docs/site)

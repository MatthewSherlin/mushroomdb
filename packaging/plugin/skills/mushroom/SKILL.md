---
name: mushroom
description: Live graph memory for agents — query and create entities, derive relationships by rule, explain why two things are associated, read the graph at a past commit, ask what a change would do, and answer as one role. Trigger on: memory, remember, recall, relationship, entity, association, why are X and Y related, as of, what if, who may see.
---

# /mushroom:mushroom

> **Alpha.** Local only. No data leaves your machine.

A live graph at `./mushroom-memory`: entities, the edges rules derive between them, and your notes. The tools print their evidence: quote it, never paraphrase, never assert a link no tool printed. Every answer opens with an untrusted-data marker, and means it.

## First minute

**1. If the store is empty, say so** — then offer to fill it: `ingest_json` for a batch of rows, `upsert_entity` for one.

**2. Take your bearings from the `SessionStart` brief** already in your context — size, the schema, one worked call per question kind — not from a search.

**3. Answer from the store, and quote what it prints.** One named call per question kind — the table below — against the store's own keys.

## Task rules

One call per question, on the store's own keys. The `SessionStart` brief printed one worked call per kind — copy those rather than probing Cypher for the schema.

| The question | The call |
|---|---|
| why are these two related | `explain_association a b` — the rule, the score and the values the two share |
| what is it related to | `node_edges a` — grouped by type, rule and score; `all_of: [T, U]` for the partners carrying every named type; `label:` narrows them |
| what did it look like then | `edges_at a <commit>` |
| what would this change do | `what_if a <field> <value>` — lost and gained, nothing written |
| who may see | `query` with a `role` from the store's `roles.json` |
| how many | a counting Cypher over the labels the brief listed |
| a durable fact | `remember` — the `text` and the existing keys it is `about`; say the `note:` key back |

Since when → `node_history`, `edge_history`, `was_linked`; around it → `neighborhood`, `node_info`; like it → `find_similar`, `hybrid_search`.

**Deprecated, removed in 0.7.** A store built by `npx -y mushroomdb@0.6.4 ingest-git './mushroom-memory' . --prs --ensure-gitignore` is a repository as entities — commits, pull requests, files, authors — and lists `explore`, `query` and `stats` instead. The code tools `map`, `context`, `impact`, `owners`, `why` and `sync` stay served behind `--all-tools`. Use the repository as a data source; do not reach for it ahead of a search.

### What runs without you

`SessionStart` put that brief in your context; `UserPromptSubmit` prints a `recall` digest when the prompt names an identifier and nothing otherwise; `PostToolUse` runs `touch` after an edit.

## Learn

The `learn` pass — `/mushroom:mushroom learn <path>` — turns prose (docs, ADRs) into `Concept` nodes: ≤ 20 documents a run, ≤ 5 concepts each, one row apiece (`id` `concept:<kebab-case-name>`, `name`, `summary` ≤ 300 chars, `source_files` verified with `query` and sorted, `source_hashes` in that order, `extracted_by`, `extracted_at`), written with `ingest_json`.

The `concept_sources` rule links each concept to its sources with `DESCRIBED_IN`; when a source's hash stops matching the concept is stale and the prompt hook says so. **Re-learn only those.**

## Advanced

`tools/list` follows the store: one built by `ingest-git` shows three — `explore`, `query` (Cypher, read or write) and `stats` — any other store shows the fifteen above. All are served either way; `npx -y mushroomdb@0.6.4 mcp <db> --all-tools` lists the rest with schemas. **Never create a rule silently:** *propose* `create_rule` with its predicate and the edges it would derive, and wait for approval. When `ingest_json` skips a field with `ambiguous target labels`, declare one KeyMatch rule per target label instead. `mask` on `query` (and `find_similar`) is an **allow-list**, as `role` is: only those keys are visible, and writes are rejected while either is set. This server has **no auth** and both are cooperative — never a security boundary; real access control is `serve --role-token`.

Never invent graph contents: if a call returns empty say so; if one fails show the error verbatim. `serve` browses the same store (`npx -y mushroomdb@0.6.4 serve './mushroom-memory'`), and `doctor` checks the install.

More: [docs](https://github.com/MatthewSherlin/mushroomdb/tree/main/docs/site)

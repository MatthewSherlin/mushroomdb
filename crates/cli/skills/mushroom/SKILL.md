---
name: mushroom
description: Live graph memory for agents — query/create entities, derive relationships by rule, explain associations, time-travel, and enforce who-can-see-what. Trigger on: memory, remember, recall, relationship, graph, entity, association, knowledge, store, forget, "why are X and Y related".
---

# /mushroom

> **Alpha.** Local only. No data leaves your machine.

This skill connects Claude Code to a live mushroomdb graph at `{{DB_PATH}}`.

The graph stays true as your data changes: when you SET a property, rules retract stale edges and fire new ones in the same transaction. Access is enforced per query — same graph, different views for different callers. Every write is committed and versioned so you can inspect the graph as it stood at any prior point.

---

## Bootstrap

**First time — `{{DB_PATH}}` does not exist yet.** Pick the source that matches where you are:

- **Inside a git repository (most common):** graph it. Authors, commits and files become nodes; `CO_CHANGED` and `KNOWS` edges are derived by rule and retract when files move or die.

```
'{{BIN}}' ingest-git '{{DB_PATH}}' . --exclude 'node_modules/' --exclude 'target/' --exclude 'vendor/'
```

Re-run the same command any time to sync new commits (it is incremental).

- **No repository:** seed the instant demo graph (10 Orgs, 20 Projects, 30 People, 7 rule sets, 334 edges):

```
'{{BIN}}' demo '{{DB_PATH}}'
```

**`{{DB_PATH}}` already exists:** the MCP server connected automatically when Claude Code started. Skip ahead to querying. If a `UserPromptSubmit` hook is installed, related facts are already in your context under "mushroomdb recall".

---

## Memory-first rules

Follow these in order before answering questions that touch facts, people, projects, or relationships.

**1. Check before you claim.**
Before answering any question about entities, relationships, or stored facts, run `query` (Cypher) or `hybrid_search` (text + vector). Do not improvise from conversation context when the graph has a live record. If the result is empty, say so.

**2. Recall context around a node.**
When a question names a specific entity, run `node_info` for its properties and `neighborhood` for its immediate graph context before answering.

**3. Persist durable facts.**
When the user states a durable fact about a person, project, org, or concept — call `upsert_entity` to persist it. Never silently skip this step.

**4. Explain before asserting relationships.**
When asked "why are X and Y related," always call `explain` (or its alias `explain_association` — both names dispatch to the same implementation) with the two keys and surface the scores. Never describe a relationship without running the tool.

**5. Propose rules — never create silently.**
When a recurring relationship pattern appears, propose `create_rule`. Always confirm with the user first (show the predicate and what edges it would derive). Never create a rule without explicit approval.

**6. Enforce access with mask.**
When acting for a restricted audience, pass `mask` on `query` (and on `find_similar`). The mask is an **allow-list**: only the listed node keys are visible; every other node is omitted from results, and write statements are rejected while a mask is set. Compute the allowed key set for the caller first (for example, every node the caller's role may see), then pass it. `explain`, `neighborhood`, `node_info`, `node_edges` and `hybrid_search` take no mask — do not use them on behalf of a restricted caller.

**7. Answer history questions with history tools.**
For "when did..." / "has X ever been linked to Y?" — use `node_history` and `edge_history` for full audit trails; use `was_linked` for point-in-time edge checks at a specific commit.

**8. Orient with stats first.**
When asked about the overall state of the graph, run `stats` before drilling into specific nodes or edges.

---

## Honesty rules

- **Never invent graph contents.** If `query` returns empty, say so and offer to ingest or upsert.
- **Surface errors verbatim.** If a tool call fails, show the error message — do not guess what the graph contains.
- **This store is local and alpha.** No cloud sync. If durability matters, the user should snapshot: `'{{BIN}}' snapshot '{{DB_PATH}}' <output-file>`.
- **Attribute derived edges.** When showing rule-fired edges, always note which rule produced them. Use `explain` or `explain_association` to get the rule name. Never assert a rule name from memory.
- **This MCP server has no auth.** `mushroomdb mcp` is a local stdio process; masks here are cooperative (the caller supplies them). Real access control is the HTTP server's role tokens (`mushroomdb serve --role-token`). Never present an MCP mask as a security boundary.

---

## Tool reference

All 16 tools. Use these names exactly.

| Tool | Use for | Required args |
|---|---|---|
| `query` | Cypher read or write — the primary tool | `cypher`; optional: `params`, `mask` (allow-list of node keys) |
| `ingest_json` | Bulk-load a JSON array as nodes | `label`, `rows_json`; optional: `key_field` (default `id`), `auto_fk_suffix` (default `_id`), `edges` (array of `{edge_type, src, dst}` user edges). Auto-FK skips a field whose values point at two labels with reason `ambiguous target labels`; for such polymorphic references declare two `create_rule` KeyMatch rules (one per target label) instead. |
| `create_rule` | Define a derived-edge rule | `name`, `src_label`, `dst_label`, `predicate`, `edge_type`; optional: `weight_prop` (default `weight`), `max_edges` (top-k per source) |
| `explain` | Rule-edge and association breakdown between two nodes (alias: `explain_association`) | `a`, `b` |
| `explain_association` | Alias for `explain` — dispatches to the same implementation | `a`, `b` |
| `stats` | Node and edge counts for the whole store | — |
| `neighborhood` | Subgraph radiating from a node | `key`; optional: `depth`, `direction`, `edge_types` |
| `node_info` | Properties of one node | `key` |
| `node_edges` | All edges on a node | `key` |
| `upsert_entity` | Create or update a node | `key`, `props`; optional: `label` |
| `find_similar` | Neighbors by edge or by vector | by edge: `key`, optional `edge_type`, `limit`; by vector: `vector`, optional `field`, `label`, `k`, `min`; optional `mask` (allow-list) in both modes |
| `hybrid_search` | Text + vector fused ranking | `query_text`, `text_field`; optional: `vector`, `vector_field`, `label`, `k`. Pass `label` whenever you pass `vector` — without it the vector leg returns nothing and ranking is text-only. |
| `node_history` | Full property-change log for a node | `key` |
| `edge_history` | Full edge-change log between two nodes | `a`, `b` |
| `was_linked` | Point-in-time edge check at a specific commit | `a`, `b`, `edge_type`, `at_commit` |
| `rename_node` | Rename a node key while preserving all its edges | `old_key`, `new_key` |

---

## 60-second demo

Walk through this after `'{{BIN}}' demo '{{DB_PATH}}'`. Every command is copy-pasteable; the outputs below are from a real run.

### Step 1 — Seed (terminal)

```
'{{BIN}}' demo '{{DB_PATH}}'
```

```
== demo ==
ingested 10 Orgs, 20 Projects, 30 People
overlap rule: skill_fit (Person.skills ∩ Project.skills, min 0.5)
numeric rule: founded_within (Org.founded_year, tolerance 2)
geo rule: nearby_office (Org.office [lat,lon], 50 km)
vector rule: similar_interests (Person.embedding dim 8, min 0.8)

== auto-FK rules ==
  auto_fk_person_org_id
  auto_fk_person_project_id
  auto_fk_project_org_id
```

### Step 2 — Query the current FIT edges for person-01 (`query`)

```cypher
MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p.id, proj.id, r.score
ORDER BY r.score DESC LIMIT 3
```

```
columns: p.id, proj.id, r.score
  p.id=person-01  proj.id=proj-01  r.score=1.0
  p.id=person-01  proj.id=proj-02  r.score=0.5
  p.id=person-01  proj.id=proj-20  r.score=0.5
```

### Step 3 — Change a property; the rule retracts and refires (`query` with SET)

```cypher
MATCH (p:Person {id: 'person-01'})
SET p.skills = ['s05','s06','s07']
RETURN p.id, p.skills
```

```
columns: p.id, p.skills
  p.id=person-01  p.skills=[s05, s06, s07]
```

The rule engine ran immediately: stale `FIT` edges from the old skill set were retracted; new ones were computed by the `skill_fit` rule.

### Step 4 — Same query; different projects fit now (`query`)

```cypher
MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p.id, proj.id, r.score
ORDER BY r.score DESC LIMIT 3
```

```
columns: p.id, proj.id, r.score
  p.id=person-01  proj.id=proj-05  r.score=1.0
  p.id=person-01  proj.id=proj-04  r.score=0.5
  p.id=person-01  proj.id=proj-06  r.score=0.5
```

`proj-01` is gone from the list. The rule retracted that edge because the skill overlap changed.

### Step 5 — Explain the remaining connection to proj-01 (`explain`)

```json
{ "a": "person-01", "b": "proj-01" }
```

```
rule=auto_fk_person_project_id  type=PROJECT  person-01→proj-01  weight=1.0
```

The `FIT` row is absent — the `skill_fit` rule retracted that edge when skills changed. The FK rule (person-01's id is a prefix of proj-01) is structural and stays regardless of skill overlap.

### Step 6 — Check that the old FIT edge existed before the SET (`was_linked`)

```json
{ "a": "person-01", "b": "proj-01", "edge_type": "FIT", "at_commit": 10 }
```

```json
{ "a": "person-01", "b": "proj-01", "edge_type": "FIT", "at_commit": 10, "linked": true }
```

At commit 10 (before the SET), the edge existed. The store keeps the full version history — nothing is lost when an edge is retracted.

---

## 60-second codebase walkthrough

Walk through this after `'{{BIN}}' ingest-git '{{DB_PATH}}' .`. Every command is copy-pasteable; the outputs below are from a real `ingest-git` run against this repository's own history. Commit numbers grow with the repo's history — the `at_commit` values below matched this specific run; read a node's actual sequence back with `node_history` before reusing them.

One substitution: `Author` keys are commit email addresses, and the address below is the RFC 2606 placeholder `maintainer@example.com` rather than the one the run actually used. Take a real key from your own graph with `MATCH (a:Author) RETURN a.id LIMIT 5` before running Steps 3 to 5.

### Step 1 — Find the tightest couplings (`query`)

```cypher
MATCH (f:File)-[r:CO_CHANGED]->(g:File)
RETURN f.id, g.id, r.score
ORDER BY r.score DESC LIMIT 5
```

```
columns: f.id, g.id, r.score
  f.id=.dockerignore  g.id=packaging/npm/.gitignore  r.score=1.0
  f.id=.dockerignore  g.id=packaging/npm/bin/mushroomdb.js  r.score=1.0
  f.id=.github/ISSUE_TEMPLATE/bug.yml  g.id=.github/ISSUE_TEMPLATE/feature.yml  r.score=1.0
  f.id=.github/ISSUE_TEMPLATE/feature.yml  g.id=.github/ISSUE_TEMPLATE/bug.yml  r.score=1.0
  f.id=benchmarks/adapters/__init__.py  g.id=benchmarks/datasets.py  r.score=1.0
```

### Step 2 — Explain one pair (`explain`)

```json
{ "a": "benchmarks/adapters/kuzu.py", "b": "benchmarks/adapters/memgraph.py" }
```

```
rule=co_changed  type=CO_CHANGED  benchmarks/adapters/kuzu.py→benchmarks/adapters/memgraph.py  weight=1.0
rule=co_changed  type=CO_CHANGED  benchmarks/adapters/memgraph.py→benchmarks/adapters/kuzu.py  weight=1.0
```

Both files' commit lists overlap at least 25% (jaccard on `commits`), so `co_changed` links them both ways at weight 1.0 — every commit that touched one touched the other.

### Step 3 — See what an author already knows (`node_edges`)

```json
{ "key": "maintainer@example.com" }
```

Filtered to `KNOWS` edges: none yet — this author identity isn't `TOP_AUTHOR` on any file, so `knows` has nothing to expand from.

### Step 4 — Reassign a file's ownership (`query` with SET)

```cypher
MATCH (f:File {id: 'benchmarks/adapters/kuzu.py'})
SET f.top_author_id = 'maintainer@example.com'
RETURN f.id, f.top_author_id
```

```
columns: f.id, f.top_author_id
  f.id=benchmarks/adapters/kuzu.py  f.top_author_id=maintainer@example.com
```

`TOP_AUTHOR` is a direct auto-FK rule on `top_author_id`, so it retracts and refires in the same transaction:

```cypher
MATCH (f:File {id:'benchmarks/adapters/kuzu.py'})-[:TOP_AUTHOR]->(a:Author) RETURN f.id, a.id
```

```
columns: f.id, a.id
  f.id=benchmarks/adapters/kuzu.py  a.id=maintainer@example.com
```

### Step 5 — `KNOWS` moved with it, in the same write (`was_linked`)

```json
{ "a": "maintainer@example.com", "b": "benchmarks/adapters/kuzu.py", "edge_type": "KNOWS", "at_commit": 14 }
```

```json
{ "a": "maintainer@example.com", "b": "benchmarks/adapters/kuzu.py", "edge_type": "KNOWS", "at_commit": 14, "linked": false }
```

```json
{ "a": "maintainer@example.com", "b": "benchmarks/adapters/kuzu.py", "edge_type": "KNOWS", "at_commit": 15 }
```

```json
{ "a": "maintainer@example.com", "b": "benchmarks/adapters/kuzu.py", "edge_type": "KNOWS", "at_commit": 15, "linked": true }
```

`KNOWS` is a two-hop rule (`Author` →`TOP_AUTHOR`→ `File` →overlap→ `File`), and rules chain: the `TOP_AUTHOR` edge the FK rule wrote in Step 4 immediately fed `knows`, with no second write. Commit 14 is the `SET` itself; every derived-edge change it caused is recorded one marker commit later, which is why 15 is the first sequence that shows the new link. `TOP_AUTHOR` reads the same way — `false` at 14, `true` at 15.

### Step 6 — Ask which hop earned the link (`explain`)

```json
{ "a": "maintainer@example.com", "b": "benchmarks/adapters/kuzu.py" }
```

```json
[{"rule": "auto_fk_file_top_author_id", "edge_type": "TOP_AUTHOR", "weight": 1.0, "via_edge": null},
 {"rule": "knows", "edge_type": "KNOWS", "weight": 1.0, "via_edge": "TOP_AUTHOR"}]
```

Both edges, abridged to the fields that matter here. The `knows` row names `TOP_AUTHOR` as its `via_edge` — the hop it chained off — so the whole path from a one-line `SET` to a new `KNOWS` edge is on the record. `node_edges` on that author now lists `benchmarks/adapters/kuzu.py` among the files they `KNOWS`.

---

For more: `'{{BIN}}' --help` · [docs](https://github.com/MatthewSherlin/mushroomdb/tree/main/docs/site)

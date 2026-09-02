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

**First time — `{{DB_PATH}}` does not exist yet:**

Run this in a terminal to seed an instant graph you can explore:

```
mushroomdb demo {{DB_PATH}}
```

This seeds 10 Orgs, 20 Projects, 30 People, 7 rule sets, and 334 edges. It takes a few seconds.

**`{{DB_PATH}}` already exists:** the MCP server connected automatically when Claude Code started. Skip ahead to querying.

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
When acting for a restricted audience, pass `mask` on `query`. The mask is a list of node keys the caller must not see. The masked nodes are excluded from results.

**7. Answer history questions with history tools.**
For "when did..." / "has X ever been linked to Y?" — use `node_history` and `edge_history` for full audit trails; use `was_linked` for point-in-time edge checks at a specific commit.

**8. Orient with stats first.**
When asked about the overall state of the graph, run `stats` before drilling into specific nodes or edges.

---

## Honesty rules

- **Never invent graph contents.** If `query` returns empty, say so and offer to ingest or upsert.
- **Surface errors verbatim.** If a tool call fails, show the error message — do not guess what the graph contains.
- **This store is local and alpha.** No cloud sync. If durability matters, the user should snapshot: `mushroomdb snapshot {{DB_PATH}} <output-file>`.
- **Attribute derived edges.** When showing rule-fired edges, always note which rule produced them. Use `explain` or `explain_association` to get the rule name. Never assert a rule name from memory.

---

## Tool reference

All 16 tools. Use these names exactly.

| Tool | Use for | Required args |
|---|---|---|
| `query` | Cypher read or write — the primary tool | `cypher`; optional: `params`, `mask` (array of node keys) |
| `ingest_json` | Bulk-load a JSON array as nodes | `label`, `rows_json`; optional: `key_field`, `auto_fk_suffix` |
| `create_rule` | Define a derived-edge rule | `name`, `src_label`, `dst_label`, `predicate`, `edge_type`; optional: `weight_prop` |
| `explain` | Rule-edge and association breakdown between two nodes (alias: `explain_association`) | `a`, `b` |
| `explain_association` | Alias for `explain` — dispatches to the same implementation | `a`, `b` |
| `stats` | Node and edge counts for the whole store | — |
| `neighborhood` | Subgraph radiating from a node | `key`; optional: `depth`, `direction`, `edge_types` |
| `node_info` | Properties of one node | `key` |
| `node_edges` | All edges on a node | `key` |
| `upsert_entity` | Create or update a node | `key`, `props`; optional: `label` |
| `find_similar` | Nearest-neighbor by edge type or by vector | by edge: `key`, `edge_type?`; by vector: `vector`, `field?`, `label?`, `k?`, `min?` |
| `hybrid_search` | RRF over fulltext + vector results | `query_text`, `text_field`; optional: `vector` (bring-your-own embedding), `vector_field`, `label`, `k` |
| `node_history` | Full property-change log for a node | `key` |
| `edge_history` | Full edge-change log between two nodes | `a`, `b` |
| `was_linked` | Point-in-time edge check at a specific commit | `a`, `b`, `edge_type`, `at_commit` |
| `rename_node` | Rename a node key while preserving all its edges | `old_key`, `new_key` |

---

## 60-second demo

Walk through this after `mushroomdb demo {{DB_PATH}}`. Every command is copy-pasteable; the outputs below are from a real run.

### Step 1 — Seed (terminal)

```
mushroomdb demo {{DB_PATH}}
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
rule=auto_fk_person_project_id  type=PROJECT  person-01→proj-01  weight=none
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

For more: `mushroomdb --help` · [docs](https://mushroomdb.dev/docs)

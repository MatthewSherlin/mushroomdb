# Agent memory quickstart

mushroomdb ships a stdio MCP server that exposes the full graph API to any
MCP-compatible agent host — Claude Desktop, Continue, Cursor, or a custom
harness. This guide walks through the canonical agent-memory workflow:
store entities, declare association rules, recall similar entities by query,
and explain why two entities are linked.

---

## Claude Desktop configuration

Add mushroomdb as an MCP server in `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "mushroomdb": {
      "command": "mushroomdb",
      "args": ["mcp", "/path/to/your/db"]
    }
  }
}
```

Replace `/path/to/your/db` with the directory where mushroomdb should store
data. The directory is created on first launch. Restart Claude Desktop after
saving.

If you have not installed the binary yet:

```sh
cargo install mushroomdb-cli
```

Or build from source:

```sh
cargo build -p mushroomdb-cli --bin mushroomdb --release
cp target/release/mushroomdb ~/.local/bin/
```

---

## Full memory workflow

The four steps below demonstrate the complete cycle from storing new
information to explaining how two pieces of knowledge are connected.

### 1. Store entities

Use `upsert_entity` to record facts. It creates the node if it does not exist,
or updates its properties if it does — no existence check required.

```json
// tool: upsert_entity
{ "key": "alice", "label": "Person", "props": { "name": "Alice", "role": "engineer", "emb": [0.9, 0.2, 0.4] } }
{ "key": "bob",   "label": "Person", "props": { "name": "Bob",   "role": "engineer", "emb": [0.8, 0.3, 0.5] } }
{ "key": "carol", "label": "Person", "props": { "name": "Carol", "role": "designer", "emb": [0.1, 0.9, 0.2] } }
```

Or ingest a batch via `ingest_json` when you have multiple records of the same
label:

```json
// tool: ingest_json
{
  "label": "Person",
  "rows_json": "[{\"id\":\"dave\",\"name\":\"Dave\",\"role\":\"engineer\",\"emb\":[0.85,0.25,0.45]}]",
  "key_field": "id"
}
```

### 2. Declare association rules

Rules derive edges automatically. Declare them once; every subsequent
`upsert_entity` or `ingest_json` evaluates them incrementally.

**Semantic similarity** (cosine on embedding field):

```json
// tool: create_rule
{
  "name": "similar_people",
  "src_label": "Person",
  "dst_label": "Person",
  "predicate": { "VectorSimilar": { "field": "emb", "min": 0.85 } },
  "edge_type": "SIMILAR",
  "weight_prop": "score"
}
```

**Shared role** (field equality):

```json
// tool: create_rule
{
  "name": "same_role",
  "src_label": "Person",
  "dst_label": "Person",
  "predicate": { "FieldEqual": { "field": "role" } },
  "edge_type": "SAME_ROLE"
}
```

After `create_rule` returns, derived edges already exist for all matching
pairs in the graph. New entities added later are matched automatically.

### 3. Recall via query

**Find similar people** using the derived edges:

```json
// tool: find_similar
{ "key": "alice", "edge_type": "SIMILAR", "limit": 5 }
```

**Precondition:** `find_similar` reads edges that were previously derived by a
rule. Without a matching rule (e.g. a `VectorSimilar` rule with
`edge_type: "SIMILAR"`), the result is empty — no live cosine computation is
performed. The `create_rule` call in step 2 must come before any `find_similar`
call on the same edge type.

Returns up to 5 neighbors connected to `alice` via `SIMILAR` edges, with
direction and whether the edge is rule-derived.

**Cypher query** for richer filtering:

```json
// tool: query
{ "cypher": "MATCH (p:Person)-[:SIMILAR]->(q:Person) WHERE p.id = 'alice' RETURN q.name, q.role ORDER BY q.name" }
```

**Neighborhood traversal** (multi-hop):

```json
// tool: neighborhood
{ "key": "alice", "depth": 2, "edge_types": ["SIMILAR", "SAME_ROLE"], "direction": "both" }
```

### 4. Explain associations

`explain_association` (or `explain`) shows which rules fired and what scores
produced the connection:

```json
// tool: explain_association
{ "a": "alice", "b": "bob" }
```

Example response:

```json
[
  {
    "rule": "similar_people",
    "edge_type": "SIMILAR",
    "src_key": "alice",
    "dst_key": "bob",
    "weight": 0.96,
    "predicate": { "kind": "VectorSimilar", "field": "emb", "min": 0.85 }
  },
  {
    "rule": "same_role",
    "edge_type": "SAME_ROLE",
    "src_key": "alice",
    "dst_key": "bob",
    "weight": null,
    "predicate": { "kind": "FieldEqual", "field": "role" }
  }
]
```

The agent now knows that alice and bob are associated because their embeddings
are 96% similar and they share the role `"engineer"`.

---

## Tool reference

| Tool | Purpose |
|---|---|
| `upsert_entity` | Insert or update a node by key. Creates if absent, updates props if present. |
| `ingest_json` | Batch-ingest an array of nodes of the same label from JSON. |
| `create_rule` | Declare a derivation rule; backfills existing nodes immediately. |
| `find_similar` | Return neighbors connected by a given edge type (default: `SIMILAR`). |
| `explain_association` | Show which rules and scores produced edges between two nodes. |
| `explain` | Alias for `explain_association`. |
| `query` | Run a Cypher read query. |
| `neighborhood` | Multi-hop neighborhood traversal with optional edge-type filter. |
| `node_info` | Return a node's key, label, and all properties. |
| `node_edges` | Return all edges incident on a node. |
| `stats` | Return live node, edge, and rule counts. |

---

## Why this works for agent memory

Graph databases are a natural fit for long-term agent memory:

- **Entities** map to nodes (`Person`, `Document`, `Project`, `Concept`).
- **Associations** are edges derived from data similarity, shared fields, or
  FK relationships — declared once, maintained automatically.
- **Recall** is graph traversal: "what is similar to X?", "what is near Y?",
  "who shares Z's role?".
- **Explainability** is built in: `explain_association` always shows the rule
  and score, not just the edge.
- **Incremental updates** are O(changed node × candidates), not full
  recomputation — memory stays fresh as the agent writes new facts.

See [`docs/site/rules.md`](rules.md) for the full predicate reference and
[`docs/site/query.md`](query.md) for the Cypher subset.

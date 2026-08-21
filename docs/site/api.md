# API reference

mushroomdb exposes the same graph operations through three surfaces:
HTTP (served by `mushroomdb serve`), MCP JSON-RPC (served by `mushroomdb mcp`),
and Python bindings (via PyO3 / maturin).

---

## HTTP API

Start the server:

```text
mushroomdb serve <db-dir> [--addr 127.0.0.1:8080] [--ui <dist-dir>] [--no-ui] [--demo-if-empty]
```

Default bind is `127.0.0.1:0` (ephemeral port). The bound address is printed
after the listener is accepting. Pass `--addr 127.0.0.1:8080` to pin a port.

### Endpoints

| Method | Path | Description |
|---|---|---|
| `POST` | `/query` | Run a Cypher query |
| `GET` | `/stats` | Database statistics |
| `POST` | `/ingest` | Ingest nodes and/or edges |
| `POST` | `/rules` | Declare a linking rule |
| `GET` | `/explain` | Explain edges between two nodes |
| `GET` | `/node/{key}` | Node info and properties |
| `GET` | `/node/{key}/edges` | Incident edges (typed, with derived flag) |
| `GET` | `/node/{key}/neighborhood` | Typed neighborhood expansion |
| `GET` | `/watch` | WebSocket — live mutation events |

---

### POST /query

Request body:

```json
{
  "cypher": "MATCH (p:Person)-[r:FIT]->(o:Org) WHERE r.score >= $min RETURN p, o, r.score AS score ORDER BY score DESC",
  "params": { "min": 0.5 }
}
```

Default response: Arrow IPC stream (`application/vnd.apache.arrow.stream`).

Add `?format=json` for a JSON response:

```json
{
  "columns": ["p", "o", "score"],
  "rows": [
    ["ada", "acme", 1.0],
    ["bob", "acme", 0.6666666666666666]
  ]
}
```

**Cypher subset:** `MATCH`, `WHERE` (with `AND`/`OR`/`NOT`, comparison
operators), `RETURN` with optional `AS`, `ORDER BY`, `SKIP`, `LIMIT`.
Relationship patterns `->`, `<-`, `-` with optional type and variable.

**Write statements** sent to `/query` (server routes to write-lock automatically):
- `CREATE (n:Label {id: 'key', ...})` — insert node(s) and optional edges
- `MATCH … SET n.field = value` — update properties
- `MATCH … DELETE r` — delete a manual edge; error if derived
- `MATCH (n) DETACH DELETE n` — delete node + all incident edges (derived edges retracted via rule engine; top-k backfill fires)
- `MATCH (n) DELETE n` — delete isolated node (error if any edges remain — use DETACH DELETE)
- `MERGE (n:Label {id: 'key'})` — match-or-create

**Single aggregate functions** in `RETURN`:

```json
{"cypher": "MATCH (p:Person) RETURN COUNT(*)"}
```
```json
{"columns": ["COUNT(*)"], "rows": [[42]]}
```

Supported: `COUNT(*)`, `COUNT(var)` (non-null bindings), `SUM(n.prop)`,
`AVG(n.prop)`, `MIN(n.prop)`, `MAX(n.prop)`. Null/non-numeric property
values are silently skipped for SUM/AVG/MIN/MAX. Grouped aggregation
(`RETURN a, COUNT(*)`) returns a `plan:` error — use the traversal API
or filter to a single aggregate.

`LIMIT`, `SKIP`, and `ORDER BY` are no-ops on aggregate queries in v1;
the single result row is always returned regardless.

**Multi-hop LIMIT:** `MATCH (a)-[:T]->(b)-[:T]->(c) RETURN a, b, c LIMIT 100`
runs with O(LIMIT) memory via the pull-based executor. Dense patterns
without `LIMIT` still error at 1,000,000 intermediate rows.

---

### GET /stats

```json
{
  "nodes_live": 60,
  "nodes_tombstoned": 0,
  "edges": 334,
  "rules": [
    {"name": "skill_fit", "edge_count": 90, "tripped": false},
    ...
  ]
}
```

---

### POST /ingest

```json
{
  "label": "Person",
  "rows": [
    {"id": "alice", "skills": ["graph", "rust"], "embedding": [0.1, 0.2, ...]},
    {"id": "bob", "skills": ["sales"]}
  ],
  "options": {},
  "edges": [
    {"edge_type": "KNOWS", "src": "alice", "dst": "bob"}
  ]
}
```

`edges` is optional. Unknown node keys in `edges` return 400
`"node key not found"`.

Auto-FK inference: fields ending in `_id` whose values match existing node
keys automatically create `KeyMatch` rules on the first ingest call that
sees them.

---

### POST /rules

Body is a `RuleDef` object:

```json
{
  "name": "skill_fit",
  "src_label": "Person",
  "dst_label": "Org",
  "predicate": {"Overlap": {"field": "skills", "min": 0.5}},
  "edge_type": "FIT",
  "weight_prop": "score",
  "max_edges": null,
  "approximate": false
}
```

Returns 400 with `{"error": "..."}` on validation failure (unknown field
type, missing required field, duplicate rule name).

Predicate JSON shapes:

```json
{"KeyMatch": {"field": "org_id"}}
{"FieldEqual": {"field": "industry"}}
{"Overlap": {"field": "skills", "min": 0.5}}
{"NumericWithin": {"field": "founded_year", "tolerance": 2.0}}
{"GeoRadius": {"field": "office", "radius_km": 50.0}}
{"VectorSimilar": {"field": "embedding", "min": 0.8, "dims": 8}}
{"All": [{"FieldEqual": {"field": "region"}}, {"Overlap": {"field": "tags", "min": 0.3}}]}
```

---

### GET /explain?a=&b=

```text
GET /explain?a=person-01&b=proj-01
```

Response: array of explanation objects:

```json
[
  {
    "rule": "auto_fk_person_project_id",
    "edge_type": "PROJECT",
    "src": "person-01",
    "dst": "proj-01",
    "weight": null,
    "approximate": false
  },
  {
    "rule": "skill_fit",
    "edge_type": "FIT",
    "src": "person-01",
    "dst": "proj-01",
    "weight": 1.0,
    "approximate": false
  }
]
```

---

### GET /node/{key}

```json
{
  "key": "person-01",
  "label": "Person",
  "props": {
    "skills": ["graph", "rust", "search"],
    "embedding": [0.1, 0.2, ...]
  }
}
```

Returns 404 with `{"error": "key not found: person-01"}` for unknown keys.

---

### GET /node/{key}/edges

```json
{
  "edges": [
    {"edge_type": "FIT", "src_key": "person-01", "dst_key": "proj-01", "derived": true},
    {"edge_type": "KNOWS", "src_key": "person-01", "dst_key": "person-02", "derived": false}
  ]
}
```

Sorted by `(edge_type, src_key, dst_key)`. `derived: true` means the edge
was created by a rule; `derived: false` means it was written directly via
ingest or the edges field of `/ingest`.

---

### GET /node/{key}/neighborhood?depth=&dir=

`depth` defaults to 1. `dir` is `out`, `in`, or `both` (default `both`).

---

### GET /watch (WebSocket)

Connect with any WebSocket client. After each committed write, the server
sends one JSON text frame per `MutationEvent`:

```json
{
  "type": "NodeInserted",
  "key": "alice",
  "label": "Person"
}
```

---

## MCP (Model Context Protocol)

```text
mushroomdb mcp <db-dir>
```

Runs a newline-delimited JSON-RPC 2.0 loop on stdio. No `Content-Length`
framing.

Handshake:

```text
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
```

Response:

```json
{
  "id": 1,
  "jsonrpc": "2.0",
  "result": {
    "capabilities": {"tools": {}},
    "protocolVersion": "2024-11-05",
    "serverInfo": {"name": "mushroomdb"}
  }
}
```

### Tools

| Tool | Description |
|---|---|
| `query` | Run a Cypher query; params: `cypher`, `params?` |
| `ingest_json` | Ingest nodes; params: `label`, `rows`, `edges?` |
| `create_rule` | Declare a linking rule; params: `RuleDef` fields |
| `explain` | Explain edges; params: `a`, `b` |
| `stats` | Database statistics (no params) |
| `neighborhood` | Typed neighborhood; params: `key`, `depth?`, `dir?` |
| `node_info` | Node info and props; params: `key` |
| `node_edges` | Incident edges; params: `key` |

---

## Python bindings

Install (after the first `v*` tag: `pip install mushroomdb`; before that,
build from source):

```text
cd bindings/python
python -m venv .venv
.venv/bin/pip install -U pip maturin
.venv/bin/maturin develop
```

### Open a database

```python
import mushroomdb

db = mushroomdb.GraphDb("/path/to/db")
```

### Insert nodes

```python
db.insert_node("Person", {"id": "alice", "skills": ["graph", "rust"]})
db.insert_node("Org", {"id": "acme", "skills": ["graph", "rust", "search"]})
```

### Batch ingest

```python
rows = [{"id": f"node-{i}", "value": i} for i in range(10000)]
db.ingest_batch("Person", rows)
```

### Create a rule

```python
db.create_rule({
    "name": "skill_fit",
    "src_label": "Person",
    "dst_label": "Org",
    "predicate": {"Overlap": {"field": "skills", "min": 0.5}},
    "edge_type": "FIT",
    "weight_prop": "score",
    "max_edges": None,
    "approximate": False,
})
```

### Query

```python
result = db.query(
    "MATCH (p:Person)-[r:FIT]->(o:Org) RETURN p, o, r.score AS score",
    {}
)
# result is a list of dicts
for row in result:
    print(row)
```

### Traversal

```python
edges = db.node_edges("alice")   # list of EdgeInfo dicts
info  = db.node_info("alice")    # {key, label, props}
neighbors = db.neighbors("alice", depth=1, direction="out")
```

### Explain

```python
explanation = db.explain("alice", "acme")
# list of dicts: rule, edge_type, src, dst, weight
```

### Stats

```python
s = db.stats()
# {"nodes_live": N, "nodes_tombstoned": 0, "edges": E, "rules": [...]}
```

### Snapshot

```python
db.snapshot()  # flush WAL and write a snapshot file
```

### Exposed surface

The bindings expose: `insert_node`, `ingest_batch`, `create_rule`,
`set_prop`, `query`, `explain`, `neighbors`, `node_edges`, `node_info`,
`stats`, `snapshot`.

Not yet exposed: `ingest_json` (use `ingest_batch`), auto-FK inference
(declare `KeyMatch` rules explicitly), `rebuild`.

# API reference

mushroomdb exposes the same graph operations through four surfaces:
HTTP (served by `mushroomdb serve`), MCP JSON-RPC (served by `mushroomdb mcp`),
Python bindings (via PyO3 / maturin), and the
**[TypeScript client](../../clients/typescript/README.md)** (`mushroomdb-client` package).

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
db.snapshot()  # write V6 (zstd-compressed) snapshot and truncate WAL
```

`snapshot()` writes the current state as a V6 (zstd-compressed) snapshot and
then truncates the WAL to a minimal baseline. Faster cold starts, but as-of
history (`open_at`) restarts from that point — commits before the snapshot are
no longer reachable via time travel.

**Keep WAL (Rust API):**

```rust
use core_api::{SnapshotOptions};
db.snapshot_with(SnapshotOptions { keep_wal: true })?;
```

`keep_wal: true` writes the V6 snapshot but leaves the WAL intact. `open_at`
can still reach pre-snapshot commits. The WAL replay over the snapshot is
idempotent — no manual recovery is needed. The WAL grows until an explicit
`snapshot()` (with default `keep_wal: false`) truncates it.

**V6 snapshot format:** magic + version header uncompressed (6 bytes); the
rest is zstd-compressed (level 3). Measured at 5k nodes: 62 KiB on disk,
16 ms to write, 2 ms to open. V5 snapshots (from v0.1.0) are read
transparently — no migration required. 100k-node numbers: 1.1 GiB on disk
(−50% vs V5), 22.563 s write, 8.880 s open — see
[`benchmarks/results/regression-v0.1.1-20260824.md`](../../benchmarks/results/regression-v0.1.1-20260824.md).

### Atomic write batches (Rust API)

`write_batch` is the closure-style entry point for atomic multi-op commits.
All ops queued inside the closure are validated in order and committed as a
single `WalRecord::Batch` frame (one fsync). Rules fire per op, in order.

```rust
let (nodes, edges) = db.write_batch(|b| {
    b.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))]);
    b.insert_node("Person", "bob", vec![]);
    b.insert_edge("KNOWS", "alice", "bob");
    b.set_prop("alice", "role", Value::Str("admin".into()));
    b.delete_node("old_key");
})?;
// nodes == 2, edges == 1; one fsync, crash-atomic
```

**Error semantics — validate-then-apply:** the closure queues ops without
touching the database. `commit` validates every op before writing anything.
If op N fails validation (duplicate key, unknown key, rule-owned edge) the
entire batch is rejected: no WAL bytes written, no in-memory state changes.

**Crash atomicity, not isolation:** on WAL replay after a crash, a torn
`Batch` frame applies none of its ops. However, while applying a committed
batch in memory, concurrent readers may observe intermediate states. There is
no interactive transaction isolation in v1.

The following ops are supported inside `write_batch`:

| Method | Description |
|---|---|
| `b.insert_node(label, key, props)` | Insert a new node |
| `b.insert_edge(edge_type, src, dst)` | Insert a user-owned edge |
| `b.set_prop(key, field, value)` | Set or overwrite a property |
| `b.remove_prop(key, field)` | Remove a property (no-op if absent) |
| `b.delete_edge(edge_type, src, dst)` | Delete a user-owned edge |
| `b.delete_node(key)` | Delete a node and all its incident edges |
| `b.create_rule(def)` | Register a linking rule |
| `b.delete_rule(name)` | Remove a linking rule |

The method-chaining `db.batch()` builder is equivalent; `write_batch` is a
convenience wrapper that auto-commits.

### Exposed surface

The bindings expose: `insert_node`, `ingest_batch`, `batch_edges`,
`create_rule`, `set_prop`, `query`, `explain`, `neighbors`, `node_edges`,
`node_info`, `stats`, `snapshot`.

`write_batch` is not directly exposed in the Python bindings because Rust
closures capturing `&mut BatchBuilder` do not map naturally to the Python
object model. Use `ingest_batch` (node + edge inserts in one frame) or
`batch_edges` (mixed insert/delete edges in one frame) for atomic Python
writes. Both use the same `WalRecord::Batch` frame and one-fsync guarantee.

Not yet exposed: `ingest_json` (use `ingest_batch`), auto-FK inference
(declare `KeyMatch` rules explicitly), `rebuild`.

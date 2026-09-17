# API reference

mushroomdb exposes the same graph operations through four surfaces:
HTTP (served by `mushroomdb serve`), MCP JSON-RPC (served by `mushroomdb mcp`),
Python bindings (via PyO3 / maturin), and the
**[TypeScript client](../../clients/typescript/README.md)** (`mushroomdb-client` package).

---

## HTTP API

Start the server:

```text
mushroomdb serve <db-dir> [--addr 127.0.0.1:8080] [--token <secret>] \
  [--role-token TOKEN:ROLE]... [--ui <dist-dir>] [--no-ui] [--demo-if-empty]
```

Default bind is `127.0.0.1:8080`. The bound address is printed after the
listener is accepting. Non-loopback `--addr` requires `--token` or
`MUSHROOMDB_TOKEN` (see `SECURITY.md`).

### Authentication

For TLS configuration options, see [deployment.md](deployment.md).

**Full-access token** (`--token` / `MUSHROOMDB_TOKEN`): bearer or `?token=`
query param. Grants access to all endpoints.

**Role-bound tokens** (`--role-token TOKEN:ROLE` / `MUSHROOMDB_ROLE_TOKENS="tok1:role1,tok2:role2"`):
bearer-only. A role token receives a node-visibility mask derived from the
named role's label selectors (defined in `schema.json` or `roles.json`), and
optionally narrowed by one property test on those labels. The mask is resolved
live at request time against the same DB snapshot used for the query — one
read-lock acquisition, memoised per commit so a scoped reader between two writes
resolves its role once.

A role may carry a `visible_where` predicate beside its labels:

```json
{
  "name": "reader",
  "labels": ["Document"],
  "visible_where": { "field": "status", "in": ["published", "archived"] }
}
```

`visible = keys ∪ { n : label(n) ∈ labels ∧ predicate(n) }` — the predicate
narrows the labels leg only, never `keys`, and a node missing the property fails
it. Only `eq` (one value) and `in` (a list) exist, exactly one per predicate, and
a predicate on a role with no labels is refused. Values may be written as plain
JSON scalars, as above, or in the graph's tagged encoding (`{"Str": "published"}`)
— both are accepted, and the tagged form is what the server writes back. See
[masks.md](masks.md#narrowing-a-role-by-a-property).

Role-token behavior per endpoint:

| Endpoint | Role-token response |
|---|---|
| `GET /health` | 200 (unauthenticated — no identity needed) |
| `POST /query` (read) | 200 — rows filtered to visible nodes; client `mask` intersects role mask (never widens) |
| `POST /query` (write: `CREATE`/`SET`/`DELETE`/`MERGE`) | 200 if in write scope + target visible; 403 with reason otherwise |
| `POST /nodes` | 200 if `label` in `create_labels`; 403 with scope reason otherwise |
| `DELETE /node/{key}` | 200 if node visible and `label` in `delete_labels`; 403 otherwise |
| `POST /edges` | 200 if type in `create_edge_types` and both endpoints visible; 403 otherwise |
| `DELETE /edges/{t}/{s}/{d}` | 200 if type in `delete_edge_types` and both endpoints visible; 403 otherwise |
| `POST /edges/upsert` | 200 if type in `create_edge_types` and both endpoints visible; 403 otherwise |
| `PUT /node/{key}/prop/{f}` | 200 if node visible and `label` in `update_labels`; 403 otherwise |
| `DELETE /node/{key}/prop/{f}` | 200 if node visible and `label` in `update_labels`; 403 otherwise |
| `POST /ingest` | 200 if `label` in `create_labels`; 403 otherwise (all-or-nothing) |
| `GET /node/{key}` | 200 if visible; 404 if hidden or absent (indistinguishable) |
| `GET /node/{key}/edges` | 200 if visible; 404 if hidden or absent |
| `GET /node/{key}/neighborhood` | 200 if visible; 404 if hidden or absent |
| `POST /find_similar` | 200 — vector search; client `mask` intersects role mask (never widens) |
| `GET /stats` | 403 (counts leak graph size beyond the role's subgraph) |
| `POST /rules` | 403 (administrative — all role tokens, including write-scoped) |
| `GET /explain` | 403 (rule explanation reveals hidden-node linkage) |
| `GET /suggest` | 403 (scans full graph) |
| `POST /algo/*` | 403 (full-graph algorithms) |
| `GET /subscribe` | 403 (unfiltered mutation stream) |
| `GET /watch` | 403 (unfiltered mutation stream) |
| `POST /nodes/{key}/rename` | 403 (admin-only identity change) |
| `POST /backup` | 403 |

Roles without a `write` scope (or roles defined in a v1 sidecar) receive 403 on all
write endpoints — exactly the v1 read-only behavior.

**Never-widen invariant:** unknown token → 401; token bound to a role name not
present in `roles.json` at open time → 401; corrupt `roles.json` → 500 for
role tokens (full-access token unaffected); empty role → sees zero nodes.
Role sidecar is stored in `<db-dir>/roles.json` (v1 for read-only roles, v2 when
any role carries a write scope, v3 when any role carries a `visible_where`
predicate). A version this binary does not recognise poisons the roles state
rather than loading it — denying beats over-granting.

---

### Write scopes for role-bound tokens

v0.3 lifts the write ban for roles that explicitly declare write scopes in
`schema.json`. Roles without a write scope remain read-only.

#### WriteScope schema

```rust
pub struct WriteScope {
    pub create_labels: Vec<String>,      // CREATE nodes under these labels
    pub update_labels: Vec<String>,      // SET props / MERGE-match on visible nodes
    pub delete_labels: Vec<String>,      // DELETE / DETACH DELETE visible nodes
    pub create_edge_types: Vec<String>,  // INSERT EDGE between two visible endpoints
    pub delete_edge_types: Vec<String>,  // DELETE user-owned edges (not derived)
}
```

All fields default to empty (read-only). Each label field must be a subset of the
role's read `labels` — `apply_schema` rejects disjoint write and read scopes with
an error naming the role and offending label.

Example `schema.json` role definition:

```json
{
  "name": "agent-memory",
  "labels": ["AgentNote", "AgentContext"],
  "write": {
    "create_labels": ["AgentNote", "AgentContext"],
    "update_labels": ["AgentNote", "AgentContext"],
    "delete_labels": ["AgentNote"],
    "create_edge_types": ["RECALLS", "DERIVED_FROM"],
    "delete_edge_types": ["RECALLS"]
  }
}
```

#### Never-widen for writes

No write a role performs may make data visible to itself or any other party that
was not already visible before the write. Concretely:

- A role may only update or delete nodes currently in its read mask.
- MERGE on a hidden key returns 403 "target node not visible" — indistinguishable
  from a non-existent key (no existence oracle).
- Edge creation requires both endpoints to be in the role's read mask.
- When a role's CREATE triggers a derivation rule that links to a hidden neighbor,
  the derived edge is created (rule engine runs with DB authority) but is filtered
  from all role-scoped read responses. The role cannot observe the derived edge.
- `/rules`, `/stats`, `/explain`, `/subscribe`, and `/watch` remain 403 for all role
  tokens regardless of write scope.

#### §4.3 Error body shapes

All write denials return 403 with a structured JSON body:

```json
{"error": "role-bound token: label 'Secret' not in write scope (create_labels)"}
{"error": "role-bound token: target node not visible"}
{"error": "role-bound token: edge endpoint not visible"}
{"error": "role-bound token: edge type 'LINKS' not in write scope (create_edge_types)"}
{"error": "role-bound token: this endpoint is not permitted"}
{"error": "role-bound token: namespace 'tenant-b' not in the role's namespaces"}
{"error": "role-bound token: MERGE create requires the role to name one namespace"}
```

The "target node not visible" response is identical for hidden nodes and
non-existent nodes — the role cannot distinguish the two cases.

The sixth is the namespace leg (v0.6.6): a role bound to `namespaces` may only
**create** a node inside them, because a node it could never read back would be a
write into another tenancy. It covers `POST /nodes`, a batch, Cypher `CREATE`, and a
`MERGE` that names `ns`. A create naming no namespace is a create in `default`, so a
role not bound to `default` is refused there too — except a `MERGE` whose executing
role is bound to exactly one namespace, which stamps that namespace on the create
arm. A role bound to two or more namespaces cannot `MERGE`-create unless the
pattern names one `ns`; the refusal is
`role-bound token: MERGE create requires the role to name one namespace`.
A role with no `namespaces` binding is unchanged. Updates need no rule of their
own: a role can only mutate nodes already in its read mask, which the namespace
leg has already narrowed.

A placeholder endpoint `POST /edges/upsert` would auto-create is refused by the same
rule (a placeholder carries no props, so it lands in `default`) but with the
**endpoint** message, `role-bound token: edge endpoint not visible`, not the namespace
one. That is deliberate: the namespace arm fires only for an endpoint that does *not*
exist and the visibility arm only for one that does, so two different strings would
turn the pair into an existence oracle. Hidden ≡ absent holds here as everywhere else.
See [masks.md](masks.md#namespaces).

#### Threat model

| Capability | v1 (read-only) | v0.3 (with write scope) |
|---|---|---|
| Read nodes outside role mask | No | No |
| Confirm existence of hidden nodes | No | No (hidden ≡ absent in all responses) |
| Create nodes under allowed labels | No | Yes (in `create_labels`) |
| Overwrite properties of visible nodes | No | Yes (if label in `update_labels`) |
| Delete visible nodes | No | Yes (if label in `delete_labels`) |
| Create edges between visible nodes | No | Yes (if type in `create_edge_types`) |
| Trigger rule engine to link hidden neighbors | Indirectly | Yes via CREATE; result edges to hidden nodes are not visible to role |
| Observe rule trip counts | No | No (`/stats` remains 403) |
| Expand write scope at runtime | No | No (`roles.json` requires full-access token) |
| Declare new linking rules | No | No (`/rules` remains 403) |
| Subscribe to mutation streams | No | No (`/subscribe` and `/watch` remain 403) |

**Accepted structural disclosure:** a role holding `create_labels` for some label
can learn whether a given key exists in the global key namespace by attempting a
CREATE — if the key is hidden (exists under a different label) the response is
"target node not visible" rather than the success response for a truly absent key.
This key-existence signal is structural and predates write scopes (a full-token
INSERT does the same via DuplicateKey). The disclosure is confined to roles holding
create scope, reveals existence only (never label, properties, or adjacency), and
is stated plainly in the threat model. Roles that do not hold create scope cannot
drive this oracle.

**MCP trust boundary:** The MCP interface (`mushroomdb mcp`) is a stdio
JSON-RPC server intended for local agent use; it operates without bearer-token
authentication and is not subject to role enforcement.

### Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Liveness + counts `{"ok":true,"nodes":N,"edges":N,"addr":"..."}` (no auth) |
| `POST` | `/query` | Run a Cypher query |
| `GET` | `/stats` | Database statistics |
| `POST` | `/ingest` | Ingest nodes and/or edges |
| `POST` | `/rules` | Declare a linking rule |
| `GET` | `/explain` | Explain edges between two nodes |
| `GET` | `/node/{key}` | Node info and properties |
| `GET` | `/node/{key}/edges` | Incident edges (typed, with derived flag) |
| `GET` | `/node/{key}/neighborhood` | Typed neighborhood expansion |
| `POST` | `/find_similar` | Vector similarity search (role tokens allowed; not `/algo/*`) |
| `POST` | `/nodes/{key}/rename` | Rename a node's key (full-token only) |
| `POST` | `/edges/upsert` | Insert an edge, auto-creating missing endpoints (full-token only) |
| `POST` | `/backup` | Consistent backup of the database to an admin-supplied path (full-token only) |
| `GET` | `/watch` | WebSocket — live mutation events |
| `GET` | `/subscribe` | WebSocket — rule and write events |

---

### POST /query

Request body:

```json
{
  "cypher": "MATCH (p:Person)-[r:FIT]->(o:Org) WHERE r.score >= $min RETURN p, o, r.score AS score ORDER BY score DESC",
  "params": { "min": 0.5 },
  "stub_hidden": false
}
```

`stub_hidden` (optional, default `false`): when `true`, hidden nodes in the mask the
request resolved to appear as `{"key": "…", "restricted": true}` rather than being
omitted. That mask is whichever one applies — a client `mask`, a `namespace` alone, or
the two intersected — so a `namespace`-only request honours `stub_hidden` exactly as a
key allow-list does. See [masks.md](masks.md) for the full policy. Note: Cypher result
rows are **always omit-only** — `stub_hidden` has no effect on which rows the query
returns; it only affects the node-info, edges, and neighborhood endpoints.

Role tokens: `stub_hidden` is silently ignored — except with `as_of`, where it is
refused (`as_of (time-travel) does not compose with stub_hidden`). Hidden nodes are
always fully omitted for role-token requests.

`as_of` (optional): a 0-based WAL commit index. The query is answered from the
graph as it existed at that commit, read-only — a write statement with `as_of`
is a 400. It composes with a role token and with a client `mask`, both of which
are resolved against the graph as it was then; a role token plus a client mask
still intersects, never widens. The role *definition* itself is the current one
(`roles.json` is a sidecar and is never a WAL record). A commit outside the
retained range is a 400 naming the range, e.g.
`commit 9999 is out of range; valid range is 12..40 — events before commit 12 are not retained`.
`as_of` and `stub_hidden` do not compose: the pair is a 400. See
[timetravel.md](timetravel.md) and [masks.md](masks.md).

```json
{
  "cypher": "MATCH (n) RETURN n",
  "as_of": 12,
  "mask": ["ada", "bob"]
}
```

**Deletion is not retroactive, and keys are not identities.** A role reads a
now-deleted node at a commit where it was live, and a role's `keys` resolve to
whichever node held that key at the commit asked for (renaming frees a key for
reuse). To revoke history, prune archives or narrow the role. See
[masks.md](masks.md).

`namespace` (optional, v0.6.6): answer from one namespace only.

```json
{
  "cypher": "MATCH (n) RETURN n",
  "namespace": "tenant-a"
}
```

It **intersects** every other restriction — a role token's own `namespaces` binding,
a client `mask`, and `as_of` — so it can only narrow, never widen: a token bound to
`tenant-a` asking for `tenant-b` gets an empty result, not the union. A role bound to
namespaces needs no `namespace` in the body at all; the resolver applies its binding
to `POST /query` and to `GET /node/{key}`, `/edges`, `/neighborhood` and the history
routes, where a node in another namespace answers exactly as an absent key does.
Omitting `namespace` is no namespace restriction (not `default`; `"default"` is how
you name the nodes that name no namespace). A name no node uses is an empty mask; an
invalid name is a 400 naming the rule. Like `mask`, passing it makes the request a
read. See [masks.md](masks.md#namespaces-on-every-surface).

**Cost.** An `as_of` read — role-token reads included — replays the WAL under
the store's read guard for the duration of the query; it does not use the
lock-free epoch snapshot that a plain read uses. Replay cost grows with the
number of commits since the last truncating snapshot, so on a large store take
snapshots (see the horizon and retention sections in
[timetravel.md](timetravel.md)) and rate-limit role tokens that are allowed to
send `as_of`.

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
- `MERGE (n:Label {id: 'key'}) [ON CREATE SET …] [ON MATCH SET …] [RETURN …]` — match-or-create with optional per-clause SET and projection. A single-namespace role's create arm stamps that namespace when the pattern does not name `ns`; a two-namespace role must name one (`MERGE (n:Label {id: 'key', ns: 'tenant-a'})`)

**Aggregate functions** in `RETURN`:

```json
{"cypher": "MATCH (p:Person) RETURN COUNT(*)"}
```
```json
{"columns": ["COUNT(*)"], "rows": [[42]]}
```

Supported: `COUNT(*)`, `COUNT(var)` (non-null bindings), `SUM(n.prop)`,
`AVG(n.prop)`, `MIN(n.prop)`, `MAX(n.prop)`. Null/non-numeric property
values are silently skipped for SUM/AVG/MIN/MAX.

**Grouped aggregation** — one or more non-aggregate items act as group keys:

```json
{"cypher": "MATCH (p:Person) RETURN p.city, COUNT(*) AS cnt ORDER BY cnt DESC LIMIT 5"}
```

Multiple group keys and multiple aggregates per query are allowed. Group count
is capped at 1,000,000 distinct keys. `ORDER BY` + `LIMIT` sort the finished
group table (top-k groups). `OPTIONAL MATCH` composes with grouped aggregation:
edgeless-anchor rows produce `COUNT = 0` rather than being dropped.

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
  "history_floor": 0,
  "namespaces": [
    {"name": "default", "nodes_live": 58},
    {"name": "tenant-a", "nodes_live": 2}
  ],
  "rules": [
    {"name": "skill_fit", "edges": 90, "tripped": false, "fires": 90, "approximate": false},
    ...
  ]
}
```

`namespaces` (v0.6.6) lists every namespace with at least one live node, in name
order, always including `default` — so a store that has never named a namespace
reports exactly one entry. The route requires a full-access token, as it always did,
which is also what keeps the roster from being a cross-tenant disclosure: a role token
gets 403 here. MCP `stats` takes `role` / `namespace` to narrow the roster instead.

A rule whose vector index is still being built carries an extra `building`
object and derives no edges until it disappears:

```json
{"name": "sim", "edges": 0, "tripped": false, "fires": 0, "approximate": true,
 "building": {"rule": "sim", "indexed": 2048, "total": 120000}}
```

The field is absent for every rule that is not building. See
[POST /rules](#post-rules) for what starts a build and
`mushroomdb build-index` for finishing one on a quiet store.

---

### GET /metrics

Returns runtime counters and the slow-query log. Requires a full-access token
(role-bound tokens receive 403, same as `/stats`).

```json
{
  "nodes_live": 60,
  "nodes_tombstoned": 0,
  "edges": 334,
  "commit_seq": 42,
  "wal_size_bytes": 8192,
  "rss_bytes": 20971520,
  "uptime_s": 3600,
  "slow_queries": {
    "threshold_ms": 100,
    "count": 3,
    "last": [
      {"ms": 142, "query": "MATCH (n:Person) RETURN n", "at_commit": 40}
    ]
  }
}
```

**Fields:**

| Field | Type | Notes |
|---|---|---|
| `nodes_live` | integer | Live (non-tombstoned) node count |
| `nodes_tombstoned` | integer | Soft-deleted node count |
| `edges` | integer | Total edge count |
| `commit_seq` | integer | WAL commit sequence number (monotonically increasing) |
| `wal_size_bytes` | integer | On-disk WAL file size in bytes |
| `rss_bytes` | integer or null | Resident set size of the server process; null on unsupported platforms |
| `uptime_s` | integer | Seconds since the HTTP server started |
| `slow_queries.threshold_ms` | integer | Current slow-query threshold (0 = disabled) |
| `slow_queries.count` | integer | Lifetime count of slow queries recorded |
| `slow_queries.last` | array | Up to 16 most recent slow-query entries, oldest first |

**Slow-query threshold:** set via the `MUSHROOMDB_SLOW_QUERY_MS` environment
variable at server start (default 100 ms; 0 disables the log). Any read query
whose execution time meets or exceeds the threshold is logged to `stderr` and
appended to the in-memory ring buffer (capped at 16 entries).

**RSS caveats:** `rss_bytes` is read from `mach_task_basic_info` on macOS and
`/proc/self/statm` on Linux. On other platforms it is always `null`. The value
includes shared library mappings and may not match tools like `top` exactly. It
never causes a panic or blocking I/O.

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

`namespace` (optional, v0.6.6) is the namespace **every node this call creates** lands
in — the reserved, write-once `ns` property, named on the call:

```json
{"label": "Person", "namespace": "tenant-a", "rows": [{"id": "alice"}]}
```

A row carrying its own `ns` must name the same namespace; a disagreement is a 400
naming the row, and nothing is written. Omitted means the `default` namespace, and an
explicit `"default"` stores nothing, so a store that never passes one never grows an
`ns` column. `POST /nodes` takes the same field for the one node it creates. A
namespace is set at insert and cannot be changed afterwards — `PUT` or `DELETE` on
`/node/{key}/prop/ns` is a 400 carrying the `NamespaceImmutable` text. See
[masks.md](masks.md#namespaces).

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
  "approximate": false
}
```

Omitted or JSON-null `max_edges` fills the default after deserialize: **32**
for scored predicates, **512** for KeyMatch (and KeyMatch-rooted `All`) — one
per element a list-valued FK field can name; a scalar FK field still resolves to
at most one destination, so the higher cap is inert there. Rust
`max_edges: None` remains the 1,000,000 global-budget hatch; HTTP cannot
express that hatch (null fills the default). Python `None` and a missing key
behave the same as HTTP null — both fill the predicate default.

**Symmetric predicates:** when `src_label == dst_label`, the rule fires in both
directions (the updated node is evaluated as source AND as destination). This
creates edges in both directions. An undirected Cypher pattern
(`MATCH (a)-[:T]-(b)`) then double-counts rows. Prefer directed patterns
(`-[:T]->`) or add `RETURN DISTINCT a, b`.

`namespace` (optional, v0.6.6) scopes the rule to one namespace: it sees only that
namespace's nodes — source, via hop and destination — so every edge it derives stays
inside. Omitted (or null) means a global rule, which is the only kind that may derive
an edge across a boundary. An invalid name is a 400 and nothing is installed. See
[rules.md](rules.md#namespace-scoping).

Returns 400 with `{"error": "..."}` on validation failure (unknown field
type, missing required field, duplicate rule name).

**202 while the index builds.** A VectorSimilar rule — in **either** mode, since
both build the vector index — over more than 2,048 vectors cannot index its
corpus in one commit, so the route installs the rule, indexes the first slice,
and returns `202 Accepted` with the progress instead of `200`:

```json
{"rule": "sim", "building": {"indexed": 2048, "total": 120000}}
```

The rule derives **no** edges until the build finishes — never a partial set —
and `GET /stats` reports the same progress under `building`. Every write
advances the build by one slice, `mushroomdb serve` advances it once a second
(including after a restart: open registers a build a snapshot cut short, so
the ticker does not wait for a write), and `mushroomdb build-index <db-dir>`
drives it to completion. At or below 2,048 vectors nothing changes: one
commit, `200`, edges present on return.

Predicate JSON shapes:

```json
{"KeyMatch": {"field": "org_id"}}
{"FieldEqual": {"field": "industry"}}
{"Overlap": {"field": "skills", "min": 0.5}}
{"NumericWithin": {"field": "founded_year", "tolerance": 2.0}}
{"GeoRadius": {"field": "office", "km": 50.0}}
{"VectorSimilar": {"field": "embedding", "min": 0.8}}
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
    "weight": 1.0,
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

Query params: `mask=key1,key2,…` (optional), `stub_hidden=true` (optional).

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

Returns 404 with `{"error": "node key not found: person-01"}` for unknown keys.

With `stub_hidden=true` and a client mask, a key that exists but is outside the mask
returns `{"key": "person-01", "restricted": true}` rather than 404. A key that does
not exist at all still returns 404. Role tokens: `stub_hidden` is ignored; hidden keys
return 404 as if absent.

---

### GET /node/{key}/edges

Query params: `mask=key1,key2,…` (optional), `stub_hidden=true` (optional).

```json
{
  "edges": [
    {"edge_type": "FIT", "src_key": "person-01", "dst_key": "proj-01", "derived": true},
    {"edge_type": "KNOWS", "src_key": "person-01", "dst_key": "person-02", "derived": false}
  ]
}
```

Sorted by `(edge_type, src_key, dst_key)`. `derived: true` means the edge was created
by a rule; `derived: false` means it was written directly.

With `stub_hidden=true` and a client mask, edges to restricted endpoints are included
in the list; the restricted endpoint is rendered as `{"key": "…", "restricted": true}`.
The `edge_type` and `derived` fields are always present in the edge object. Without
`stub_hidden`, edges to hidden endpoints are omitted entirely.

---

### GET /node/{key}/neighborhood?depth=&dir=

`depth` defaults to 1. `dir` is `out`, `in`, or `both` (default `both`).

Query params: `mask=key1,key2,…` (optional), `stub_hidden=true` (optional).

With `stub_hidden=true` and a client mask, hidden **direct** neighbors of visited nodes
appear as stub rows (`label: null`). The BFS frontier is not expanded through hidden
nodes — stub rows are terminal.

Returns 404 with `{"error": "node key not found: …"}` for unknown keys, including under
a client mask — the same body as `GET /node/{key}`.

---

### POST /find_similar

Vector kNN. **Not** under `/algo/*`: role tokens are allowed, and a client `mask`
intersects the role mask (never widens). Unknown keys in `mask` are skipped.

Request body:

```json
{
  "field": "embedding",
  "vector": [0.1, 0.2, 0.3],
  "k": 10,
  "min": 0.0,
  "label": "Person",
  "mask": ["alice", "bob"],
  "where": {"field": "status", "eq": "published"},
  "exact": true
}
```

`field` and `vector` are required. `k` defaults to 10. HTTP `min` defaults to **0.0**
(the Python default). MCP vector-mode `find_similar` still defaults `min` to **0.8**.
`where` is a `{field, eq}` / `{field, in}` predicate and implies exact GEMM; invalid
`where` is 400. `exact: true` skips HNSW.

Response: `{"hits": [[key, score], …]}` — the same `(key, similarity)` pairs Python
`find_similar` returns. Scores are cosine similarity in `[-1, 1]`; `score >= min`.

---

### POST /nodes/{key}/rename

Rename a node's key. `{key}` is the current (old) key. Full-token only; role tokens
return 403.

Request body:

```json
{"new_key": "alice2"}
```

Responses:

- `200 OK` — `{"ok": true}` — rename succeeded.
- `404 Not Found` — old key does not exist.
- `409 Conflict` — new key already exists.

The rename is WAL-logged as a single `RenameNode` record. All edges referencing the
node continue to work under the new key. Node history and time-travel (`open_at`)
correctly scope events to the identity that held the key at each commit — recycling
a key for a different node does not contaminate the previous identity's history.

---

### POST /edges/upsert

Insert an edge, auto-creating any missing endpoint nodes. Full-token only; role tokens
return 403.

Request body:

```json
{
  "edge_type": "KNOWS",
  "src_key": "alice",
  "dst_key": "bob",
  "placeholder_label": "Person"
}
```

`placeholder_label` is the label used when a missing endpoint node is created. If both
endpoints already exist the body still requires this field.

Response:

```json
{"nodes_created": 1, "edge_inserted": true}
```

`nodes_created` is 0, 1, or 2. `edge_inserted` is `false` when the edge already
existed (idempotent on re-submission). Linking rules fire on any newly created
placeholder nodes within the same batch frame.

---

### POST /backup

Take a consistent backup of the database. Full-token only; role tokens return 403.

Request body:

```json
{"dest": "/absolute/path/to/backup-dir"}
```

`dest` is an arbitrary admin-supplied path; the server must have write permission to
it. The endpoint accepts any absolute path without validation — restrict access to
this endpoint accordingly.

The server holds the read lock for the **full duration** of the operation: file copies
plus post-copy CRC verification. Writers are blocked for that entire window. For
stores with large snapshots this may be tens of seconds.

The server copies: `snapshot.bin`, `wal.bin`, all `wal.<N>.archive` files,
`wal.floor`, `wal.genesis`, and `roles.json`. After the copies complete it opens the
backup read-only and runs the CRC verifier.

Responses:

- `200 OK` — backup completed and verified. Body:

  ```json
  {
    "files": ["snapshot.bin", "wal.bin", "wal.genesis"],
    "bytes": 1887436800,
    "verified": true
  }
  ```

- `500 Internal Server Error` — backup succeeded but verification failed
  (`verified: false` in body). Treat as a failure; do not use the backup.

**This is the correct path for live-served stores.** Running `mushroomdb backup` CLI
against a directory that a `serve` process is writing to is unsafe — the CLI copies
are not atomic across processes. Use `POST /backup` when the server is running.

See [format-stability.md](../format-stability.md) for the PITR (point-in-time
recovery) workflow using backup together with WAL archives.

---

## Backup and export (Rust API and CLI)

### Backup (Rust API)

```rust
let report = db.backup_to(Path::new("/backup/dir"))?;
// report.files: Vec<String>  — files copied, sorted
// report.bytes: u64          — total bytes copied
// report.verified: bool      — true when post-copy CRC check passed
```

`backup_to` copies the store files and verifies the copy by opening it read-only.
The `&self` borrow excludes concurrent writers within the same process, but it
provides **no cross-process guarantee**. Do not call `backup_to` via the Rust API
against a directory that a separate `mushroomdb serve` process is writing to. Use
`POST /backup` instead.

### Backup (CLI)

```sh
mushroomdb backup <db-dir> <backup-dir>
```

WARNING: unsafe against a concurrently running `mushroomdb serve` process. For
live-served stores use `POST /backup`.

### Export (Rust API)

```rust
let nodes: Vec<NodeInfo>     = db.all_nodes_for_export();
let edges: Vec<ExportEdge>   = db.all_edges_for_export();
// ExportEdge { edge_type, src, dst, derived: bool, rule: Option<String>, weight: Option<f64> }
```

Nodes are sorted by key. Edges are sorted by `(edge_type, src, dst)`. Derived
edges include the rule name in `rule`; manually-written edges have `rule: None`.

### Export (CLI)

```sh
mushroomdb export <db-dir> <dest> [--format jsonl|parquet|graphml]
```

Default format: `jsonl`.

**JSONL format** (default): writes `nodes.jsonl`, `edges.jsonl`, and `rules.jsonl`
to `<dest>` (a directory). Each line is a JSON object. JSONL output is stable,
deterministic, and byte-identical between two runs on the same store.

**Parquet format** (`--format parquet`): writes `nodes.parquet`, `edges.parquet`,
and `rules.parquet` to `<dest>` (a directory) with Snappy compression (parquet-rs
default). Parquet output is **not** byte-identical between parquet-rs library
versions — use JSONL if you need a stable byte checksum.

**GraphML format** (`--format graphml`): writes a single GraphML file — nodes
and edges only, since rules have no GraphML analogue — for import into generic
graph viewers and analysis tools. If `<dest>` is an existing directory, the file
is `<dest>/graph.graphml`; otherwise `<dest>` is the file path itself. Node
elements carry a `label` attribute plus every property (lists and maps are
JSON-encoded as `string`-typed attributes); edge elements carry `type` and
`derived`, plus `rule` and `weight` when present. `weight` is the creating
rule's declared `weight_prop`, read off the edge. GraphML output is
byte-identical between two runs on the same store.

**GraphML attribute types:** `int` properties (mushroomdb's `Value::Int` is a
64-bit `i64`) declare `attr.type="long"`, not `"int"` — GraphML's informal
convention treats `"int"` as 32-bit, so `"long"` is the interoperable choice
for the full range. If nodes disagree on a property's type (one has an int
under `score`, another a string), the key declares `attr.type="string"` for
every node rather than risk a value that doesn't fit a narrower type.

**Float handling:** `Value::Float` fields that are NaN or ±Inf are exported as
JSON `null` (JSONL/Parquet) or an empty `<data>` element (GraphML). Normal
finite floats round-trip correctly.

**Derived edges:** the `derived` field is `true` for rule-derived edges; `rule` is
the rule name (present for derived edges, null/omitted for manual edges).

---

### GET /node/{key}/history

Return the WAL change history for a node.

Response:
```json
{
  "key": "alice",
  "history": [
    { "commit": 0, "change": { "type": "NodeInserted", "label": "Person" } },
    { "commit": 1, "change": { "type": "PropSet", "field": "age", "value": 30 } },
    { "commit": 2, "change": { "type": "EdgeAdded", "edge_type": "KNOWS", "other": "bob", "outgoing": true } }
  ],
  "total_commits": 3,
  "horizon": 0
}
```

`total_commits` is the horizon upper bound (exclusive) — the number of WAL frames
visible in the current window. `horizon` is the oldest commit still retained; it is `0`
until archives are pruned, and when it is greater than `0` the events before it were
pruned and are not in `history`. History before the last WAL-truncating snapshot is not
visible. See [Horizon contract](#horizon-contract) below.

Role tokens: if the requested key is outside the role's visibility mask, the response
is identical to querying an absent key (404) — no existence oracle.

---

### GET /history/edge?a=&b=

Return the full add/retract lifecycle for edges between two nodes.

Response:
```json
{
  "a": "alice",
  "b": "bob",
  "events": [
    { "edge_type": "KNOWS", "commit": 2, "event": "Added", "rule": null },
    { "edge_type": "SIMILAR", "commit": 3, "event": "Added", "rule": "sim_emb" }
  ],
  "total_commits": 4,
  "horizon": 0
}
```

`event` is `"Added"` or `"Retracted"`. `rule` is the rule name for derived edges,
`null` for manually written edges. `total_commits` is the horizon upper bound, and
`horizon` is the oldest commit still retained — events before it were pruned and are not
in `events`.

Role tokens: BOTH `a` AND `b` must be visible in the role mask. If either is hidden,
the response is 404 for that key (no existence oracle).

---

### GET /history/was_linked?a=&b=&edge_type=&at_commit=

Point-in-time check: was an edge of `edge_type` between `a` and `b` active at WAL
commit `at_commit` (0-based)?

Response:
```json
{ "a": "alice", "b": "bob", "edge_type": "KNOWS", "at_commit": 2, "linked": true }
```

Returns 400 (not 500) when `at_commit` is outside the retained horizon. The body carries
the range it will accept, and on a pruned store says what is gone:
```json
{ "error": "commit 3 is out of range; valid range is 12..40 — events before commit 12 are not retained" }
```

On a store that has pruned nothing the floor is `0` and the message is
`commit 999 is out of range; valid range is 0..40`.

Role tokens: BOTH `a` AND `b` must be visible (same-as-absent rule applies).

#### Horizon contract

Valid commit indices are `floor..total_commits`. `total_commits` is the exclusive upper
bound; `floor` is the oldest commit still retained, `0` until archive pruning advances it.
`GET /node/{key}/history` and `GET /history/edge` report that floor as `horizon` in their
response body; `GET /history/was_linked` names both bounds in the 400 it returns for a
commit outside the range. `GET /stats` reports the same floor as `history_floor`.

When the WAL is empty (after a truncating snapshot and before any new writes),
`total_commits` is 0. Pre-snapshot commits are not visible — history restarts from the
first WAL frame after the snapshot. Use
`snapshot_with(SnapshotOptions { keep_wal: true })` to preserve deep history across
snapshots, or `archive_wal: true` to keep it in archives.

---

### GET /watch (WebSocket)

Connect with any WebSocket client. After upgrade the first text frame is
`{"subscribed":true}`. Subsequent frames are `MutationEvent` JSON,
externally tagged snake_case — one frame per committed mutation (or a
lag notice):

```json
{"node_inserted":{"label":"Person","key":"alice"}}
{"prop_set":{"key":"alice","field":"age"}}
{"lagged":3}
```

When the server is started with a token, pass `?token=` on the WebSocket URL.

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
    "serverInfo": {"name": "mushroomdb", "version": "0.6.9"}
  }
}
```

### Tools

Seventeen tools:

| Tool | Description |
|---|---|
| `query` | Run a Cypher query (read or write); params: `cypher`, `params?`, `mask?` (node key allow-list; read-only when set), `role?` (answer as one of the store's roles), `namespace?` (answer from one namespace — intersects `role` and `mask`, never widens them; a role bound to namespaces honours them with no argument), `as_of?` (0-based commit index — answer from the graph as it was then; composes with `role` or with `mask`, not both, since the tool refuses `role` + `mask` together, and with `namespace`; writes and `stub_hidden` refused), `stub_hidden?` (bool; see below) |
| `ingest_json` | Ingest nodes; params: `label`, `rows_json`, `edges?`, `namespace?` (the namespace every node it creates lands in; a row naming a different `ns` is refused) |
| `create_rule` | Declare a linking rule; params: `RuleDef` fields, including `namespace?` (scope the rule to one namespace; omitted is global). Over ~2,048 vectors the reply carries `building: {indexed, total}` and a note: the rule is installed but derives no edges until its vector index is built (see [POST /rules](#post-rules)) |
| `explain` | Explain edges; params: `a`, `b` |
| `stats` | Database statistics, including `namespaces`; params: `role?` and `namespace?`, which narrow the `namespaces` roster (the store-wide counts are unchanged) |
| `neighborhood` | Typed neighborhood; params: `key`, `depth?`, `dir?` |
| `node_info` | Node info and props; params: `key` |
| `node_edges` | Incident edges; params: `key` |
| `upsert_entity` | Insert or update a node by key; params: `key`, `props`, `label?`, `namespace?` (the namespace a created node lands in; on an existing node, the one it is already in is a no-op and another is refused). An update is atomic: every property is checked before any is written, so a refusal leaves the node unchanged. |
| `find_similar` | Two modes: (1) vector search — `vector`, `field?`, `label?`, `k?`, `min?` (default **0.8**), `where?`, `exact?`; (2) edge traversal — `key`, `edge_type?`, `limit?`. Scores are cosine similarity in `[-1, 1]`; a distance of `1 - sim` is the caller's conversion. `where` is a `{field, eq}` / `{field, in}` predicate and implies exact GEMM; `exact` true skips HNSW. Edge-traversal mode ignores both. Vector search under a mask (a role, or the MCP `mask` allow-list) widens its HNSW beam until it has `k` visible hits; if the beam reaches the same cap an exact `VectorSimilar` rule uses (`EF_MAX` = 4,096) it falls back to an exhaustive masked scan. It does not return fewer than `k` while more visible hits exist. HTTP `POST /find_similar` is vector-only and defaults `min` to **0.0**. |
| `pairwise_similar` | Exact cosine top-k among `keys` on `field`; params: `keys`, `field`, `k?` (default 10), `min?` (default 0.0). Self excluded. Never HNSW. |
| `explain_association` | Alias of `explain`; params: `a`, `b` |
| `hybrid_search` | RRF over fulltext + vector; params: `query_text`, `text_field`, `vector?`, `vector_field?`, `label?`, `k?` |
| `node_history` | WAL change history for a node; params: `key`. Returns `{key, history, total_commits, horizon}` |
| `edge_history` | Add/retract lifecycle for edges between two nodes; params: `a`, `b`. Returns `{a, b, events, total_commits, horizon}` |
| `was_linked` | Point-in-time edge check; params: `a`, `b`, `edge_type`, `at_commit`. Returns `{linked}` or error when outside horizon |
| `rename_node` | Rename a node's key; params: `old_key`, `new_key`. Errors if old key absent or new key already exists. |

**`stub_hidden` on the `query` tool:** when `true` and a `mask` is supplied,
hidden nodes in the mask appear as `{"key":"…","restricted":true}` in node-info and
neighborhood results. Cypher result rows are always omit-only — `stub_hidden` does not
change which Cypher rows are returned.

**MCP trust boundary:** the MCP server is a stdio JSON-RPC interface for local trusted
use. It operates without bearer-token authentication and is not subject to role
enforcement. All tools have full-access semantics regardless of any role configuration
on the HTTP server. `role` and `namespace` on `query` and `stats` are therefore a
caller asking to *be answered as* a tenant, not an enforced boundary — which is also
why `stats` answers with the whole namespace roster unless one of them is passed.

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

db = mushroomdb.GraphDb.open("/path/to/db")
reader = mushroomdb.GraphDb.open("/path/to/db", read_only=True)
```

`GraphDb.open` creates the database directory if it does not already exist — no
manual `mkdir` is required.

A read-write handle takes the store's cross-process write lock and raises
`MushroomBusy` if another one already holds it; `read_only=True` never takes the
lock. See [Concurrency](#concurrency-1) below.

### Insert nodes

`insert_node(label, key, props, namespace=None)` — same argument order as Rust.
Raises if the key is already live.

```python
db.insert_node("Person", "alice", {"skills": ["graph", "rust"]})
db.insert_node("Org", "acme", {"skills": ["graph", "rust", "search"]})
db.insert_node("Doc", "a1", {"title": "t"}, namespace="tenant-a")
```

`namespace` (v0.6.6) is the namespace the node is created in — set at insert and
immutable, so `set_prop(key, "ns", …)` afterwards raises. Omitted (or `"default"`)
means the `default` namespace and stores nothing. An invalid name raises `ValueError`;
a `props["ns"]` disagreeing with the argument raises too.

### Upsert nodes

`upsert_node(label, key, props)` returns `"inserted"` or `"updated"`.

```python
db.upsert_node("Person", "alice", {"team": "red"})   # "inserted"
db.upsert_node("Person", "alice", {"team": "blue"})  # "updated"
```

On update it writes only the fields present in `props` whose value differs from
the stored one. Fields you do not pass are left untouched, and an unchanged
field produces no WAL record — so rules do not re-fire needlessly. An existing
key under a different label raises `ValueError`: relabelling a node is not an
upsert.

### Set and remove properties

```python
db.set_prop("alice", "team", "green")
db.remove_prop("alice", "team")        # True if removed, False if already absent
db.set_prop("alice", "team", None)     # identical to remove_prop
```

Python has no distinct "null property" and the store has no null `Value`, so
`None` means absent. Removing a field a rule watches retracts the edges that
field derived.

### Delete a node

```python
report = db.delete_node("alice")
# {"manual_edges": 1, "derived_edges": 3}
```

Deletes the node and every incident edge, returning counts of the user-inserted
and rule-derived edges removed. An unknown or already-deleted key raises
`RuntimeError` (`KeyNotFound`).

### Batch ingest

`ingest_batch(nodes, edges=None)` commits every node and edge in a single WAL
frame. Each node is a `{key, label, props}` dict; each edge is a
`{edge_type, src, dst}` dict.

```python
nodes = [
    {"key": f"node-{i}", "label": "Person", "props": {"value": i}}
    for i in range(10000)
]
report = db.ingest_batch(nodes)
# {"inserted": 10000, "edges_inserted": 0, "row_errors": [], ...}
```

A bad edge rejects the entire batch. Keep each call to ≤10,000 nodes: one
giant WAL frame's fsync cost dominates and negates the batching benefit.

### Create a rule

```python
db.create_rule({
    "name": "skill_fit",
    "src_label": "Person",
    "dst_label": "Org",
    "predicate": {"kind": "overlap", "fields": ["skills"], "min": 0.5},
    "edge_type": "FIT",
    "weight_prop": "score",
    "max_edges": None,
    "approximate": False,
})
```

`create_rule` returns `True` when it created the rule. Pass
`if_not_exists=True` to get `False` instead of an exception when a rule of that
name is already registered:

```python
db.create_rule(rule, if_not_exists=True)   # True first time, False after
```

A `"namespace"` key (v0.6.6) scopes the rule to one namespace — it sees only that
namespace's nodes, so every edge it derives stays inside. Omitted is a global rule.

**Predicate shape.** The canonical form is the snake_case one that `explain`
emits, so an explanation round-trips straight back into a new rule:

| `kind` | extra keys |
|---|---|
| `key_match`, `field_equal` | — |
| `overlap`, `vector_similar` | `min` |
| `numeric_within` | `tolerance` |
| `geo_radius` | `km` |
| `all`, `any` | `parts` (a list of nested predicates) |

The field name comes from `fields[0]` (a bare `"field"` string is also
accepted). The Rust-native externally-tagged form still works:
`{"Overlap": {"field": "skills", "min": 0.5}}`, `{"All": [ … ]}`.

```python
why = db.explain("alice", "acme")
clone = {**base_rule, "name": "skill_fit_v2", "predicate": why[0]["predicate"]}
db.create_rule(clone)
```

**`max_edges` semantics:** Python `None` and a missing key both fill the
predicate default (32 for scored predicates, 512 for KeyMatch). This is the same
as HTTP `null` — neither Python nor HTTP can express the Rust `max_edges: None`
global-budget hatch (1,000,000 per source). Use the Rust API directly if you
need the uncapped budget.

**Symmetric predicates:** when `src_label == dst_label` (e.g., Person-to-Person
similarity), the rule fires in **both directions** — a property change on any
node triggers candidate scanning from that node as both source and destination.
An undirected Cypher pattern like `MATCH (a)-[:SIMILAR]-(b)` double-counts rows
because both `a→b` and `b→a` edges exist. Use directed patterns
(`MATCH (a)-[:SIMILAR]->(b)`) or `RETURN DISTINCT a, b` to avoid duplicates.

### Query

```python
# params as a dict
result = db.query(
    "MATCH (p:Person)-[r:FIT]->(o:Org) WHERE r.score >= $min RETURN p, o, r.score AS score",
    params={"min": 0.5}
)

# params as a list of (name, value) tuples (back-compat form)
result = db.query(
    "MATCH (p:Person) WHERE p.age > $age RETURN p",
    params=[("age", 30)]
)

# no params
result = db.query("MATCH (p:Person) RETURN p")

for row in result:
    print(row["p"], row["score"])
```

`params` accepts a `dict`, a list of `(str, value)` tuples, or `None`. Values
are passed as Cypher parameters — they are never interpolated into the Cypher
string, so string values are safe against injection regardless of content.

`query_with_params(cypher, params)` is a retained back-compat alias for
`query(cypher, params=params)`.

`role` and `namespace` (v0.6.6) restrict what the query may see:

```python
mine = db.query("MATCH (n) RETURN n", role="a-reader", namespace="tenant-a")
```

`role` answers as one of the store's `roles.json` roles and `namespace` from one
namespace; together they **intersect**, so a role bound to `tenant-a` asked for
`tenant-b` returns `[]` rather than the union. Either argument makes the call a read —
a write statement raises — and an invalid namespace name raises `ValueError`.

`query_write(cypher, params=None)` takes the same three param shapes:

```python
db.query_write(
    "MATCH (n:Person) WHERE key(n) = $k SET n.age = 31 RETURN key(n)",
    {"k": "alice"},
)
```

`n.key` / `n.id` read the node key (a stored property of the same name wins).
`key(n)` / `id(n)` always return the id-map key. `key(n)` is available in both
read queries and write-statement `RETURN` projections. See [query.md](query.md).

The dict keys in each row are the **RETURN aliases** from the query — `p`, `o`,
and `score` in the example above. Bare `RETURN n` yields `{"n": ...}`; `RETURN
n.age AS age` yields `{"age": ...}`.

### Traversal

```python
edges = db.node_edges("alice")   # [{edge_type, src_key, dst_key, derived}, ...]
info  = db.node_info("alice")    # {key, label, props}; None for an unknown key
neighbors = db.neighbors("alice", "KNOWS", "out")   # one hop, list of keys
```

`node_info` returns `None` for an unknown key; `node_edges` raises
`RuntimeError`. The asymmetry mirrors the Rust API (`Option` vs `Result`).

### Vector search

`find_similar(field, vector, label=None, k=10, min=0.0, mask=None, where=None, exact=False)`
returns the `k` nearest nodes to `vector` by cosine similarity on `field`.

Scores are cosine similarity in `[-1, 1]`. The engine keeps `score >= min`
(inclusive). A distance of `1 - sim` is the caller's conversion — the engine
does not speak distance. Convert after the call if you need a strict distance
cut (`keep if 1 - sim < T`); do not bake `T` into `min`.

```python
hits = db.find_similar("embedding", query_vec, k=10, min=0.0)
# [(key, similarity), ...]
```

Python `min` defaults to `0.0`. MCP vector mode defaults `min` to `0.8`; that
default is unchanged.

**Exact vs approximate.** When no approximate `VectorSimilar` rule covers
`field`, brute `find_similar` is an exact GEMM. HNSW is still the approximate
path — used when such a rule covers the field and `exact` is false.
`exact=True` forces GEMM and does not consult HNSW. A `where=` predicate also
implies exact. Brute results sort similarity descending, then key ascending;
the HNSW sort is unchanged. A candidate whose embedding is missing, zero-norm,
or a different length than the query is skipped. A zero-norm query returns `[]`.

```python
hits = db.find_similar("embedding", query_vec, exact=True)
hits = db.find_similar(
    "embedding", query_vec, label="Document",
    where={"field": "status", "eq": "published"},
)
```

`where` is the same shape as a role's `visible_where`: exactly one of `eq` or
`in`. A missing property fails. An empty `in` matches nothing. Invalid `where`
(both `eq` and `in`, empty `field`) raises `ValueError` and is never sent into
the engine.

```python
{"field": "resource_scope_id", "eq": "scope-1"}
{"field": "status", "in": ["published", "archived"]}
```

Candidates are `label ∩ mask ∩ where`. `mask` is still a list of keys; it
intersects, never widens. `label` restricts as before. If `label` is set and
`enable_index(label, where.field)` is on, `eq` / `in` use the property index;
otherwise the labelled (or live) set is scanned. `where` is a query argument,
not a role definition — see [masks.md](masks.md#narrowing-a-role-by-a-property)
for the role-side predicate.

`pairwise_similar(keys, field, k=10, min=0.0)` is exact cosine top-k for each
key in `keys`, scored only against that same set. Self-matches are excluded.
It never uses HNSW and does not write edges.

```python
pairs = db.pairwise_similar(["a", "b", "c"], "embedding", k=5, min=0.0)
# [("a", [("b", 0.91), ...]), ("b", [...]), ("c", [])]
```

Treat the result as a map keyed by source: outer order is first-seen packed
keys, not a zip with the input. A packed source with no neighbour above `min`
still appears as `(src, [])`, so "present, nothing similar" is distinct from
"omitted". Unknown keys, missing embeddings, zero-norm and wrong-dimension
vectors are omitted as both query and candidate. Duplicate keys collapse to
first-seen order. Empty `keys` returns `[]`. More than 8192 unique resolved
keys raises. `min` is the same unit and inequality as `find_similar`.

### Degree

Adjacency is a set: a second `insert_edge` of the same `(edge_type, src, dst)`
returns `False` and does not increase degree. `degree` is unique-neighbour
count, not a stored counter and not a row-count of duplicate pairs.

```python
n = db.degree("alice", edge_type="KNOWS", direction="both")  # out + in sum
ranked = db.degrees(label="Person", direction="both", limit=500)
# [("alice", 12), ("bob", 12), ...]  degree desc, key asc
```

`direction` is `"out"`, `"in"`, or `"both"`. `"both"` is the sum of unique
out-neighbours and unique in-neighbours (a reciprocal pair counts 2 at each
endpoint), not the size of the undirected neighbour set.
`neighbors(..., direction="both")` still raises `ValueError` — that API is one
directed hop.

`degree(key)` raises for an unknown key. `degrees` omits unknown keys;
`degrees(keys=[])` returns `[]`. An unknown `edge_type` yields 0.

`degrees` universe: `keys` if given; else `label`; else all live nodes. `where`
intersects (same dict shape as `find_similar`). `limit` applies after the sort.

This is the read-only subset API. It is not `degree_centrality` (full-graph
ranking, time-budgeted, not bound in Python) and not a Degree materialized view
(a write that maintains a property). See [algorithms.md](algorithms.md).

### Rename a node

```python
db.rename_node("alice", "alice2")
```

Raises `KeyNotFoundError` if the old key does not exist, `DuplicateKeyError` if the
new key is already taken.

### Insert edge with auto-create

```python
result = db.insert_edge_upsert("KNOWS", "alice", "bob", "Person")
# result is a dict: {"nodes_created": 1, "edge_inserted": True}
```

Missing endpoint nodes are created with the given `placeholder_label`. If the edge
already exists, `edge_inserted` is `False` (idempotent).

### Explain

```python
explanation = db.explain("alice", "acme")
# [{"rule", "edge_type", "src_key", "dst_key", "weight", "predicate"}, ...]
```

`predicate` is the snake_case summary shape that `create_rule` accepts verbatim.

### Stats

```python
s = db.stats()
# {"nodes_live": N, "nodes_tombstoned": 0, "edges": E, "history_floor": 0,
#  "namespaces": [{"name": "default", "nodes_live": N}], "rules": [...]}
```

`namespaces` (v0.6.6) lists every namespace with at least one live node, in name
order, always including `default` — one entry on a store that has never named one.

### Concurrency

**One writer at a time across processes.** The store carries an advisory write
lock. A read-write handle holds it for as long as it is open, so opening a
second one anywhere on the machine raises `MushroomBusy` rather than letting two
writers corrupt the store. Nothing was written when it raises, so retrying later
is safe:

```python
from mushroomdb import GraphDb, MushroomBusy

try:
    db = GraphDb.open("./db")
except MushroomBusy:
    ...  # another process is writing
```

**Readers see every commit, after `refresh()`.** A handle does not poll the
store, so another process's commits stay invisible until you ask for them.
`refresh()` applies them in place and returns how many arrived — rules fire and
derived edges appear exactly as on a fresh open, and no `close()` and reopen is
needed:

```python
n = db.refresh()
```

**`read_only=True` never takes the lock.** Such a handle opens immediately even
while a writer holds it, writes nothing to disk, raises `RuntimeError` on any
mutation, and can still `refresh()` to follow the writer:

```python
reader = GraphDb.open("./db", read_only=True)
reader.refresh()
```

A commit another process is midway through writing is left alone and picked up
by the next `refresh()`; a partial write is never an error.

Within one process the handle is guarded by a mutex, so calls from multiple
threads are serialized and safe. They are not isolated transactions: readers
can observe intermediate states while a batch is being applied. See
[concurrency.md](concurrency.md) for the full cross-process model and
[durability.md](durability.md) for the WAL and crash-atomicity guarantees.

### Type stubs

The wheel ships `__init__.pyi` (generated from
`bindings/python/mushroomdb.pyi`) plus a `py.typed` marker, so mypy and
Pyright pick up signatures with no extra configuration. Every method also
carries a docstring and a `__text_signature__`, so `help(mushroomdb.GraphDb)`
is useful at the REPL.

### Snapshot

```python
db.snapshot()  # write V8 (mmap rkyv) snapshot and truncate WAL
```

`snapshot()` writes the current state as a V8 mmap snapshot (12 rkyv sections,
zero-copy open) and then truncates the WAL to a minimal baseline. Faster cold
starts, but as-of history (`open_at`) restarts from that point — commits before
the snapshot are no longer reachable via time travel.

**Keep WAL (Rust API):**

```rust
use core_api::{SnapshotOptions};
db.snapshot_with(SnapshotOptions { keep_wal: true })?;
```

`keep_wal: true` writes the V8 snapshot but leaves the WAL intact. `open_at`
can still reach pre-snapshot commits. The WAL replay over the snapshot is
idempotent — no manual recovery is needed. The WAL grows until an explicit
`snapshot()` (with default `keep_wal: false`) truncates it.

**V9 snapshot format:** mmap-able rkyv sections (13 total); zero-copy open via
mmap; no heap allocation for section data. V5–V8 stores are auto-migrated to
V9 on open; a V9 snapshot cannot be opened by a pre-0.6.5 binary, which refuses
it with `snapshot: unsupported version 9`. See [`docs/format-stability.md`](../format-stability.md) for the
full section table and migration notes. 100k nodes (v0.2, ~10M derived edges):
V8 snapshot open 0.02 s / 31–41 MiB RSS (warm file cache, cold process,
2026-08-28, Apple M4 Pro).

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

### Compare-and-set write batches (Rust API)

`write_batch_cas` is the optimistic-concurrency entry point. All preconditions
are checked atomically under the write lock before any operation is applied; if
any precondition fails, the entire batch is rejected with `GraphError::CasConflict`
and no WAL frame is written.

```rust
use core_api::{Precondition, GraphError};

// Read the current last-changed commit for a node.
let seq = db.last_changed("alice")?; // Some(commit_seq) or None if absent/deleted

let result = db.write_batch_cas(
    vec![Precondition::NodeUnchangedSince {
        key: "alice".into(),
        expected: seq.unwrap_or(0),
    }],
    |b| {
        b.set_prop("alice", "role", Value::Str("admin".into()));
    },
);
match result {
    Ok(_) => { /* applied */ }
    Err(GraphError::CasConflict { key, expected, actual }) => {
        // alice was modified between last_changed() and write_batch_cas().
    }
    Err(e) => { /* other error */ }
}
```

**Precondition types:**

| Precondition | Fails when |
|---|---|
| `NodeUnchangedSince { key, expected }` | Node does not exist, or `last_changed(key) != expected` |
| `NodeAbsent { key }` | Node exists (for insert-only semantics) |

**Touch definition:** `last_changed` is updated on `InsertNode`, `SetProp`,
`RemoveProp`, `InsertEdge` (both endpoints), `DeleteEdge` (both endpoints),
`DeleteNode` (entry removed; `last_changed` returns `None` thereafter). History
markers and rule-management records do not update `last_changed`. Values are
persisted in V8 snapshot section 11 (LAST_CHANGE) and survive restarts.

`SharedDb::submit_batch_cas` provides the same guarantee over the shared-writer
async interface. See [timetravel.md](timetravel.md) for the full semantics.

### Exposed surface

The bindings expose: `insert_node`, `ingest_batch`, `batch_edges`,
`create_rule`, `set_prop`, `query` (with `params`), `query_with_params` (alias),
`explain`, `neighbors`, `node_edges`, `node_info`, `find_similar`,
`pairwise_similar`, `degree`, `degrees`, `stats`, `snapshot`,
`refresh`, `rename_node`, `insert_edge_upsert`, plus the `MushroomBusy`
exception.

`write_batch` is not directly exposed in the Python bindings because Rust
closures capturing `&mut BatchBuilder` do not map naturally to the Python
object model. Use `ingest_batch` (node + edge inserts in one frame) or
`batch_edges` (mixed insert/delete edges in one frame) for atomic Python
writes. Both use the same `WalRecord::Batch` frame and one-fsync guarantee.

Not yet exposed: `ingest_json` (use `ingest_batch`), auto-FK inference
(declare `KeyMatch` rules explicitly), `rebuild`.

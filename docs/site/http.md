# HTTP API

Every route `mushroomdb serve` answers. Requests and responses are JSON unless a section says
otherwise; `POST /query` answers Arrow by default.

## Starting the server

Start the server:

```text
mushroomdb serve <db-dir> [--addr 127.0.0.1:8080] [--token <secret>] \
  [--role-token TOKEN:ROLE]... [--ui <dist-dir>] [--no-ui] [--demo-if-empty]
```

Default bind is `127.0.0.1:8080`. The bound address is printed after the
listener is accepting. Non-loopback `--addr` requires `--token` or
`MUSHROOMDB_TOKEN` (see `SECURITY.md`).

## Authentication

For TLS configuration options, see [deployment.md](deployment.md).

**Full-access token** (`--token` / `MUSHROOMDB_TOKEN`): sent as an `Authorization: Bearer`
header, as a `?token=` query parameter, or in the `mushroomdb_token` cookie. The server sets
that cookie itself when it answers an explorer page to a request that carried the full-access
token, so a browser opened with `?token=` stays signed in. The header is read first, then the
query, then the cookie. Grants access to all endpoints.

**Role-bound tokens** (`--role-token TOKEN:ROLE` / `MUSHROOMDB_ROLE_TOKENS="tok1:role1,tok2:role2"`):
read the same three ways, though the server never sets the cookie for one. A role token
receives a node-visibility mask derived from the
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
| `GET /health` | 200 `{"ok":true}` — reachable without a credential, and discloses nothing beyond liveness to anyone but a full-access token |
| `POST /query` (read) | 200 — rows filtered to visible nodes; client `mask` intersects role mask (never widens) |
| `POST /query` (write: `CREATE`/`SET`/`DELETE`/`MERGE`) | 200 if in write scope + target visible; 403 with reason otherwise |
| `POST /nodes` | 200 if `label` in `create_labels`; 403 with scope reason otherwise |
| `DELETE /node/{key}` | 200 if node visible and `label` in `delete_labels`; 403 otherwise |
| `POST /edges` | 200 if type in `create_edge_types` and both endpoints visible; 403 otherwise |
| `DELETE /edges/{t}/{s}/{d}` | 200 if type in `delete_edge_types` and both endpoints visible; 403 otherwise |
| `POST /edges/upsert` | 200 if type in `create_edge_types`, every existing endpoint visible, and — for an endpoint it has to create — `placeholder_label` in `create_labels`; 403 otherwise |
| `PUT /node/{key}/prop/{f}` | 200 if node visible and `label` in `update_labels`; 403 otherwise |
| `DELETE /node/{key}/prop/{f}` | 200 if node visible and `label` in `update_labels`; 403 otherwise |
| `POST /ingest` | 200 if `label` in `create_labels`; 403 otherwise (all-or-nothing) |
| `GET /node/{key}` | 200 if visible; 404 if hidden or absent (indistinguishable) |
| `GET /node/{key}/edges` | 200 if visible; 404 if hidden or absent |
| `GET /node/{key}/neighborhood` | 200 if visible; 404 if hidden or absent |
| `GET /node/{key}/history` | 200 with edge-event entries whose `other` endpoint is hidden filtered out; 404 if the target key is hidden or absent |
| `GET /history/edge` | 200 if both `a` and `b` are visible; 404 for the first invisible key |
| `GET /history/was_linked` | 200 if both `a` and `b` are visible; 404 for the first invisible key |
| `POST /find_similar` | 200 — vector search; client `mask` intersects role mask (never widens) |
| `GET /metrics` | 403 (counters leak graph size beyond the role's subgraph) |
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
predicate, v4 when any role is bound to one or more namespaces). A version this
binary does not recognise poisons the roles state rather than loading it —
denying beats over-granting.

## Write scopes for role-bound tokens

v0.3 lifts the write ban for roles that explicitly declare write scopes in
`schema.json`. Roles without a write scope remain read-only.

### WriteScope schema

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

### Never-widen for writes

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

### Error body shapes

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

### Threat model

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

## Limits, errors and request ids

- **Body size.** A request body may be at most 64 MiB.
- **Deadline.** A request that has not finished in 120 seconds is answered `408 Request Timeout`
  and dropped. The two WebSocket routes are exempt: a subscription is meant to outlive any
  request.
- **Concurrency.** At most 256 requests are in flight at once. The next one is answered
  `503 Service Unavailable` straight away; nothing queues. The WebSocket routes do not count.
- **Request id.** Every response carries `x-request-id`. Send your own, up to 128 ASCII
  characters, and it is echoed; otherwise the server makes one.

An error the server's own handlers raise is one JSON object, `{"error": "<what went wrong>"}`:

| Status | When |
|---|---|
| 400 | A field is missing or wrong, or the engine refused the request: an unknown key on a write, a duplicate key, a rule-owned edge, a query that does not parse |
| 401 | A token is configured and the request carries none, or one the server does not know. Body: `{"error":"unauthorized"}` |
| 403 | A role-bound token asked for something outside its role. The message says what |
| 404 | A key that does not exist on a read route — `GET /node/{key}` and its `/edges`, `/neighborhood` and `/history` — or one the caller's role may not see; `POST /nodes/{key}/rename` on a key that does not exist |
| 409 | `POST /nodes/{key}/rename` onto a key that is taken |
| 413 | A request body over 64 MiB |
| 415 | A route that takes a JSON body was sent without `Content-Type: application/json` |
| 500 | The store is corrupt, or `roles.json` is misconfigured |
| 503 | Another process holds the write lock (`Retry-After: 1`), or the server is at its concurrency limit |

What is refused before a handler runs is not that object. A body that is not valid JSON is
400 with a plain-text message (`Failed to parse the request body as JSON: …`), and 408, 413
and 415 are plain text too. A path that is not a route is 404 with an empty body, unless the
server is serving the explorer, which answers instead; a route asked with the wrong method is
405 with an empty body. A failed `POST /backup` answers 500 with its report.

A write to a key that does not exist is 400, not 404: `DELETE /node/{key}`, the property routes
and `POST /edges` all answer `400 {"error":"node key not found: <key>"}`, and only rename maps
it to 404. For a role-bound token the same write is 403, `target node not visible`: a key it
may not see and a key that does not exist are told apart for no one.

## Routes

| Method | Path | What it does |
|---|---|---|
| `GET` | [`/health`](#get-health) | Liveness; with the full-access token, node and edge counts |
| `GET` | [`/stats`](#get-stats) | Node, edge and rule counts |
| `GET` | [`/metrics`](#get-metrics) | Process and store metrics, as JSON |
| `POST` | [`/backup`](#post-backup) | A verified copy of the store, taken while it serves |
| `POST` | [`/query`](#post-query) | Run a Cypher read or write |
| `POST` | [`/ingest`](#post-ingest) | Insert many nodes of one label |
| `POST` | [`/rules`](#post-rules) | Declare a rule |
| `GET` | [`/suggest`](#get-suggest) | Rules the store proposes from its own values |
| `GET` | [`/explain`](#get-explain) | Why two nodes are linked: the rules, scores and values |
| `POST` | [`/find_similar`](#post-find_similar) | Nearest nodes to a vector |
| `GET` | [`/node/{key}`](#get-nodekey) | A node's label and properties |
| `GET` | [`/node/{key}/edges`](#get-nodekeyedges) | The edges on a node, with the rule behind each |
| `GET` | [`/node/{key}/neighborhood`](#get-nodekeyneighborhood) | What is within a few hops |
| `GET` | [`/node/{key}/history`](#get-nodekeyhistory) | Every change to a node |
| `GET` | [`/history/edge`](#get-historyedge) | When edges between two nodes came and went |
| `GET` | [`/history/was_linked`](#get-historywas_linked) | Whether two nodes were linked on a date or at a commit |
| `POST` | [`/nodes`](#post-nodes) | Create a node |
| `DELETE` | [`/node/{key}`](#delete-nodekey) | Delete a node |
| `PUT` | [`/node/{key}/prop/{field}`](#put-nodekeypropfield) | Set a property |
| `DELETE` | [`/node/{key}/prop/{field}`](#delete-nodekeypropfield) | Remove a property |
| `POST` | [`/nodes/{key}/rename`](#post-nodeskeyrename) | Change a node's key |
| `POST` | [`/edges`](#post-edges) | Create an edge between two nodes that exist |
| `POST` | [`/edges/upsert`](#post-edgesupsert) | Create an edge, and its endpoints if they are missing |
| `DELETE` | [`/edges/{etype}/{src}/{dst}`](#delete-edgesetypesrcdst) | Delete a hand-written edge |
| `POST` | [`/algo/pagerank`](#post-algopagerank) | PageRank |
| `POST` | [`/algo/wcc`](#post-algowcc) | Weakly connected components |
| `POST` | [`/algo/degree`](#post-algodegree) | Degree centrality |
| `GET` | [`/watch`](#get-watch) | WebSocket: every committed mutation |
| `GET` | [`/subscribe`](#get-subscribe) | WebSocket: rule firings, and optionally writes and a query's rows |

## Service

### `GET /health`

Liveness. It needs no token and never answers 401, so a load balancer can call it.

```json
{"ok": true}
```

With the full-access token — or on a server started with no token at all — it also says what
it is serving:

```json
{"ok": true, "nodes": 60, "edges": 334, "addr": "127.0.0.1:8080"}
```

A role-bound token, a wrong token and no token all get `{"ok": true}` and nothing more: the
counts say how big the graph is, and `/stats` refuses a role the same counts.

### `GET /stats`

```json
{
  "nodes_live": 60,
  "nodes_tombstoned": 0,
  "edges": 334,
  "history_floor": 0,
  "chain_truncations": 0,
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

### `GET /metrics`

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

### `POST /backup`

Take a consistent backup of the database. Full-token only; role tokens return 403.

Request body:

```json
{"dest": "/absolute/path/to/backup-dir"}
```

`dest` must fall inside the backup root: `MUSHROOMDB_BACKUP_DIR` if it is set, otherwise the
directory the server was started in. A relative `dest` is resolved under the root; an absolute
one must already be inside it; a `..` segment is refused. Anything else is 400, and the
message names the root. The server must be able to write there.

The server holds the read lock for the **full duration** of the operation: file copies
plus post-copy CRC verification. Writers are blocked for that entire window. For
stores with large snapshots this may be tens of seconds.

The server copies: `snapshot.bin`, `wal.bin`, all `wal.<N>.archive` files,
`wal.floor`, `wal.genesis`, `roles.json`, `commit_times.bin` and, when there is one, `snapshot.intent`. After the copies complete it opens the
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

See [format-stability.md](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md) for the PITR (point-in-time
recovery) workflow using backup together with WAL archives.

## Query, ingest and rules

### `POST /query`

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

### `POST /ingest`

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

### `POST /rules`

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

### `GET /suggest`

Rules the store proposes from its own values. It takes no parameters, reads the whole graph
under a five-second budget, and creates nothing: pass a suggestion's `def` to `POST /rules` to
create it. The same store always gets the same answer.

```json
{
  "suggestions": [
    {
      "def": {
        "name": "suggest_fe_person_person_org_id",
        "src_label": "Person",
        "dst_label": "Person",
        "predicate": {"FieldEqual": {"field": "org_id"}},
        "edge_type": "SAME_ORG_ID",
        "max_edges": 32,
        "weight_prop": null,
        "approximate": false,
        "namespace": null,
        "via_edge": null,
        "via_label": null,
        "via_dir": null
      },
      "est_edges": 60,
      "examples": [["person-04", "person-14", 1.0]],
      "rationale": "…"
    }
  ],
  "truncated": false
}
```

`truncated` is true when the budget ran out before every candidate was scored. A role-bound
token gets 403: the scan would reveal nodes outside its role. More in
[Rule suggestion](suggest.md).

### `GET /explain`

```text
GET /explain?a=person-01&b=proj-01
```

Response: array of explanation objects:

```json
[
  {
    "rule": "skill_fit",
    "edge_type": "FIT",
    "src_key": "person-01",
    "dst_key": "proj-01",
    "weight": 1.0,
    "via_edge": null,
    "predicate": {
      "kind": "overlap",
      "fields": ["skills"],
      "min": 0.5,
      "tolerance": null,
      "km": null,
      "parts": null,
      "approximate": false
    }
  }
]
```

One object for each rule-derived edge between the two keys. `predicate.kind` is `key_match`,
`field_equal`, `overlap`, `numeric_within`, `geo_radius`, `vector_similar`, `all` or `any`; the
fields that do not apply to a kind are `null`. Both keys are required, and a key that does not
exist is `400 {"error":"node key not found: <key>"}`.

### `POST /find_similar`

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

`field` and `vector` are required. `k` defaults to 10.
`min` defaults to **0.8**, as it does over MCP and in the Python binding. It was **0.0** here
through 0.6 — a request that names no `min` now drops every hit below 0.8. Pass `"min": 0.0`
for the old behaviour.
`where` is a `{field, eq}` / `{field, in}` predicate and implies exact GEMM; invalid
`where` is 400. `exact: true` skips HNSW.

Response: `{"hits": [[key, score], …]}` — the same `(key, similarity)` pairs Python
`find_similar` returns. Scores are cosine similarity in `[-1, 1]`; `score >= min`.

## Reading a node

### `GET /node/{key}`

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

### `GET /node/{key}/edges`

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

### `GET /node/{key}/neighborhood`

`depth` and `dir` are query parameters: `GET /node/alice/neighborhood?depth=2&dir=out`.

`depth` defaults to 1. `dir` is `out`, `in`, or `both` (default `both`).

Query params: `mask=key1,key2,…` (optional), `stub_hidden=true` (optional).

With `stub_hidden=true` and a client mask, hidden **direct** neighbors of visited nodes
appear as stub rows (`label: null`). The BFS frontier is not expanded through hidden
nodes — stub rows are terminal.

Returns 404 with `{"error": "node key not found: …"}` for unknown keys, including under
a client mask — the same body as `GET /node/{key}`.

### `GET /node/{key}/history`

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

## History between two nodes

### `GET /history/edge`

Return the full add/retract lifecycle for edges between two nodes. `a` and `b` are query
parameters, both required: `GET /history/edge?a=alice&b=bob`.

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

### `GET /history/was_linked`

Point-in-time check: was an edge of `edge_type` between `a` and `b` active at `at_commit`?
`at_commit` is a 0-based WAL commit index, or an RFC 3339 date — `2026-06-19`, or
`2026-06-19T12:00:00Z` — which resolves to the last commit at or before it. All four
parameters are required: `GET /history/was_linked?a=alice&b=bob&edge_type=KNOWS&at_commit=2026-06-19`.
The `at_commit` in the response is the commit index it resolved to.

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

## Writing nodes and edges

### `POST /nodes`

Create one node.

```json
{"label": "Person", "key": "ada", "props": {"name": "Ada", "skills": ["rust"]}}
```

`label` and `key` are required. `props` is an object; a `null` value in it is dropped.
`namespace`, a string, is optional: it puts the node in that namespace, for life.

```json
{"ok": true, "nodes": 1, "edges": 0}
```

`nodes` and `edges` count what the request itself wrote. The edges that rules derive from the
new node are not in the count; `GET /node/{key}/edges` lists them.

It is an insert, not an upsert: a key that exists is `400 {"error":"duplicate node key: ada"}`.
A missing field is `400 {"error":"missing label"}` or `"missing key"`. A role-bound token may
create a node whose label is in its `create_labels`; otherwise 403 with the reason.

### `DELETE /node/{key}`

Delete a node and the edges on it. `200 {"ok": true}`.

A key that does not exist is `400 {"error":"node key not found: <key>"}`. A role-bound token
may delete a node it can see whose label is in its `delete_labels`; otherwise 403.

Afterwards the key answers 404 on every read route, `/history` included. What the node was
is still in the log until it is pruned: a `POST /query` with `as_of` reads the graph as it stood
before the delete. See [Time travel](timetravel.md).

### `PUT /node/{key}/prop/{field}`

Set one property. Every rule that reads the field is re-evaluated in the same write.

```json
{"value": ["rust", "graphs"]}
```

`value` is required and may be any JSON value but `null`; to remove a property, use `DELETE`.
`200 {"ok": true}`.

400 when the key does not exist, when `value` is missing or `null`, or when the field is one a
materialized view owns. A role-bound token may set a property on a node it can see whose label
is in its `update_labels`; otherwise 403.

### `DELETE /node/{key}/prop/{field}`

Remove one property. It takes no body. `200 {"ok": true}` — also when the node has no such
field, which is not an error.

400 when the key does not exist or a materialized view owns the field. A rule that read the
field retracts the edges it no longer supports, in the same write. Role-bound tokens: as for
`PUT`.

### `POST /nodes/{key}/rename`

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

The rename is one WAL frame. A node that stores its key as an `id` property — one
created by `upsert_entity` or by a Cypher `CREATE {id: …}` — has that `id` rewritten to
the new key in the same frame, so `n.id` and `key(n)` keep agreeing; an `id` holding any
other value is left alone. All edges referencing the
node continue to work under the new key. Node history and time-travel (`open_at`)
correctly scope events to the identity that held the key at each commit — recycling
a key for a different node does not contaminate the previous identity's history.

### `POST /edges`

Create one edge between two nodes that exist.

```json
{"type": "KNOWS", "src": "ada", "dst": "grace"}
```

All three are required. `200 {"ok": true}` — also when the edge is already there.

400 when a rule already owns that edge, or when the two nodes are in different namespaces. A
missing endpoint is `400 {"error":"node key not found: grace"}`; to have it created, use
`POST /edges/upsert`, whose body names the same three things differently (`edge_type`,
`src_key`, `dst_key`). A role-bound token may create an edge whose type is in its
`create_edge_types` between two nodes it can see; otherwise 403.

### `POST /edges/upsert`

Insert an edge, auto-creating any missing endpoint nodes. A role-bound token may do so when
the edge type is in its `create_edge_types` and it can see each endpoint that exists. An
endpoint that does not exist is created for it only when `placeholder_label` is in its
`create_labels`. Otherwise 403.

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

### `DELETE /edges/{etype}/{src}/{dst}`

Delete one edge that was written by hand. `200 {"ok": true}` — also when there is no such edge
between the two nodes.

An edge a rule derived cannot be deleted this way:

```json
{"error": "edge is rule-owned: edge FIT person-01→proj-01 is rule-owned; delete or change the owning rule"}
```

That is 400, as is an endpoint that does not exist. A role-bound token may delete an edge whose
type is in its `delete_edge_types` between two nodes it can see; otherwise 403.

## Graph algorithms

### `POST /algo/pagerank`

PageRank over hand-written and derived edges together. The body is a JSON object and may be
`{}`: every field has a default. It may not be empty. A field the config does not have is 400
on all three `/algo` routes, so `max_iter` for `max_iters` is refused and not run with the
default.

| Field | Default | What it is |
|---|---|---|
| `damping` | 0.85 | The damping factor, from 0 to 1; anything else is 400 |
| `max_iters` | 50 | Iterations before giving up |
| `tol` | 1e-6 | Stop when the scores' total change in one iteration falls below it |
| `direction` | `"out"` | `"out"`, `"in"` or `"both"` |
| `edge_type` | all | Count only edges of this type |
| `weight_prop` | none | Weight each edge by this property |
| `min_weight` | none | Ignore edges whose weight is below it |
| `budget_ms` | 5000 | Stop after this long; 0 for no limit |

```json
{"scores": [["org-07", 0.1029], ["org-03", 0.0882]], "converged": true}
```

Scores are highest first. `converged` is false when the iterations or the budget ran out first.

A value the config does not accept is 400, and the message names what it would have taken:

```json
{"error": "invalid pagerank config: unknown variant `sideways`, expected one of `out`, `in`, `both`"}
```

```json
{"error": "invalid pagerank config: unknown field `max_iter`, expected one of `damping`, `max_iters`, `tol`, `edge_type`, `direction`, `budget_ms`, `weight_prop`, `min_weight`"}
```

A role-bound token gets 403 on every `/algo` route, because the scan covers the whole graph.
More in [Algorithms](algorithms.md).

### `POST /algo/wcc`

Weakly connected components. Fields: `edge_type`, `weight_prop`, `min_weight` and `budget_ms`;
`{}` is valid. Here `weight_prop` weights nothing: with `min_weight` it only leaves out the
edges below that weight.

```json
{"components": [["org-01", "org-01"], ["org-02", "org-01"]], "truncated": false}
```

Each pair is a node and its component, named by the smallest key in it. `truncated` is true
when the budget ran out.

### `POST /algo/degree`

Degree centrality. Fields: `direction` (default `"both"`), `edge_type`, `weight_prop`,
`min_weight` and `budget_ms`; `{}` is valid. As for components, `weight_prop` and `min_weight`
only leave edges out; the score is a count.

```json
{"scores": [["org-04", 11], ["org-05", 11]], "truncated": false}
```

Highest first.

## Live streams

### `GET /watch`

Connect with any WebSocket client. After upgrade the first text frame is
`{"subscribed":true}`. Subsequent frames are `MutationEvent` JSON,
externally tagged snake_case — one frame per committed mutation (or a
lag notice):

```json
{"node_inserted":{"label":"Person","key":"alice"}}
{"prop_set":{"key":"alice","field":"age"}}
{"lagged":3}
```

The other frames are `prop_removed`, `edge_inserted` and `edge_deleted` (`edge_type`, `src`,
`dst`), `node_deleted` (`key`), `rule_created`, `rule_deleted` and `rule_rebuilt` (`name`),
`batch_applied` (`ops`) and `ingested` (`label`, `inserted`). The lag notice here is
`{"lagged": n}`; on `/subscribe` it is `{"type": "lagged", "missed": n}`.

When the server is started with a token, send it as a header or a cookie, or as `?token=` on
the WebSocket URL when the client cannot set headers. A role-bound token gets 403.

### `GET /subscribe`

A WebSocket that streams what rules do, and optionally every write and the rows of one query.
After the upgrade, send one text frame saying what you want:

```json
{"rules": ["skill_fit"], "writes": true, "cypher": "MATCH (p:Person) RETURN p.name LIMIT 50"}
```

Ask for at least one of the three. `rules` names the rules whose edges to follow, `writes` adds
node, edge and property events, and `cypher` follows the rows of one query, re-run on each
commit — give it a `LIMIT`. The server answers `{"subscribed": true}`. A frame that asks for
nothing gets that answer and is then closed, having nothing to send. A frame that is not JSON,
a rule that does not exist or a query that does not parse gets `{"error": "…"}` and is closed.

Then one frame an event, each with a `type`:

| `type` | Fields |
|---|---|
| `edge_fired` | `rule`, `src_key`, `dst_key`, `edge_type`, `weight` |
| `edge_retracted` | `rule`, `src_key`, `dst_key`, `edge_type` |
| `node_inserted` | `label`, `key` |
| `node_deleted` | `key` |
| `edge_inserted`, `edge_deleted` | `edge_type`, `src`, `dst` |
| `prop_set`, `prop_removed` | `key`, `field` |
| `query_row_added`, `query_row_removed` | `columns`, `row` |

Every event but the two query-row ones also carries `commit_seq`, the commit it happened at.
`weight` is there only when the rule scores its edges; a key-match rule's `edge_fired` has
none. The values in `row` are in the store's tagged form — `[{"Str": "Ada"}]`, not `["Ada"]`.
Frames of different kinds are not ordered against each other: a write event may arrive after
a rule event from a later commit, so order by `commit_seq`.

A consumer that falls behind is not disconnected. It gets `{"type": "lagged", "missed": 12}`
for the events it did not see, and the stream goes on.

Only writes made through this server process are seen: a commit another process made is
readable at once and announces nothing. A role-bound token gets 403, because the stream has
no mask. More in [Subscriptions](subscriptions.md).

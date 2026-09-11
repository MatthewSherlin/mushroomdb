# Node masks and access control

mushroomdb has two layered mechanisms for restricting what nodes a caller can see:
**role-bound tokens** (coarse-grained, enforced server-side) and **client node masks**
(fine-grained, per-request allow-lists). This page covers how they compose and how
restricted nodes are presented to callers.

---

## Role-bound tokens

A role token is a bearer credential tied to a named role whose label selectors are
defined in `roles.json`. The server computes the role mask at request time and
intersects it with any client mask — it can never widen the intersection.

- Hidden nodes return 404 on node-info/edges/neighborhood endpoints (existence is
  not disclosed).
- Cypher rows referencing hidden nodes are filtered out entirely.
- Roles without a declared write scope get 403 on all write and analytics endpoints.
- Roles with a write scope may write within their declared scope — but only to nodes
  currently in their read mask. See [api.md](api.md#write-scopes-for-role-bound-tokens)
  for the full write-scope surface, error body shapes, and the threat model.
- An unknown token or a role name not present in `roles.json` returns 401.

**Never-widen invariant (read):** a client-supplied mask is always intersected with
the role mask. A caller with a role token cannot supply a mask that expands their view.

**Never-widen invariant (write):** no write a role performs may make data visible to
itself or any other party that was not already visible before the write. A role token
may only mutate nodes that are currently in its read mask. Hidden nodes are treated
as non-existent in all write responses — no existence oracle.

---

## Client node masks

Full-access callers can supply a `mask` allow-list to any read path (query, node-info,
edges, neighborhood, MCP query). Nodes not in the mask are hidden. Default behavior
when no mask is supplied is to show all nodes.

---

## Restricted-stub mode (`stub_hidden`)

By default, hidden nodes are **omitted** from responses entirely — their existence is
not disclosed. This is `MaskMode::Omit`, the default for all paths.

`MaskMode::Stub` is an opt-in that discloses existence without leaking any data.
When enabled, a hidden node appears as:

```json
{"key": "alice", "restricted": true}
```

No label, no properties, no other fields.

**This is a deliberate existence disclosure.** Use it only when your application
needs to indicate that a node exists but is not accessible to the requester. If you
need to conceal both content and existence, use the default omit mode.

### How to enable stub mode

Add `stub_hidden=true` to HTTP query params or the POST /query body:

```
GET /node/alice?stub_hidden=true
GET /node/alice/edges?stub_hidden=true&mask=alice,carol
GET /node/alice/neighborhood?stub_hidden=true&mask=alice,carol
POST /query   body: {"cypher": "...", "stub_hidden": true}
```

For the MCP `query` tool, pass `stub_hidden: true` in the tool arguments.

### Behavior per endpoint

| Endpoint | Omit mode (default) | Stub mode |
|---|---|---|
| `GET /node/{key}` | 404 if hidden | `{"key":…,"restricted":true}` |
| `GET /node/{key}/edges` | hidden endpoint omitted from list | edge object kept; endpoint field is `{"key":…,"restricted":true}` |
| `GET /node/{key}/neighborhood` | hidden direct neighbors omitted | hidden direct neighbors appear as stub rows (`label: null`); BFS does not expand through them |
| `POST /query` | hidden nodes omitted from Cypher rows | Cypher rows: omit-only (same as Omit mode — see below) |
| MCP `query` | hidden nodes omitted | Cypher rows: omit-only |

### Cypher query results are always omit-only

Cypher executes over the full graph topology and cannot partially reveal nodes as
stubs mid-result. In both Omit and Stub mode, Cypher rows that reference hidden nodes
are omitted from the result set entirely. The `stub_hidden` flag has no effect on
Cypher result rows.

### Edges to restricted endpoints

When stub mode is active and an edge touches a restricted node, the edge object is
included in the response but the restricted endpoint is rendered as a stub:

```json
{
  "edges": [
    {"edge_type": "KNOWS", "src_key": "alice", "dst_key": {"key": "bob", "restricted": true}, "derived": false}
  ]
}
```

The `edge_type` and `derived` fields are present on every edge object in stub mode.
This is in-contract: an edge's existence and type are disclosed when the non-hidden
endpoint is visible.

### Role tokens never use stub mode

Role paths always use `MaskMode::Omit`. The `stub_hidden` parameter is silently
ignored on any request authenticated with a role token — except with `as_of`, where
it is refused rather than ignored (`as_of (time-travel) does not compose with
stub_hidden`; see [Composing with `as_of`](#composing-with-as_of)). A role caller
never receives stub responses — hidden nodes are fully omitted.

### MCP trust boundary

The MCP interface (`mushroomdb mcp`) is a stdio JSON-RPC server for local trusted
use; it operates without bearer-token authentication and is not subject to role
enforcement. The `stub_hidden` arg on the MCP `query` tool applies the client mask
in stub mode, but there is no role layer enforcing minimum visibility.

The MCP `query` tool also takes a `role`, which resolves a name from `roles.json`
to the same node mask the HTTP role path would compute and applies it as a client
mask. It is a convenience for asking "what would this role see", not a credential:
any caller may name any role, and passing both `role` and `mask` is rejected. Real
enforcement is the HTTP server's role tokens (`serve --role-token`).

---

## Composing masks

When a full-access caller supplies both a role context (via the server config) and a
client mask, the server intersects them. When a full-access caller supplies only a
client mask, that mask is applied directly. When no mask is supplied, all nodes are
visible.

Summary of what each caller class sees:

| Caller | Mask applied |
|---|---|
| No token (no auth endpoint) | n/a |
| Full-access token, no mask | all nodes |
| Full-access token + client mask | client mask |
| Role token | role mask ∩ client mask (if any) |

### Composing with `as_of`

A mask composes with time travel. `POST /query` accepts `as_of` alongside a
role token or a client `mask`, and the MCP `query` tool accepts `as_of`
alongside `role` or `mask`. Every key and label is resolved against the graph
**as it was at that commit**, so a key that did not exist yet resolves to
nothing and a role that may see a label sees exactly the nodes that carried it
then. The intersection rule is unchanged: a client mask can only narrow a
role, never widen it. Writes are refused at any commit.

The graph is historical; the role *definition* is not. `roles.json` is a
sidecar and is never a WAL record, so there is no past version of it to read —
an as-of read applies today's role definition to the graph as it was then. See
[timetravel.md](timetravel.md).

**`stub_hidden` does not compose with `as_of`** — the pair is rejected with
`as_of (time-travel) does not compose with stub_hidden`. Stub mode exists to
disclose that a hidden node *exists*, and node existence at a past commit is
exactly the question an as-of read is asking; answering it through a stub
would leak the historical shape of the graph outside the mask. Drop
`stub_hidden` or drop `as_of`.

#### Deletion is not retroactive

**Deleting a node does not remove it from a role's past.** A role that may see
the `Public` label reads a now-deleted `Public` node — and the edges it had —
at any retained commit where it was live. A role with `keys: ["k"]` reads `k`
at a past commit even though `k` is gone today. `DELETE` changes the present;
it does not rewrite the WAL. **To revoke history, prune the archives** (see
[timetravel.md](timetravel.md) — WAL archives and retention) **or narrow the
role**, which takes effect at every commit at once because the role definition
is always the current one.

#### Keys are not identities

A role's `keys` name **whichever node held that key at the commit asked for**.
Renaming a node frees its key, and a later node may take it. A role with
`keys: ["alice"]` that reads at an old commit sees the node that was called
`alice` *then* — not the one called `alice` now. Grant by label, or by a key
you do not recycle, when that distinction matters.

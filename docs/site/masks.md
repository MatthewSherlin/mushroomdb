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
- Write and analytics endpoints return 403.
- An unknown token or a role name not present in `roles.json` returns 401.

**Never-widen invariant:** a client-supplied mask is always intersected with the role
mask. A caller with a role token cannot supply a mask that expands their view.

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
ignored on any request authenticated with a role token. A role caller never receives
stub responses — hidden nodes are fully omitted.

### MCP trust boundary

The MCP interface (`mushroomdb mcp`) is a stdio JSON-RPC server for local trusted
use; it operates without bearer-token authentication and is not subject to role
enforcement. The `stub_hidden` arg on the MCP `query` tool applies the client mask
in stub mode, but there is no role layer enforcing minimum visibility.

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

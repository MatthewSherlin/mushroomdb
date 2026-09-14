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

### Narrowing a role by a property

A label is often too coarse: every document is a `Document`, but a reviewer should
see only the published ones. A role may carry one **property test**, `visible_where`,
beside its labels:

```json
{
  "version": 3,
  "roles": [
    {
      "name": "reader",
      "keys": [],
      "labels": ["Document"],
      "visible_where": { "field": "status", "in": ["published", "archived"] },
      "write": null
    }
  ]
}
```

The resolved mask is:

```
visible = keys ∪ { n : label(n) ∈ labels ∧ predicate(n) }
```

- The predicate narrows the **labels leg only**. `keys` is an administrative grant
  and is never narrowed by it.
- **A missing property fails the predicate.** A `Document` with no `status` at all
  is not visible to the role above. Absent is not a match — that is the deny-side
  answer, and it is the one a narrowed role wants.
- Resolution stays live. A node whose `status` changes to `published` is visible on
  the next read; one edited out of the predicate is gone on the next read. No
  re-apply of the schema is needed either way.
- Write values as plain JSON scalars — `"published"`, `3`, `true`. The graph's own
  tagged encoding (`{"Str": "published"}`, `{"Int": 3}`, `{"Bool": true}`) is accepted
  too, including mixed within one `in` list, and means exactly the same thing; it is
  what the server writes back when it rewrites the sidecar. Comparison is by value,
  not by rendering.
- A value that is neither — an object that is not a tagged value, say — is refused at
  load, which poisons the roles state rather than dropping the narrowing.

Only two operators exist:

| Form | Meaning |
|---|---|
| `"eq": <value>` | the property equals this value |
| `"in": [<value>, …]` | the property equals one of these values — an empty list matches nothing |

Exactly one of the two must be set, and `field` must not be empty; `apply_schema`
refuses anything else. There are no ranges, no negation, and no nesting, and that is
deliberate: a mask that can express arbitrary predicates is a query language with a
security boundary attached, and every operator added is another shape the resolver
has to be right about, on the deny side, forever. A `visible_where` on a role that
declares no labels is also refused — it would narrow nothing, and a name that reads
like a restriction should never be one.

**Version 3 is refused by an older binary.** A sidecar carrying a predicate is written
as `{"version": 3, …}`, and a binary that predates predicates does not recognise the
version, so it poisons its roles state and denies every role instead of loading the
file and resolving the role to its whole label set. Denying is the safe direction;
silently ignoring a narrowing is not. (Versions 1 and 2 still load, and a role without
`visible_where` behaves exactly as it did before version 3 existed.)

---

## Namespaces

A namespace is a **tenancy boundary**: a second visibility axis that crosses the
label one. Where `labels` and `visible_where` answer "what kind of node, with what
property", a namespace answers "whose data".

A node's namespace is a reserved node property, `ns`:

```json
{ "label": "Document", "key": "d1", "props": { "ns": "tenant-a", "status": "published" } }
```

- **Absent `ns` means the namespace `default`.** Every node in a store that has
  never named a namespace is in `default`, so nothing about such a store changes
  when this version is installed.
- **A store that already used `ns` for something else does change.** `ns` is
  reserved from 0.6.6 on, and existing string values are read as namespace names
  on the first open: those nodes leave `default`, `ns` becomes unwritable on
  them, and a value outside the name rules above can be listed by `stats` where
  no surface will accept it back. There is no migration — rename the property
  before upgrading if it meant something else.
- **Writing `ns: "default"` explicitly stores nothing.** A single-tenant store
  carries no `ns` column at all and pays nothing for the feature.
- **A name is 1–64 characters of `[A-Za-z0-9_.-]`.** `/` is excluded on purpose:
  keys already contain it, and a namespace must never read as a key prefix. An
  invalid name, or a non-string `ns`, is refused at insert.
- **A node's props name `ns` once, or not at all.** Two `ns` entries in one insert
  are refused (`ns is given more than once; a node has exactly one namespace`):
  with two, "the node's namespace" stops being a single fact, and a write checked
  against one entry could land under the other.
- **Keys do not change.** A namespace is not a key prefix — `key(n)`, every history
  body, every mask entry and every `KeyMatch` target keeps the exact value it has.
  Keys stay globally unique across namespaces.

### Set at insert, immutable after

`insert_node`, `/ingest`, Cypher `CREATE` and every other create path accept `ns`
like any other property. Changing it afterwards is refused — `set_prop`, Cypher
`SET`, `MERGE`'s property merge and every upsert that merges props:

```
node d1 is in namespace tenant-a; a namespace is set at insert and cannot be
changed to tenant-b — delete and re-insert the node instead
```

**Removing `ns` is changing it**, to `default` — the namespace an absent property
names — so it is refused with the same error: `remove_prop`, a batched
`RemoveProp`, and `DELETE /node/{key}/prop/ns` all return it. (The Cypher dialect
has no `REMOVE`, so that spelling is a query error before it reaches this check.)

Writing the namespace a node is already in is a no-op, not an error, and so is
removing `ns` from a node already in `default`: neither changes a namespace, and
neither takes a commit. `rename_node` changes the key, not the namespace. Moving a
node between tenants is a deletion from one and a creation in the other, and saying
so is more honest than a property edit that silently re-homes every edge the node
carries.

A materialized view may not own the `ns` column either: a `view_prop` of `"ns"` is
refused when the view is created, because a view rewrites its column on every
relevant change and that is exactly what immutability forbids.

### A role binds to namespaces — `roles.json` version 4

```json
{
  "version": 4,
  "roles": [
    {
      "name": "tenant-a-reader",
      "keys": [],
      "labels": ["Document"],
      "visible_where": { "field": "status", "in": ["published"] },
      "namespaces": ["tenant-a"],
      "write": null
    }
  ]
}
```

The full resolution, all three legs:

```
visible = ( keys ∪ { n : label(n) ∈ labels ∧ visible_where(n) } )
          ∩ { n : ns(n) ∈ namespaces }
```

- **Absent `namespaces` is unscoped** — exactly the behaviour every role had before
  version 4, so no existing role changes meaning.
- **The namespace leg intersects `keys` too**, unlike `visible_where`, which narrows
  only the label leg. A namespace is a tenancy boundary, and an explicitly named key
  in another tenant's namespace is a mistake rather than an administrative grant:
  `apply_schema` refuses a role whose `keys` name a **live** node outside its
  namespaces, naming both the key and its namespace. A key naming no live node is
  still silently ignored, as it is today.
- **`"namespaces": []` is refused.** A role that sees nothing is written by omitting
  `keys` and `labels`, not by closing the namespace leg.
- Resolution stays live, as the other two legs do: a node inserted into the role's
  namespace is visible on the next read.

**Version 4 is refused by an older binary**, for the same reason version 3 is: a
binary that does not know `namespaces` would resolve a tenant-scoped role across
every tenant. An unrecognised version poisons the roles state and denies every role.
Versions 1–3 still load, and version 4 is written **only** when some role actually
carries a `namespaces` binding — a store that uses no namespaces keeps the sidecar
version it had.

### A role writes only into its own namespaces

A role bound to `namespaces` may only **create** a node inside them. The never-widen
rule is about what a write makes visible to *any* party, not only to the writer: a
node the role could never read back would be a write into somebody else's tenancy.
The refusal is a 403 with

```
role-bound token: namespace '<ns>' not in the role's namespaces
```

and it covers `POST /nodes`, a batch, Cypher `CREATE`, and the node `MERGE` creates. A
create without an `ns` is a create in `default`, so an `x`-bound role is refused there
too. A role with no `namespaces` binding is unchanged: it writes wherever its label
scope allows.

A placeholder endpoint `POST /edges/upsert` would auto-create is refused by the same
rule — a placeholder carries no props, so it lands in `default` — but the message is
the endpoint one, `role-bound token: edge endpoint not visible`, because that arm
fires only for an endpoint which does **not** exist and the visibility arm only for one
which does: two different strings there would be an existence oracle. Hidden ≡ absent,
as everywhere else.

Updates need no separate rule — a role can only mutate nodes already in its read
mask, and the namespace leg has already narrowed that.

**`MERGE` creates in the default namespace, for every caller.** A `MERGE` pattern
carries exactly one identifying property (`MERGE (n:Doc {id: 'x'})`), and only that
property reaches the node it creates, so there is no way to name a namespace in a
`MERGE` — and `ON CREATE SET n.ns = …` cannot stand in for one, because that is a
namespace change and is refused as one. The consequences, stated plainly:

- A role bound to namespaces **cannot `MERGE`-create**: the node would land in
  `default`, which it may not write. It gets the namespace refusal above.
- Its `MERGE` **match** arm is unaffected — the node it matches is already in the
  role's mask, and `ON MATCH SET` works as it always has.
- To create a node in a namespace, use `CREATE (n:Doc {id: 'x', ns: 'tenant-a'})`,
  `insert_node`, or `/ingest`, all of which take `ns` like any other property.

This is deliberately the loud answer rather than the convenient one. A role bound to
exactly one namespace *could* have its creates default into that namespace, but then
the same statement would write different data under different tokens, and it would
write a property the caller never named. If that default is wanted, it belongs
alongside an explicit `namespace` argument on the write surfaces, decided once.

### No cross-namespace edges

A user-written edge stays inside one namespace:

```
edge d1 → e1 crosses a namespace boundary (tenant-a → tenant-b);
only a global rule may derive one
```

Every insert-edge path goes through the same check, single op or batch. Only a
**global** rule — one with no `namespace` — may derive an edge across the boundary;
see [rules.md](rules.md#namespace-scoping). On an existing store every node is in
`default`, so nothing that works today stops working.

### Time travel

A role bound to a namespace resolves that binding against the commit being read, so
an as-of read under a tenant-scoped role sees that tenant's nodes **as they were
then**. The rule is the one as-of always follows: the graph is historical, the role
definition is current. A node's namespace, unlike its label or its properties,
cannot have changed — it is immutable — so there is no second case to explain.

`stats` carries a `namespaces` list with the live node count per namespace, always
including `default`.

### Namespaces on every surface

Every surface takes a `namespace`, and it means the same thing everywhere: **one more
leg intersected into whatever restriction already applies.** It can only narrow.

| Surface | How to pass it |
|---|---|
| MCP `query` | `"namespace": "tenant-a"` beside `role`, `mask` and `as_of` |
| MCP `stats` | `"role"` and/or `"namespace"` narrow the `namespaces` roster |
| MCP `upsert_entity`, `ingest_json` | `"namespace"` — the namespace a created node lands in |
| MCP `create_rule` | `"namespace"` — scope the rule (see [rules.md](rules.md#namespace-scoping)) |
| `POST /query` | `"namespace"` in the body |
| `POST /nodes`, `POST /ingest` | `"namespace"` in the body, applied to every node created |
| `POST /rules` | `"namespace"` in the `RuleDef` |
| `GET /stats` | the `namespaces` roster (full-access tokens only — see below) |
| CLI `query` | `--namespace <ns>`, optionally with `--role <name>` |
| CLI `asof` | `--namespace <ns>` |
| CLI `stats`, `doctor` | a `namespaces:` line / a `, N namespaces` clause, both omitted on a single-namespace store |
| Python | `insert_node(..., namespace=)`, `query(..., role=, namespace=)`, `stats()["namespaces"]`, `create_rule({"namespace": …})` |

The composition rules, which hold on all of them:

- **Namespace alone** — the mask is that namespace's live nodes.
- **Namespace with a role** — `mask_for_role(role) ∩ mask_for_namespace(ns)`. A role
  bound to `tenant-a` asked for `tenant-b` answers with **nothing**. Never the union.
- **Namespace with a client key mask** — the same intersection, so a namespace narrows
  an allow-list and an allow-list narrows a namespace.
- **A role bound to namespaces needs no argument at all.** Its binding is part of the
  one resolver every read path calls, so `POST /query`, `GET /node/{key}`,
  `/edges`, `/neighborhood` and the history routes narrow with nothing passed, and a
  node in another namespace answers exactly as an absent key does (404, no stub).
- **Namespace with `as_of`** — both legs resolve against the graph at that commit.
- **Absent** — no namespace restriction at all. Not `default`: `"default"` is how you
  ask for the nodes that name no namespace.
- **An unused name is an empty mask**, never everything. An *invalid* name is refused
  outright (`namespace "…" is not a valid namespace name — 1 to 64 characters of
  [A-Za-z0-9_.-]`), because a typo that silently answers "nothing" reads like an empty
  store.
- **Either argument makes the call a read.** A restricted write is refused, as it
  already was with `role` or `mask`.

On the write surfaces `namespace` is the same write-once `ns` property, named on the
call instead of buried in the props. A row or a `props` object that carries its own
`ns` naming a *different* namespace is refused before anything is written — one node
is created in one namespace. On `upsert_entity` over a node that already exists the
namespace is written like any other property, so naming the one it is already in is a
no-op and naming another is the `NamespaceImmutable` refusal above.

### What `stats` discloses

The `namespaces` roster is the one part of `stats` that is a list of *other tenants*.

- `GET /stats` **denies role tokens entirely** (403,
  `role-bound token: /stats requires a full-access token`), as it did before namespaces
  existed, so no tenant-scoped HTTP client ever sees the roster.
- MCP `stats` has no token — it is the local stdio surface, as trusted as the store
  directory — so it answers in full by default and takes `role` / `namespace` to narrow
  the roster for a caller that is answering as a tenant. The store-wide counts beside
  it are unchanged: they were never per-namespace.
- The CLI prints the whole roster; it is the operator's own shell.

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
to the same node mask the HTTP role path would compute — `visible_where` included —
and applies it as a client mask. It is a convenience for asking "what would this role see", not a credential:
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

A `visible_where` predicate follows the same rule and is evaluated against the
**property values at the commit being read**. A document that was a draft then
and is published now is outside a `status in ["published"]` role at that past
commit, and inside it today. Narrowing a role therefore takes effect at every
commit at once, which is what makes it a revocation.

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

The same caveat reaches the history endpoints. `GET /node/{key}/history` under
a role token filters out `EdgeAdded`/`EdgeRemoved` entries whose other
endpoint the role cannot see — but that filter resolves the other endpoint's
key against **today's** ids, not the ids as of the historical event. A key
that was reused since the event can be let through, or held back, on the
strength of who holds it now rather than who held it then.

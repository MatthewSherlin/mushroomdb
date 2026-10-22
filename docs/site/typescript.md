# TypeScript client

`mushroomdb-client` is a typed wrapper over the [HTTP API](http.md) and its WebSocket stream.
It talks to a running `mushroomdb serve`; it does not embed the engine.

```sh no-run
npm install mushroomdb-client
```

It needs Node 18 or later, and runs in a browser. In a Node without a global `WebSocket`,
install `ws` as well if you use subscriptions.

## Quick start

```ts
import { MushroomClient } from 'mushroomdb-client';

const client = new MushroomClient('http://127.0.0.1:8080', { token: 'your-token' });

// A read.
const result = await client.query(
  'MATCH (p:Person) RETURN p.id AS id, p.name AS name LIMIT 10',
);
console.log(result.columns); // ['id', 'name']
console.log(result.rows);    // [['person-01', 'Alice'], ...]

// A read with a parameter.
const one = await client.query(
  'MATCH (p:Person {id: $pid}) RETURN p.name AS name',
  { params: { pid: 'person-01' } },
);

// Why are these two linked?
const why = await client.explain('person-01', 'proj-01');
```

The token is sent as an `Authorization: Bearer` header, and on the WebSocket URL as `?token=`
for subscriptions. Leave the second argument out for a server started without one.

## Methods

Each method calls one route. The route's page has the request and the response in full.

| Call | What it does | Route |
|---|---|---|
| `client.query(` | Run a Cypher read or write | [`POST /query`](http.md#post-query) |
| `client.ingest(` | Insert many nodes of one label | [`POST /ingest`](http.md#post-ingest) |
| `client.stats(` | Node, edge and rule counts | [`GET /stats`](http.md#get-stats) |
| `client.suggest(` | Rules the store proposes from its own values | [`GET /suggest`](http.md#get-suggest) |
| `client.explain(` | Why two nodes are linked | [`GET /explain`](http.md#get-explain) |
| `client.createRule(` | Declare a rule | [`POST /rules`](http.md#post-rules) |
| `client.node(` | One node's label and properties; `null` when it does not exist | [`GET /node/{key}`](http.md#get-nodekey) |
| `client.neighborhood(` | What is within a few hops of a node | [`GET /node/{key}/neighborhood`](http.md#get-nodekeyneighborhood) |
| `client.algo(` | PageRank, connected components or degree, by name | [`POST /algo/pagerank`](http.md#post-algopagerank), [`/wcc`](http.md#post-algowcc), [`/degree`](http.md#post-algodegree) |
| `client.renameNode(` | Change a node's key | [`POST /nodes/{key}/rename`](http.md#post-nodeskeyrename) |
| `client.upsertEdge(` | Create an edge, and its endpoints if they are missing | [`POST /edges/upsert`](http.md#post-edgesupsert) |
| `client.subscribe(` | Stream rule and write events | [`GET /subscribe`](http.md#get-subscribe) |
| `client.health(` | Server liveness and node/edge counts (when authenticated) | [`GET /health`](http.md#get-health) |
| `client.metrics(` | Detailed runtime metrics including slow-query log | [`GET /metrics`](http.md#get-metrics) |
| `client.findSimilar(` | Approximate nearest-neighbour vector search | [`POST /find_similar`](http.md#post-find_similar) |
| `client.createNode(` | Insert a node by label and key | [`POST /nodes`](http.md#post-nodes) |
| `client.deleteNode(` | Remove a node; throws when it does not exist | [`DELETE /node/{key}`](http.md#delete-nodekey) |
| `client.nodeEdges(` | All edges attached to a node | [`GET /node/{key}/edges`](http.md#get-nodekeyedges) |
| `client.nodeHistory(` | Per-commit change log for a node | [`GET /node/{key}/history`](http.md#get-nodekeyhistory) |
| `client.edgeHistory(` | Edge presence log between two nodes | [`GET /history/edge`](http.md#get-historyedge) |
| `client.wasLinked(` | Whether two nodes were linked at a given commit | [`GET /history/was_linked`](http.md#get-historywas_linked) |
| `client.createEdge(` | Insert an explicit (non-derived) edge | [`POST /edges`](http.md#post-edges) |
| `client.deleteEdge(` | Remove an explicit edge | [`DELETE /edges/{etype}/{src}/{dst}`](http.md#delete-edgesetypesrcdst) |
| `client.setNodeProp(` | Set or overwrite one property on a node | [`PUT /node/{key}/prop/{field}`](http.md#put-nodekeypropfield) |
| `client.removeNodeProp(` | Delete one property from a node | [`DELETE /node/{key}/prop/{field}`](http.md#delete-nodekeypropfield) |
| `client.backup(` | Copy the store to a directory and verify it | [`POST /backup`](http.md#post-backup) |
| `client.beginTransaction(` | Open a transaction and return its handle | [`POST /tx/begin`](http.md#post-txbegin) |
| `client.transaction(` | Run a callback in a transaction; commit when it resolves, roll back when it throws | [`POST /tx/begin`](http.md#post-txbegin), [`POST /tx/{tx_id}/commit`](http.md#post-txidcommit), [`POST /tx/{tx_id}/rollback`](http.md#post-txidrollback) |
| `client.watch(` | Stream the raw mutation log over WebSocket | [`GET /watch`](http.md#get-watch) |

## Transactions

```ts
import { TransactionConflictError } from 'mushroomdb-client';

// The callback form commits when the callback resolves and rolls back,
// rethrowing, when it throws.
const seenCount = await client.transaction(async (tx) => {
  await tx.execute('CREATE (:Person {id: $id})', { id: 'p1' });
  const seen = await tx.execute('MATCH (p:Person {id: $id}) RETURN p.id AS id', { id: 'p1' });
  return seen.rows.length; // sees its own write
});

// The explicit form.
const tx = await client.beginTransaction();
try {
  await tx.execute('CREATE (:Person {id: $id})', { id: 'p2' });
  const commitNumber = await tx.commit();
} catch (err) {
  if (err instanceof TransactionConflictError) {
    // a node it read or wrote changed after begin: retry the whole unit of work
  }
  await tx.rollback();
}
```

`tx.execute(cypher, params?)` returns the same `{ columns, rows }` as `client.query`. `tx.commit()` resolves to the commit number. A conflict at commit throws `TransactionConflictError`; a transaction the server no longer knows (expired, finished, or lost in a restart) throws `TransactionExpiredError`. Both extend `MushroomError`. The client does not retry. If a rollback fails after the callback threw, the callback's error is still the one rethrown, with the rollback failure on its `rollbackError` property. If `commit()` fails at the network level its outcome is unknown and the handle refuses further use; a failed `rollback()` can be retried.

## What it does not wrap

All HTTP routes and WebSocket endpoints have a method on the client.

Creating a node, setting a property and deleting can also be done through `client.query`, with
a Cypher `CREATE`, `SET` or `DELETE`.

## Query options

`client.query(cypher, opts?)` accepts three optional fields beyond `params`:

| Option | Type | What it does |
|---|---|---|
| `as_of` | `number` | Time-travel: run the query on the graph as it was at this WAL commit index (0 to `commit_seq − 1`, read-only). A commit index from a `nodeHistory`/`edgeHistory` entry is the usual source. |
| `mask` | `string[]` | Node-key allow-list: the query sees only nodes whose key is in this set. Makes the query read-only. |
| `namespace` | `string` | Restrict results to this namespace, intersecting with `mask` and any role. Makes the query read-only. |

## Dates, durations and binary

Five value types travel as one-key JSON objects, and a `CellValue` can be any of them:

| Type | Form | Example |
|---|---|---|
| Date | `{ $date }` | `{ $date: '2024-01-15' }` |
| Local date-time | `{ $ldt }` | `{ $ldt: '2024-01-15T12:34:56.789012' }` |
| Date-time | `{ $dt }` | `{ $dt: '2024-01-15T12:34:56.789012+05:30' }` |
| Duration | `{ $dur }` | `{ $dur: 'P1Y2M3DT4H5M6.789S' }` |
| Binary | `{ $binary }` | `{ $binary: '3q2+7w==' }` (base64, at most 1 MiB) |

The text is ISO 8601, and a date-time keeps the UTC offset it was written with (within ±18:00).
`asDate`, `asLocalDateTime`, `asDateTime`, `asDuration` and `asBinary` narrow a cell to its text, or
return `undefined` for any other cell; `binaryBytes` decodes a binary cell to a `Uint8Array`.

```ts
import { asDate } from 'mushroomdb-client';

const { rows } = await client.query('MATCH (e:Event) RETURN e.on AS on');
const day = asDate(rows[0][0]); // '2024-01-15', or undefined if the cell is not a date
```

These types are not yet storable: a write that carries one is refused, naming the type, until the
format that holds them lands.

## Errors

A response that is not a success throws `MushroomError`. Its `detail`, which is also its
`message`, is what the server said went wrong. Empty or plain-text error bodies (e.g. 404
with no JSON, or 413) surface as `MushroomError("HTTP <status>")` rather than a raw parse
error.

```ts
import { MushroomError } from 'mushroomdb-client';

try {
  await client.query('BAD CYPHER');
} catch (err) {
  if (err instanceof MushroomError) {
    console.error('Query failed:', err.detail);
  }
}
```

## Subscriptions

```ts
const handle = await client.subscribe(
  { rules: ['skill_fit'], writes: true },
  (ev) => {
    switch (ev.type) {
      case 'edge_fired':
        console.log(`${ev.rule}: ${ev.src_key} -> ${ev.dst_key} (commit ${ev.commit_seq})`);
        break;
      case 'lagged':
        console.warn(`missed ${ev.missed} events; re-read what you depend on`);
        break;
    }
  },
);

// Later:
await handle.close();
```

- A subscription sees only writes made through the server it is connected to.
- It does not reconnect. When the connection drops, no more events arrive; subscribe again.
- A `lagged` event means the server's queue for this subscriber overflowed and some events were
  not sent. The stream goes on.
- In a Node without a global `WebSocket`, pass the `ws` constructor as `wsConstructor` in the
  first argument.

Every option of every method is in the package's own
[README](https://github.com/MatthewSherlin/mushroomdb/blob/main/clients/typescript/README.md).

# TypeScript client

`mushroomdb-client` is a typed wrapper over the [HTTP API](http.md) and its WebSocket stream.
It talks to a running `mushroomdb serve`; it does not embed the engine.

```sh no-run
npm install mushroomdb-client
```

It needs Node 18 or later, and runs in a browser. In Node before 21, install `ws` as well if you
use subscriptions.

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

The token is sent as an `Authorization: Bearer` header. Leave the second argument out for a
server started without one.

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

## What it does not wrap

Fifteen of the server's routes have no method on the client. For these, call the route with
`fetch`; the [HTTP API](http.md) page has each request and response.

- Reading: `GET /health`, `GET /metrics`, `GET /node/{key}/edges`, `GET /node/{key}/history`,
  `GET /history/edge`, `GET /history/was_linked`, `POST /find_similar`
- Writing: `POST /nodes`, `DELETE /node/{key}`, `PUT` and `DELETE /node/{key}/prop/{field}`,
  `POST /edges`, `DELETE /edges/{etype}/{src}/{dst}`
- Operating: `POST /backup`
- Streaming: `GET /watch`

Creating a node, setting a property and deleting can also be done through `client.query`, with
a Cypher `CREATE`, `SET` or `DELETE`.

## Errors

A response that is not a success throws `MushroomError`. Its `detail`, which is also its
`message`, is what the server said went wrong.

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
- In Node before 21, pass the `ws` constructor as `wsConstructor` in the first argument.

Every option of every method is in the package's own
[README](https://github.com/MatthewSherlin/mushroomdb/blob/main/clients/typescript/README.md).

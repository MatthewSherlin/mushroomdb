# mushroomdb-client

TypeScript client for the [mushroomdb](https://github.com/MatthewSherlin/mushroomdb) graph database.
Wraps the HTTP and WebSocket API exposed by `mushroomdb serve`.

> **Not yet published to npm.** Install from the repo path (see below).

---

## Installation (from repo)

```sh
npm install /path/to/graph-db/clients/typescript
```

Or add to `package.json`:

```json
{
  "dependencies": {
    "mushroomdb-client": "file:../path/to/graph-db/clients/typescript"
  }
}
```

Node 18+ required. Fetch is built-in. For WebSocket in Node < 21, install `ws`:

```sh
npm install ws
```

---

## Quick start

```ts
import { MushroomClient } from 'mushroomdb-client';

const client = new MushroomClient('http://127.0.0.1:8080');

// Read query
const result = await client.query(
  'MATCH (p:Person) RETURN p.id AS id, p.name AS name LIMIT 10',
);
console.log(result.columns); // ['id', 'name']
console.log(result.rows);    // [['person-01', 'Alice'], ...]

// Parameterised read
const row = await client.query(
  'MATCH (p:Person {id: $pid}) RETURN p.name AS name',
  { params: { pid: 'person-01' } },
);

// Write (CREATE, MERGE, SET, DELETE)
await client.query("CREATE (n:Company {id: 'acme', name: 'Acme Corp'})");

// Bulk ingest
await client.ingest({
  label: 'Product',
  rows: [
    { id: 'prod-1', name: 'Widget', price: 9.99 },
    { id: 'prod-2', name: 'Gadget', price: 24.99 },
  ],
});

// Stats
const stats = await client.stats();
console.log(stats.nodes_live, stats.edges);

// Rule suggestions
const report = await client.suggest();
console.log(report.truncated, report.suggestions.length);

// Graph algorithms
const pr = await client.algo('pagerank');
console.log(pr.scores.slice(0, 5)); // top-5 nodes by PageRank

const wcc = await client.algo('wcc');
console.log(wcc.components.length);

const deg = await client.algo('degree', { direction: 'out' });
console.log(deg.scores.slice(0, 5));
```

---

## WebSocket subscriptions

Subscribe to post-commit events over `GET /subscribe`.

**No auto-reconnect in v1.** When the connection drops, no further events are
delivered. Reconnect manually if required.

**The `lagged` event** is passed to `onEvent` like any other event. It fires
when the server's per-subscriber queue (65,536 events) overflows. On receiving
it, re-read the affected graph state for lossless consumers.

```ts
import WS from 'ws'; // Node < 21: npm install ws
import type { WsConstructor } from 'mushroomdb-client';

const handle = await client.subscribe(
  {
    rules: ['skill_fit'],   // rule-fire events
    writes: true,           // node/prop write events
    wsConstructor: WS as unknown as WsConstructor, // Node < 21 only
  },
  (ev) => {
    switch (ev.type) {
      case 'edge_fired':
        console.log(`Rule ${ev.rule}: ${ev.src_key} → ${ev.dst_key} (seq ${ev.commit_seq})`);
        break;
      case 'node_inserted':
        console.log(`New node: ${ev.key} (label ${ev.label})`);
        break;
      case 'lagged':
        console.warn(`Missed ${ev.missed} events — re-read state`);
        break;
    }
  },
);

// ... do work ...

// Always await close to avoid dangling handles.
await handle.close();
```

---

## API reference

### `new MushroomClient(baseUrl: string, opts?: { token?: string })`

Create a client. `baseUrl` is the HTTP base URL printed by `mushroomdb serve`,
e.g. `"http://127.0.0.1:8080"`. When `opts.token` is set, every HTTP fetch
sends `Authorization: Bearer <token>`.

### `client.query(cypher, opts?) → Promise<QueryResult>`

Run a Cypher query (read or write). The server detects write statements and
acquires the write lock automatically. Returns `{ columns: string[], rows: CellValue[][] }`.

`opts.params` — bound parameters; values must be JSON scalars
(`string | number | boolean`).

### `client.ingest(req) → Promise<IngestReport>`

Bulk-ingest nodes and optional edges. `req.label`, `req.rows` are required.
See `IngestOptions` and `IngestEdge` for advanced options.

### `client.stats() → Promise<Stats>`

Returns live node/edge counts and per-rule statistics.

### `client.suggest() → Promise<SuggestReport>`

Profile the database and return candidate linking rules. CPU-intensive;
capped at 5 s server-side. The `truncated` flag is `true` when the budget
fires early.

### `client.explain(a, b) → Promise<Explanation[]>`

Wires `GET /explain?a=&b=`. Returns rule-derived edges between two node keys.

### `client.createRule(def) → Promise<void>`

Wires `POST /rules` with a `RuleDef` JSON body.

### `client.node(key) → Promise<NodeInfo | null>`

Wires `GET /node/{key}`. Returns `null` for an unknown key (HTTP 404).

### `client.neighborhood(key, opts?) → Promise<Neighborhood>`

Wires `GET /node/{key}/neighborhood`. `opts.depth` defaults to the server's
(1). Result columns are `key`, `label`, `depth`.

### `client.algo(name, config?) → Promise<AlgoReport>`

Run a graph algorithm:

| name | config type | result type |
|------|-------------|-------------|
| `"pagerank"` | `PageRankConfig` | `PageRankReport` — `{ scores: [key, score][], converged }` |
| `"wcc"` | `WccConfig` | `WccReport` — `{ components: [key, component_id][], truncated }` |
| `"degree"` | `DegreeConfig` | `DegreeReport` — `{ scores: [key, degree][], truncated }` |

All config fields are optional; server defaults apply.

### `client.subscribe(opts, onEvent) → Promise<SubscribeHandle>`

Open a WebSocket subscription. Returns a handle with `close(): Promise<void>`.

---

## Error handling

All HTTP errors throw `MushroomError` with the server's error detail in
`err.detail` (and `err.message`):

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

---

## Known server-side limitations

These are limitations of the mushroomdb server's Cypher implementation that
affect how you write queries through this client.

### 1. `CREATE ... RETURN` is supported

You can include a `RETURN` clause directly after `CREATE` or `MERGE` to get
back the created or matched bindings in a single statement:

```ts
// Single-statement create + return:
const result = await client.query(
  "CREATE (n:Widget {id: 'w1', name: 'Sprocket'}) RETURN n.name AS nm"
);
// result.rows[0][0] === 'Sprocket'

// MERGE + RETURN (returns the node whether created or matched):
const r2 = await client.query(
  "MERGE (n:Tag {id: 'rust'}) RETURN n"
);
```

### 2. Every node requires a string `id` property

When using `CREATE`, nodes must include an `id` field with a string value.
This is the key the server uses to identify the node:

```ts
// WRONG — missing 'id'
await client.query("CREATE (n:Widget {name: 'Sprocket'})");

// CORRECT
await client.query("CREATE (n:Widget {id: 'w1', name: 'Sprocket'})");
```

---

## Node-only vs browser-compatible

| Feature | Browser | Node 18+ |
|---------|---------|----------|
| `query`, `ingest`, `stats`, `suggest`, `algo`, `explain`, `createRule`, `node`, `neighborhood` | Yes (uses `fetch`) | Yes |
| `subscribe` | Yes (uses global `WebSocket`) | Requires `wsConstructor` option + `ws` package |

---

## Running the tests

```sh
cd clients/typescript
npm ci
npm test
```

Tests spawn a real `mushroomdb` binary. The first run builds it with `cargo`
(takes ~30 s on a cold cache). If the build fails, all tests are **skipped**
with a clear message (not marked as failed).

Set `CARGO=/path/to/cargo` to use a specific cargo binary (required in CI or
on machines where cargo is not on `PATH`).

---

## License

Apache-2.0. Copyright 2026 Matthew Sherlin.

# TypeScript quickstart

The `mushroomdb-client` package sends requests to a running HTTP server. This
example uses Node 18 or later; the same client is typed for TypeScript projects.

Start the server on a fresh store in one terminal:

```sh no-run
npx mushroomdb serve ./quickstart-db --token quickstart-local-token
```

In another terminal, install the client:

```sh no-run
npm install mushroomdb-client
```

Save this ES module as `quickstart.mjs`:

```ts
import { MushroomClient } from "mushroomdb-client";

const client = new MushroomClient("http://127.0.0.1:8080", {
  token: "quickstart-local-token",
});
const alice = "quickstart-alice";
const bob = "quickstart-bob";
await client.createNode("Person", alice, { name: "Alice" });
await client.createNode("Person", bob, { name: "Bob" });
await client.createEdge("KNOWS", alice, bob);
const result = await client.query(
  "MATCH (a:Person)-[:KNOWS]->(b) WHERE key(a) = $alice AND key(b) = $bob RETURN a.name, b.name",
  { params: { alice, bob } },
);
if (JSON.stringify(result.columns) !== '["a.name","b.name"]' ||
    JSON.stringify(result.rows) !== '[["Alice","Bob"]]') {
  throw new Error("Unexpected graph result");
}
console.log(JSON.stringify(result.rows));
```

```sh no-run
node quickstart.mjs
```

It prints `[["Alice","Bob"]]` in one row array. The server retains the graph in
`quickstart-db`; use fresh keys or a fresh store when repeating the inserts.
Queries return a column list and a two-dimensional row array. Stop the server
with Ctrl-C when finished.

## What's next

- [TypeScript reference](typescript.md): queries, errors and subscriptions.
- [Agent memory](quickstart.md): memory operations over the same graph.
- [Deployment](deployment.md): service configuration and persistence.

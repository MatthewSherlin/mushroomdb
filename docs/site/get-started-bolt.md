# An existing Cypher app, over Bolt

An application written against the official graph-database drivers can talk to mushroomdb without a
new client library: `mushroomdb serve` speaks the Bolt protocol beside HTTP. This page gets such an app
connected and lists what to check in its queries. The protocol details are in
[Bolt protocol](bolt.md).

## 1. Start the server with Bolt on

```sh no-run
mushroomdb serve ./db --token your-token --bolt-port 7687
```

It prints both listeners:

```text
listening on http://127.0.0.1:8080
listening on bolt://127.0.0.1:7687
```

The server binds `127.0.0.1` unless `--addr` says otherwise. To reach it from another machine or a
container, bind a public address and tell drivers which host to use; see
[The routing table](bolt.md#the-routing-table). For TLS, start the server with `--tls-cert` and
`--tls-key`, and Bolt serves the same certificate; see [TLS](bolt.md#tls).

## 2. Point the driver at it

Change three settings in the driver's configuration and nothing else:

| Setting | Value |
|---|---|
| URI | `bolt://127.0.0.1:7687`, or the driver's routing scheme on the same address. Use the `+s` form of either scheme when the server has TLS. |
| Authentication | Basic authentication with any user name and the token as the password, or a bearer token. With no token configured on the server, any credentials are accepted. |
| Database | Leave it unset, or use the driver's default name, or `mushroomdb`. |

A role token or an access key works the same way as on HTTP, with the same scope; see
[Authentication](bolt.md#authentication).

Working programs that do exactly this, with the official Python and JavaScript drivers, are in the
repository: [`tests/bolt/python_compat.py`](https://github.com/MatthewSherlin/mushroomdb/blob/main/tests/bolt/python_compat.py)
and [`tests/bolt/js_compat.mjs`](https://github.com/MatthewSherlin/mushroomdb/blob/main/tests/bolt/js_compat.mjs).
CI runs both against every change.

## 3. Check the app's queries

Auto-commit statements, explicit and managed transactions, bookmarks, streaming and result summaries
work as the drivers expect. What most often needs a change in an existing app:

- **One label per node.** A node has exactly one label, set when it is created; see
  [Concepts](concepts.md).
- **`id(n)` is the node's key**, not the integer id a driver shows for a node. Match on the key, or on
  the element id; see [Nodes, relationships and paths](bolt.md#nodes-relationships-and-paths).
- **One relationship of a type in each direction between two nodes.**
- **The Cypher subset.** [Query](query.md) lists the supported clauses and functions;
  [openCypher conformance](conformance.md) has the measured coverage.
- **Value types.** Time, LocalTime, points and date-times with a named time zone are refused as
  parameters; see [Value types](bolt.md#value-types).
- **Schema statements** (`CREATE INDEX`, `CREATE CONSTRAINT`) cannot run inside a transaction; run them
  on their own.
- **Transactions are snapshot-isolated.** A conflicting commit fails with a transient error that a
  managed transaction retries. See [Transactions](transactions.md), including what snapshot isolation
  does not prevent.

## Next

- [Drivers and Bolt](drivers-bolt.md): which driver features are supported.
- [Bolt protocol](bolt.md): flags, limits, the version handshake and the value mapping.

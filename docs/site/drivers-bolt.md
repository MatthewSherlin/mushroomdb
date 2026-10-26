# Drivers and Bolt

`mushroomdb serve --bolt-port <port>` accepts connections from the official graph-database drivers over
the Bolt protocol. The server speaks Bolt 5.0 and 4.4; every current official driver negotiates 5.0.
To connect an application, see [An existing Cypher app, over Bolt](get-started-bolt.md). For the flags,
limits and wire details, see [Bolt protocol](bolt.md).

## Which drivers are checked

| Driver | How it is checked |
|---|---|
| Official Python driver 6.4.1 | Twelve application scenarios, against a release build, on every pull request and push to `main` |
| Official JavaScript driver 6.2.0 | The same twelve scenarios, in the same CI job |
| Official Java, Go and .NET drivers | Their handshake source was audited; they are not run in CI |

The scenarios and the audit are in [Driver compatibility tests](bolt.md#driver-compatibility-tests)
and [Driver compatibility matrix](bolt.md#driver-compatibility-matrix).

## Driver features

| Feature | Supported | Notes |
|---|---|---|
| Auto-commit statements | Yes | With parameters, write counters and result summaries; see [Write counters](bolt.md#write-counters) |
| Explicit transactions | Yes | `BEGIN` / `COMMIT` / `ROLLBACK`; see [Transactions](transactions.md) |
| Managed read and write transactions, with retry | Yes | A commit conflict is the transient `DeadlockDetected`, which the driver retries |
| Bookmarks across sessions | Yes | See [Transactions and bookmarks](bolt.md#transactions-and-bookmarks) |
| Routing scheme | Yes | One address for every role; see [The routing table](bolt.md#the-routing-table) |
| TLS | Yes | The `+s` schemes; see [TLS](bolt.md#tls) |
| Streaming in batches | Yes | `PULL` with a fetch size |
| Nodes, relationships and paths as structures | Yes, for bare variables and lists of them | See [Nodes, relationships and paths](bolt.md#nodes-relationships-and-paths) |
| `EXPLAIN` plans | Yes | `summary.plan`; see [EXPLAIN](bolt.md#explain) |
| Catalog procedures | Yes | `db.labels()` and the others in [Catalog procedures](bolt.md#catalog-procedures) |
| Database selection | One database | The default name, or `--bolt-database` |
| `PROFILE` | No | |
| Impersonation | No | Naming a user fails with `Forbidden` |
| Re-authentication on an open connection (Bolt 5.1 and later) | No | Not offered |
| Time travel, masks and namespaces per statement | No | HTTP, and time travel also embedded (`open_at`); see [Not available over Bolt](bolt.md#not-available-over-bolt) |

## Access control

A connection runs with its credential's scope, exactly as HTTP does: a role token sees and writes what
its role allows, a `read` key cannot write, and a role-scoped transaction is read-only until 0.11. See
[Scoped access](bolt.md#scoped-access).

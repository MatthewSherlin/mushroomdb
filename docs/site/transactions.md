# Transactions

A transaction groups several Cypher statements into one atomic commit. Each statement reads the store
as it was when the transaction began, plus the transaction's own earlier writes. `COMMIT` writes
everything as one commit, or fails and writes nothing. `ROLLBACK`, or abandoning the transaction,
writes nothing.

Transactions are available everywhere a statement can run:

| Surface | Begin | Reference |
|---|---|---|
| Rust, embedded | `SharedDb::begin_transaction()` | [Rust](#rust) |
| Python, embedded | `with db.transaction() as tx:` or `db.begin_transaction()` | [Python](#python), [Python guide](python.md#transactions) |
| HTTP | `POST /tx/begin` | [HTTP](#http), [HTTP API](http.md#transactions) |
| TypeScript client | `client.transaction(fn)` or `client.beginTransaction()` | [TypeScript](#typescript), [TypeScript client](typescript.md#transactions) |
| Bolt drivers | `BEGIN` / `COMMIT` / `ROLLBACK`, and the drivers' managed transactions | [Bolt](#bolt), [Bolt protocol](bolt.md#transactions-and-bookmarks) |

Every surface runs the same engine, so the guarantees below hold on all of them.

## What a transaction guarantees

- **Atomic.** A commit is one WAL frame and one commit number. Replay after a crash applies the whole
  frame or none of it, and [time travel](timetravel.md) sees the transaction as a single commit.
- **Reads see the transaction's own writes.** A `CREATE` in one statement is matched by the next,
  a `SET` is read back, a `DELETE` hides the node. Other clients see none of it until the commit.
- **Snapshot isolation.** `BEGIN` freezes the store as it is. Commits made by other writers after that
  point are not seen inside the transaction.
- **Conflicts are detected.** The transaction records every node it reads or writes, with the commit
  that last changed it. If another writer changed any of those nodes after `BEGIN`, or created a node
  under a key the transaction looked up and did not find, the commit fails with a conflict and writes
  nothing. Lost updates are therefore impossible: every node a transaction writes is in that set.
  Reading or writing a relationship records both of its end nodes.
- **Validation happens at commit.** Unique constraints, view-owned properties, namespace and
  edge-ownership checks run when the transaction commits, against committed state plus the
  transaction's writes. A failure rejects the whole transaction.
- **Rules and views fire at commit.** Derived relationships and view values that the transaction's
  own writes would produce are not visible inside it. They appear once it commits, as they would for
  the same writes made outside a transaction.
- **A statement is atomic inside the transaction.** A statement that fails buffers nothing; on HTTP
  and in the embedded APIs the transaction stays usable. (Bolt ends the transaction on a failure, as
  its state machine requires; see [Bolt](#bolt).)
- **Nothing is written before the commit.** An open transaction exists only in the memory of the
  process that holds it. A crash, a restart or a killed process rolls back every open transaction, and
  the store reopens with the commits made before it and nothing of the transaction.
- **A transaction that wrote nothing** commits without writing a frame, and returns the commit number
  it began on.

### What snapshot isolation does not prevent

A read records the nodes it matched, not the condition it matched them with. Two transactions that
each check that something does not exist yet, and each create it, can both commit (write skew through
a phantom):

```cypher
-- Run by two transactions at once. Both see 0, both create a Leader, both commit.
MATCH (n:Leader) RETURN count(n) AS c
CREATE (n:Leader {id: $id})
```

Two ways to make such a check safe:

- Give the decision a node both transactions write, for example
  `MATCH (g:Guard {id: 'leader'}) SET g.claimed_by = $id`. The second commit then conflicts.
- Declare a [unique constraint](query.md#schema-statements-ddl) on what must be unique, for example on a
  `slot` property that every leader sets to the same value. The second commit is then refused.

Also outside the conflict set:

- nodes bound only by a `MATCH` that follows a `WITH`, and the intermediate nodes of a variable-length
  or shortest path (their end nodes are in it);
- the relationships another writer's `DETACH DELETE` removes along with their node. A transaction that
  read or wrote only a neighbour of that node still commits. A transaction's own `DETACH DELETE`
  records both ends of every relationship it removes.

## Statements inside a transaction

Every read and every write statement that runs on its own runs inside a transaction, with the same
results and the same counters: `CREATE`, `MERGE`, `MATCH … SET` (including the `SET n = {…}` and
`SET n += {…}` map forms), `REMOVE`, `DELETE`, `DETACH DELETE`, the per-row forms `MATCH … CREATE`,
`MATCH … MERGE` and `UNWIND … <write>`, and any of these with `RETURN`. `EXPLAIN` and the catalog
procedures (`CALL db.labels()` and the others) run too, and a procedure reads the transaction's own
writes. See [Write statements](query.md#write-statements).

Schema statements are refused inside a transaction: `CREATE INDEX`, `DROP INDEX`, `CREATE CONSTRAINT`
and `DROP CONSTRAINT`. Run them on their own. `SHOW INDEXES` and `SHOW CONSTRAINTS` are allowed and
list the schema as it was at `BEGIN`.

## Errors, and which to retry

| What happened | Rust | Python | TypeScript | HTTP | Bolt | Retry? |
|---|---|---|---|---|---|---|
| Another writer changed a node the transaction read or wrote | `GraphError::CasConflict` | `TransactionConflict` | `TransactionConflictError` | `409`, `"code": "conflict"` | `DeadlockDetected` (transient) | Yes, the whole transaction from `BEGIN` |
| A unique constraint refuses the commit | `GraphError::ConstraintViolation` | `ConstraintViolation` | `ConstraintViolationError` | `409`, `"code": "constraint"` | `ConstraintValidationFailed` (client error) | No: it fails again until the clashing value changes |
| A second request while one is running on the same transaction | | | `MushroomError`, `code` `"busy"` | `409`, `"code": "busy"` | | Yes, the request, once the first returns |
| The transaction timed out or the server restarted | | | `TransactionExpiredError` | `410`, then `404` | `TransactionTimedOut` | Yes, from `BEGIN` |
| A schema statement inside the transaction | `GraphError::QueryError` | `QueryError` | `MushroomError`, status 400 | `400` | `SyntaxError` | No: run it outside the transaction |

A failed commit has written nothing, and the transaction is over either way.

## Rust

`SharedDb::begin_transaction` returns a `Transaction`. It holds no lock, is `Send`, and can be kept
and moved between threads while open; other readers and writers carry on meanwhile. Dropping it
without calling `commit` is a rollback.

```rust
use mushroomdb::{GraphError, SharedDb, Value};
use std::collections::BTreeMap;

let db = SharedDb::open(&dir)?;
let no_params = BTreeMap::new();

let mut tx = db.begin_transaction();
tx.execute("CREATE (a:Account {id: 'a', balance: 100})", &no_params)?;
tx.execute("CREATE (b:Account {id: 'b', balance: 0})", &no_params)?;

// The transaction sees its own writes.
let rs = tx.execute("MATCH (n:Account) RETURN count(n) AS c", &no_params)?;
assert_eq!(rs.get(0, "c"), Some(&Value::Int(2)));

match tx.commit() {
    Ok(commit) => println!("committed as commit {commit}"),
    // Nothing was written: run the transaction again from begin_transaction().
    Err(GraphError::CasConflict { key, .. }) => println!("conflict on {key}; retry"),
    Err(other) => return Err(other),
}
```

- `execute(cypher, &params)` runs one statement and returns its `ResultSet`. A write returns the shape
  it returns outside a transaction.
- `commit()` consumes the transaction and returns the commit number. `rollback()` consumes it and
  writes nothing.
- `base_version()` is the commit number every statement reads.
- An embedded transaction has no timeout. Keep it short: it holds its snapshot and its buffered writes
  in memory until it ends.

## Python

```python
import mushroomdb
from mushroomdb import TransactionConflict

db = mushroomdb.GraphDb.open("./db")

with db.transaction() as tx:
    tx.execute("CREATE (n:Account {id: 'a', balance: 100})")
    tx.execute("CREATE (n:Account {id: 'b', balance: 0})")
    rows = tx.execute("MATCH (n:Account) RETURN n.id AS id")  # sees both
```

The block commits when it ends cleanly, and rolls back and re-raises when it raises. Without a `with`:

```python
tx = db.begin_transaction()
try:
    tx.execute("MATCH (n:Account {id: $id}) SET n.balance = n.balance - 10", {"id": "a"})
except Exception:
    tx.rollback()
    raise
commit = tx.commit()  # the commit number
```

A commit that conflicts has written nothing, so the transaction can be run again:

```python
for _ in range(3):
    try:
        with db.transaction() as tx:
            tx.execute("MATCH (n:Account {id: 'a'}) SET n.balance = n.balance - 10")
        break
    except TransactionConflict:
        continue  # nothing was written; run it again
```

- `tx.execute(cypher, params=None)` takes and returns what `query_write` does.
- `tx.commit()` returns the commit number; the transaction is finished afterwards, whether or not it
  raised. Using a finished transaction raises `TransactionClosed`. A transaction dropped unfinished is
  a rollback.
- A `read_only=True` handle cannot begin one (`ReadOnly`). A transaction on a `scoped()` handle reads
  through the scope and cannot write (`ReadOnly`); see [Scoped transactions are read-only](#scoped-transactions-are-read-only).
- `close()` raises while a transaction is open on the store.
- Many transactions may run at once from different threads; one transaction used from two threads is
  serialised. An embedded transaction has no timeout.

## HTTP

```sh no-run
TOKEN=your-token
TX=$(curl -s -X POST localhost:8080/tx/begin -H "Authorization: Bearer $TOKEN" | jq -r .tx_id)

curl -s -X POST localhost:8080/tx/$TX/run -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"cypher": "CREATE (n:Person {id: $id})", "params": {"id": "p1"}}'

curl -s -X POST localhost:8080/tx/$TX/run -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"cypher": "MATCH (n:Person {id: $id}) RETURN n.id AS id", "params": {"id": "p1"}}'

curl -s -X POST localhost:8080/tx/$TX/commit -H "Authorization: Bearer $TOKEN"
# {"bookmark": 42, "bookmark_text": "mushroomdb:commit:42"}
```

`POST /tx/{id}/rollback` (or `DELETE /tx/{id}`) discards the transaction. A transaction belongs to the
credential that began it; anyone else gets `404` for its id. A server with no token and no keys has a
single owner, so every caller can reach every transaction whose id it knows. The routes, their bodies and every status
code are in [HTTP API: Transactions](http.md#transactions).

The server holds open transactions in memory and bounds them:

| Limit | Flag | Variable | Default |
|---|---|---|---|
| Idle time before rollback (every `run` restarts it) | `--tx-timeout-secs` | `MUSHROOMDB_TX_TIMEOUT_SECS` | 30 s |
| Lifetime from `begin`, however busy | `--tx-max-lifetime-secs` | `MUSHROOMDB_TX_MAX_LIFETIME_SECS` | 300 s |
| Transactions open at once (`503` with `Retry-After: 1` beyond it) | `--tx-max` | `MUSHROOMDB_TX_MAX` | 100 |

A flag wins over its variable. These limits apply to HTTP transactions; Bolt has its own idle limit
(see [Bolt](#bolt)). An expired transaction answers its owner `410` for a while, then `404`. The
message says which limit ended it: `transaction expired: idle too long, rolled back`, or
`transaction expired: open longer than its maximum lifetime, rolled back`. When the server is told
to stop, it rolls back every open transaction at the start of the drain and answers
`POST /tx/begin` with `503`.

## TypeScript

```ts
import { MushroomClient, TransactionConflictError } from 'mushroomdb-client';

const client = new MushroomClient('http://127.0.0.1:8080', { token: 'your-token' });

// Commits when the callback resolves; rolls back and rethrows when it throws.
const count = await client.transaction(async (tx) => {
  await tx.execute('CREATE (:Person {id: $id})', { id: 'p1' });
  const seen = await tx.execute('MATCH (p:Person {id: $id}) RETURN p.id AS id', { id: 'p1' });
  return seen.rows.length; // 1: it sees its own write
});

// The explicit form.
const tx = await client.beginTransaction();
try {
  await tx.execute('CREATE (:Person {id: $id})', { id: 'p2' });
} catch (err) {
  await tx.rollback();
  throw err;
}
try {
  const commitNumber = await tx.commit();
} catch (err) {
  if (err instanceof TransactionConflictError) {
    // a node it read or wrote changed after begin: nothing was written,
    // so the whole unit of work can run again
  }
  // A ConstraintViolationError (a unique constraint refused the writes) is not
  // worth retrying: the same writes are refused again.
  throw err;
}
```

A commit that reaches the engine ends the transaction on the server whatever its outcome, and the
handle follows: after a failed commit the client asks the server (`GET /tx/{id}`) whether it still
holds the transaction, and the handle stays usable only if it does. If `commit()` fails at the network
level, or the server is still running the commit, the outcome is unknown and the handle refuses
further use: check the store before retrying.

The client does not retry by itself. See [TypeScript client: Transactions](typescript.md#transactions)
for every error it throws.

## Bolt

A driver's explicit transaction (`BEGIN`, statements, `COMMIT` or `ROLLBACK`) and its managed read and
write transactions run on the same engine. A conflict at `COMMIT` is the transient error
`DeadlockDetected`, which drivers' managed transactions retry from the start; a constraint refusal is
`ConstraintValidationFailed`, a client error, which is not retried.

Differences from HTTP:

- A statement that fails ends the transaction, as the Bolt state machine requires. The client sends
  `RESET` and begins again.
- The idle limit is `--bolt-tx-timeout-secs` (default 30 s). There is no lifetime limit and no separate
  cap on open transactions: a connection holds at most one, and connections are capped by
  `--bolt-max-connections` (default 256).
- `BEGIN` in mode `"r"` opens a read transaction that refuses writes.
- On shutdown each connection is closed at its next idle point and its open transaction is rolled back.

See [Bolt protocol: Transactions and bookmarks](bolt.md#transactions-and-bookmarks) for the `BEGIN`
fields, bookmark waits and the state machine.

## Bookmarks

A commit returns its commit number. HTTP returns it twice: as the integer `bookmark` and as the text
`mushroomdb:commit:<n>` in `bookmark_text`. Bolt returns only the text form. Only Bolt takes a
bookmark back: pass `bookmark_text`, or a bookmark a Bolt commit returned, in a later Bolt `BEGIN` or
`RUN`. Any other form, the bare integer included, is refused as an unrecognised bookmark. HTTP takes
no bookmark. A Bolt `BEGIN` or `RUN` given a bookmark waits until the store has reached that commit
(5 s by default, `--bookmark-timeout-secs`), so a read on another session sees the write.

## Scoped transactions are read-only

A transaction begun with a role token or a `read`-tier key (HTTP and Bolt), or on a Python `scoped()`
handle, reads through its role (for a `read` key, the role the key names) on the snapshot it began
on, and **cannot write**. This holds even for a
role whose write scope lets it write through `POST /query` or a Bolt auto-commit `RUN`: the
transaction engine has no role-scoped commit yet. Its commit writes nothing and returns the commit it
read. Role-scoped writes inside a transaction are planned for 0.11 (ledger row 140); until then, write
with a role through auto-commit statements. The full token, `admin` keys and `write` keys run
transactions without this limit.

## Limits

- A transaction is held in the memory of one process: the server for HTTP and Bolt, the embedding
  process for Rust and Python. There are no transactions that span processes.
- An open transaction holds its snapshot and every write it buffered (up to a request body, 64 MiB,
  per HTTP statement). Keep transactions short; the server's limits bound how many are open and for how
  long, not how large one is.
- Schema statements cannot run inside a transaction.
- Isolation is snapshot isolation, not serializable: see
  [What snapshot isolation does not prevent](#what-snapshot-isolation-does-not-prevent).

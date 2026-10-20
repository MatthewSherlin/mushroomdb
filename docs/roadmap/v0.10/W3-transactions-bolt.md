# W3 — Interactive Transactions and the Bolt Wire Protocol

> Part of the v0.10 spec — see [`../v0.10-spec.md`](../v0.10-spec.md). Where this document and the master spec disagree, the master spec wins; its §4 lists every override.

**Workstream prefix:** TX-  
**Cross-workstream dependencies:** W1 (Cypher `CALL` procedures, EXPLAIN surface), W2 (Date/Binary types in PackStream; WAL version-bump coordination; constraint checking at commit), W4 (TLS cert/key shared with Bolt listener)

---

## 1. Outcome

After 0.10 a general developer can:

- Open an interactive transaction (`BEGIN`), issue multiple reads and writes
  (reads see their own writes), then `COMMIT` or `ROLLBACK` — all with
  standard atomicity and crash safety.
- Connect to mushroomdb using the official driver for their language (Python,
  JavaScript, Java, Go, .NET) without any code change — just swap the URI to
  `bolt://host:7687`.
- Use any visualization or BI tool that speaks Bolt (DBeaver, DataGrip, etc.)
  and have it connect, browse labels and relationship types, and run queries.
- Trust that Bolt connections obey the same RBAC masks and scoped visibility
  that the HTTP API enforces — nothing is bypassed.
- Use causal bookmarks to enforce read-your-own-writes across connections.

---

## 2. Current State

### Transactions

There are no interactive transactions.  The single mutation primitive is
`write_batch` (`crates/core-api/src/db.rs:6831`) which assembles all ops
client-side, submits them as one `WalRecord::Batch` frame (discriminant 8,
`crates/core-storage/src/wal.rs:44`), and fsyncs.  The frame is atomic:
either every record in the batch is applied on replay, or none is.

`commit_seq` (`db.rs:1589`) is a `u64` that increments by one per WAL frame
(including `Batch` frames, `RebuildRule` frames, and single-record frames).
It seeds from the last persisted `last_change` entry on open (`db.rs:2642`).
This is already the correct primitive for Bolt bookmarks.

Limits documented in `docs/site/limits.md`: "Multi-statement BEGIN/COMMIT is
not supported"; "write_batch commits all ops in one WAL frame (all-or-nothing
on crash replay) but is NOT isolated."

### Concurrency / lock-free reader

The lock-free MVCC epoch reader exists and ships (`crates/core-api/src/reader.rs:1`).
It snapshots at every `FOLD_EVERY_K = 64` commits plus a bounded delta tail,
providing snapshot isolation without holding the write lock.

Four HTTP routes already use it: `http.rs:1261`, `1728`, `1799`, `1878`.
All other read routes (`http.rs:770`, `1251`, `1306`, `1332`, `1380`, `1385`,
`1412`, `1430`, `1706`, `1754`, `1777`, `1818`, `1839`, `1903`, `1924`, `2469`,
`2505`, `2537`, `2556`, `2593`) still call `db.read()` (the `RwLock` write-
exclusive guard), stalling during the ~4 ms WAL fsync per write.

The concurrency decision (`docs/concurrency-decision.md:78–88`) ruled against
`parking_lot` swap; the approved fix is `tokio::task::spawn_blocking` around
remaining `db.read()` call sites.

### Bolt

Zero Bolt implementation exists.  The port 7687 appears only in competitor
benchmark notes (`benchmarks/results/head-to-head-10k.md:29`), not in server
code.  No PackStream encoder/decoder exists in the codebase.

---

## 3. Design

### 3.1 Engine Transactions

#### Isolation model

- **Writes serialized**: the single-writer WAL architecture is unchanged.
  One write applies at a time; concurrent BEGIN'd transactions share the
  serialization queue.
- **Readers: snapshot isolation**: every `db.reader()` call returns a frozen
  `ReaderSnapshot` consistent with the state at that instant.  A Bolt
  connection in an open transaction uses one frozen `ReaderSnapshot` acquired
  at `BEGIN` plus an in-memory `TxBuffer` (uncommitted ops) to give
  read-your-own-writes semantics (§3.1 "reads see own writes").
- **Write conflicts are detected (master §4.2 overrides this paragraph).** A
  transaction records every node and edge key it read or wrote with the
  `last_change` commit each had in `base_snap`; `COMMIT` goes through
  `write_batch_cas` with a `NodeUnchangedSince` precondition per key, so any
  change since `BEGIN` fails the commit as a transient conflict that drivers
  retry. Isolation is snapshot isolation: no lost updates; write skew through a
  label scan is possible and documented.

#### Transaction lifetime and idle timeout

A transaction lives in the Bolt connection state machine.  It holds:

```
TxBuffer {
    ops: Vec<BatchOp>,          // accumulated mutations
    base_snap: ReaderSnapshot,  // frozen at BEGIN
    opened_at: Instant,
    tx_id: u64,                 // monotonic, per-server
}
```

The buffer holds NO write lock.  The RwLock is acquired only at COMMIT when
the buffer is submitted via `submit_batch` (or `write_batch_cas` if CAS
preconditions are used).

**Idle timeout**: configurable `--tx-timeout-secs` (default 30 s, env
`MUSHROOMDB_TX_TIMEOUT_SECS`).  The Bolt connection actor checks elapsed time
on every message receipt and on a background ticker.  An expired transaction
is rolled back (buffer dropped); the client receives `…ClientError.Transaction.TransactionTimedOut`.

#### Reads inside an open transaction

A `RUN` inside `BEGIN`/`COMMIT` executes as follows:

1. Write queries: ops accumulated into `TxBuffer.ops`; the query returns
   the locally-applied result by playing ops over `TxBuffer.base_snap`
   (a lightweight in-memory projection — node/edge inserts visible,
   properties visible; rule-derived edges NOT visible until COMMIT, consistent
   with rules-fire-at-commit below).
2. Read queries: executed against `base_snap` plus the local op log, giving
   read-your-own-writes.

#### Rules fire at commit only

Rule engine (incremental edge maintenance) runs inside `write_batch` on the
drain thread under the write guard.  Interactive tx accumulates raw ops;
rule firing happens when those ops land in `submit_batch` at COMMIT.  Inside
an open transaction, rule-derived edges from the transaction's own writes are
NOT queryable.  This keeps the model simple, avoids in-memory rule
speculation, and is the correct semantic: uncommitted data has no derived
edges.

**Document this limit in the user-facing transaction docs.**

#### Constraint checking (coordinate with W2)

W2 adds unique constraints.  Constraint checking must run at COMMIT time
(inside `submit_batch`) against the full committed state — not against the
local buffer — so two concurrent transactions inserting the same unique value
produce exactly one success.  The constraint check runs under the write guard
with no TOCTOU window (same seam as `write_batch_cas`).

W2 must expose a `check_constraints(ops: &[BatchOp], db: &GraphDb) -> Result<()>`
function called by the drain thread.  W3 calls it transparently.

#### WAL representation

No new WAL record variants are needed for transactions.  A committed
transaction is a single `WalRecord::Batch` frame (same as `write_batch`
today).  A rolled-back transaction produces no WAL entry — the buffer is
dropped.  Crash in the middle of a tx: buffer never reached WAL; nothing
visible after restart.

**WAL format bump coordination**: W2 owns the format version bump (since W2
is adding constraint records).  W3 adds no new discriminants.  W3 must ship
after or alongside W2's format bump.  If W3 ships first (without W2), no
format bump is needed.

#### Auto-snapshot interaction

Auto-snapshot triggers are unaffected.  A snapshot captures the state at the
most recent committed frame; an open transaction's buffer is not in the
snapshot.  If a snapshot fires while a transaction is open, the snapshot and
the transaction are independent.  Commit after snapshot replay: the tx's
`base_snap` may be stale by up to `FOLD_EVERY_K` commits; the CAS path
handles the conflict.

#### Rust API surface

```rust
// crates/core-api/src/lib.rs — new public items

/// An open interactive transaction.
pub struct Transaction<'db> {
    buffer: Vec<BatchOp>,
    base_snap: ReaderSnapshot,
    opened_at: std::time::Instant,
}

impl SharedDb {
    /// Begin an interactive transaction.
    /// Acquires the read lock only to clone Arc handles for base_snap.
    pub fn begin_transaction(&self) -> Transaction<'_>;
}

impl Transaction<'_> {
    /// Execute a Cypher write, accumulating ops into the buffer.
    /// Reads return results from base_snap + buffer projection.
    pub fn execute(&mut self, cypher: &str, params: Params) -> Result<ResultSet>;

    /// Commit: submit buffer as a single Batch; returns commit_seq on success.
    pub fn commit(self) -> Result<u64>;

    /// Rollback: drop the buffer; no WAL entry.
    pub fn rollback(self);
}
```

#### Python API surface

```python
# context-manager style (preferred)
with db.transaction() as tx:
    tx.execute("CREATE (n:User {id: $id})", {"id": "u1"})
    tx.execute("CREATE (n:Post {id: $id})", {"id": "p1"})
    # commit on __exit__ if no exception; rollback on exception

# explicit style
tx = db.begin_transaction()
try:
    tx.execute(...)
    bookmark = tx.commit()  # returns int commit_seq
except:
    tx.rollback()
```

#### TypeScript API surface

```typescript
// callback style
const bookmark = await db.transaction(async (tx) => {
    await tx.execute("CREATE (n:User {id: $id})", { id: "u1" });
    return tx.commit(); // returns commit_seq
});

// explicit style
const tx = db.beginTransaction();
try {
    await tx.execute(...);
    const bookmark = await tx.commit();
} catch {
    await tx.rollback();
}
```

#### HTTP API

Stateless HTTP gets explicit transaction endpoints with a server-assigned
transaction id and TTL:

```
POST   /tx/begin                         → { tx_id, expires_at }
POST   /tx/{tx_id}/run                   ← { cypher, params }; → rows
POST   /tx/{tx_id}/commit                → { bookmark: commit_seq }
POST   /tx/{tx_id}/rollback              → 200
DELETE /tx/{tx_id}                       → same as rollback (alias)
GET    /tx/{tx_id}                       → { tx_id, expires_at, op_count }
```

`tx_id` is a random 128-bit hex string (UUID v4).  Server-side state is
an in-memory `HashMap<TxId, TxBuffer>` with a background reaper that
rolls back expired entries.  This state is not persisted; a server restart
rolls back all open HTTP transactions (correct — they were uncommitted).

MCP is unchanged.  MCP tools execute single auto-commit statements (same
as today).

---

### 3.2 Concurrency — Moving Read Routes to the Lock-Free Reader

**Scope**: Every HTTP read route that calls `db.read()` must be moved to
`db.reader()` (lock-free snapshot).  Write routes remain on `db.write()`.

Routes requiring conversion (verified against `http.rs`):

| Line | Route/handler | Action |
|------|---------------|--------|
| 770  | health/stats handler | → `db.reader()` |
| 1251 | query (`as_of` path) | → `db.read().query_at(…)` wraps in `spawn_blocking` (time-travel read requires full lock) |
| 1306 | `/suggest` | → `db.reader()` |
| 1332 | `/stats` | → `db.reader()` |
| 1380–1385 | `/query` (live and as-of) | live → `db.reader()`; as-of → `spawn_blocking(db.read())` |
| 1412, 1430 | algo routes | → `db.reader()` |
| 1706 | node REST API | → `db.reader()` |
| 1754, 1777 | neighborhood/edges | → `db.reader()` |
| 1818, 1839 | explain | → `db.reader()` |
| 1903, 1924 | search | → `db.reader()` |
| 2469, 2505, 2537, 2556, 2593 | misc read routes | → `db.reader()` |

Note on row 97 (deferred): `reader().node_edges` already tracks derived edges
via `derived_inserts`/`derived_deletes` in `CommitDelta` (`reader.rs:43–46`),
but defect row 114 says it reports `derived: false`.  Fix row 114 as part
of this work (expose the `derived` flag from the delta tail).

**Reader freshness guarantee**: a `ReaderSnapshot` sees all commits up to
and including the fold epoch, plus at most `FOLD_EVERY_K − 1 = 63` subsequent
commits via the delta tail.  Maximum staleness under sustained writes at
N writes/s: `63 / N` seconds.  At 100 writes/s, staleness ≤ 630 ms.
Document this as the reader freshness bound.

**Concurrency benchmark gate**:

```
Bench: readers_p50_p99_under_sustained_writes
  - 16 concurrent HTTP-equivalent read calls (neighborhood, depth 1, 10k graph)
  - 1 background writer: 50 set_prop calls/s (continuous)
  - Measure: p50 and p99 read latency before and after migration
Gate: p99 read latency ≤ 2 ms at 10k nodes, 50 writes/s (vs current ~4 ms blocked)
Compare vs v0.9.x baseline on same hardware
```

---

### 3.3 Bolt Wire Protocol

#### Version target

Official drivers in 2026 (5.x and 6.x series) negotiate Bolt **4.4 through 5.x**.
A third-party Elixir driver lists 5.0–5.8 and 6.0.
The minimum Bolt version for all current official Python/JS/Java/Go/.NET
drivers (5.x series) to connect is **Bolt 4.4**.

mushroomdb implements: **Bolt 4.4** (primary) and **Bolt 5.0** (upgrade path).
The 4-slot handshake offers `[5.0, 4.4, 0, 0]` (highest first, per spec).

Bolt 5.1+ LOGON/LOGOFF auth refresh and 5.4 telemetry are deferred to 0.11.
Bolt 6.0 is deferred.

Bolt ROUTE (4.3+) is implemented as a single-server routing table (enough
for routing-scheme URIs that issue ROUTE on connect).

#### Crate layout

New crate: `crates/bolt`.  Depends on `core-api` (same as `server`).
`server` gains a `--bolt-port` flag that starts the Bolt TCP listener in a
separate tokio task.

```
crates/bolt/
  src/
    lib.rs
    packstream.rs      # PackStream v1 encoder/decoder
    framing.rs         # chunked framing (2-byte length headers, 0x0000 EOMsg)
    handshake.rs       # magic 0x6060B017 + 4-slot version negotiation
    messages/
      hello.rs         # HELLO, LOGON (4.4/5.0)
      run.rs           # RUN, PULL, DISCARD
      tx.rs            # BEGIN, COMMIT, ROLLBACK
      reset.rs         # RESET, GOODBYE
      route.rs         # ROUTE (single-server table)
    connection.rs      # per-connection actor (tokio task)
    server.rs          # TcpListener loop, TLS upgrade
    procedures.rs      # built-in CALL procedures
    error.rs           # Bolt error codes → engine errors
```

#### PackStream encoding

PackStream v1 is hand-written (no production-ready Rust crate for Bolt 5.x;
see §9 New Dependencies).

| mushroomdb `Value` | PackStream type |
|---|---|
| `Int(i64)` | Integer |
| `Float(f64)` | Float64 |
| `Str(String)` | String |
| `Bool(bool)` | Boolean |
| `Null` | Null |
| `List(Vec<Value>)` | List |
| `Map(BTreeMap<String,Value>)` | Map (string keys) |
| W2 `Date` | Struct tag `0x44` (Date) |
| W2 `DateTime` | Struct tag `0x49` (DateTime with tz offset) |
| W2 `Duration` | Struct tag `0x45` (Duration) |
| W2 `Binary` / byte array | Bytes |
| Graph Node | Struct tag `0x4E` (Node): `{element_id, labels, properties}` |
| Graph Relationship | Struct tag `0x52` (Relationship): `{element_id, start_element_id, end_element_id, type, properties}` |
| Path | Struct tag `0x50` (Path) |

`element_id` for nodes and relationships is the internal `u32` formatted as
a decimal string (stable for a connection lifetime; not guaranteed stable
across server restarts — document this).

W2 temporal/binary types must be finalized before PackStream encoding is
complete.  W3 ships the stubs and wires them in once W2's `Value` variants
exist.

#### Handshake

```
Client → Server: 0x60 0x60 0xB0 0x17   (magic)
Client → Server: version[4]             (4 × u32 big-endian; [5,0,0,0],[4,4,0,0],[0,0,0,0],[0,0,0,0])
Server → Client: agreed_version         (4 bytes [pad, range, minor, major]: 0x00000005 for 5.0, 0x00000404 for 4.4, 0x00000000 if none)
```

Required (master §4.3): skip or implement the manifest indicator slot `0x000001FF`, and match client
slots by range — a slot `[0, r, m, M]` accepts any server version `M.v` with `m − r ≤ v ≤ m`.

#### Auth — HELLO / LOGON mapping

Bolt `scheme: "none"` → anonymous; allowed if `--allow-unauthenticated` is set.  
Bolt `scheme: "basic"`, `principal` / `credentials` → `credentials` is
treated as a mushroomdb bearer token (same as `Authorization: Bearer` HTTP
header).  The `principal` field is logged but not checked against a username
table (mushroomdb has no username table; tokens carry the role).  
Bolt `scheme: "bearer"`, `credentials` → same as basic credentials.

On HELLO success, the connection acquires a `RoleDef` from the token and
holds it for the connection lifetime.  All subsequent `RUN` / `PULL` calls
use that role's mask.  Scoped visibility is enforced identically to the HTTP
layer — Bolt cannot bypass RBAC.

#### RUN / PULL / DISCARD streaming with backpressure

`RUN` dispatches to `db.reader()` (auto-commit reads) or `tx.execute()`
(inside BEGIN/COMMIT).  Write queries in auto-commit mode submit a single-op
`Batch` immediately.

Results are streamed as `RECORD` messages.  `PULL n=-1` streams all; `PULL n=k`
streams k rows, leaving the cursor open (server holds a `ResultSet` iterator).
`DISCARD` drops the cursor.  Cursor state is per-connection, not shared.

Backpressure: the tokio task pauses `PULL` dispatch until the TCP send buffer
drains (natural backpressure from `AsyncWrite`).  No explicit flow-control
token is needed at the application layer for 0.10.

#### BEGIN / COMMIT / ROLLBACK mapping

`BEGIN` → `SharedDb::begin_transaction()`, stores `TxBuffer` in connection state.  
`COMMIT` → `tx.commit()` → `submit_batch(ops)` → returns `commit_seq` as bookmark.  
`ROLLBACK` → `tx.rollback()`, buffer dropped.  

Bookmark field: `{ "bookmark": "mushroomdb:commit:<commit_seq>" }`.  A
`PULL` after `BEGIN` with a bookmark list waits until `db.reader()` reflects
a `commit_seq` ≥ the maximum of the requested bookmarks (poll with short
sleep; see §3.3 causal consistency below).

#### Causal consistency / bookmarks

Bookmark format: `"mushroomdb:commit:<u64>"`.  The `commit_seq` is the
value returned by `Transaction::commit()`.  

A connection that receives a bookmark in `BEGIN` (or `RUN` in auto-commit)
waits until `db.reader().commit_seq >= bookmark_seq`.  Wait loop: check
every 10 ms, timeout after `--bookmark-timeout-secs` (default 5 s).

#### RESET

Rolls back any open transaction, closes any open cursor, returns connection
to `READY` state.  Does not disconnect.

#### GOODBYE

Closes the connection cleanly.  Rolls back any open transaction.

#### ROUTE (single-server)

```
ROUTE {} [] "<default-db>" →
{
  "rt": {
    "ttl": 300,
    "db": "<default-db>",
    "servers": [
      {"addresses": ["host:7687"], "role": "WRITE"},
      {"addresses": ["host:7687"], "role": "READ"},
      {"addresses": ["host:7687"], "role": "ROUTE"}
    ]
  }
}
```

This makes routing-scheme URIs (`<routing-scheme>://host:7687`) work.  The TTL of 300 s
means drivers refresh the routing table every 5 minutes (no overhead in
practice for a single-server setup).

#### Database selection

Bolt 4.4+ carries a `db` field on `BEGIN` and `RUN`.  mushroomdb accepts
`null`, `""`, the drivers' default database name, and the configured store name (from `--db-name`,
default `"mushroomdb"`).  Any other name returns:
`…ClientError.Database.DatabaseNotFound`.

#### TLS

The Bolt listener reuses the same rustls `ServerConfig` as the HTTPS server
(W4 owns the cert/key config).  `--bolt-tls` boolean flag enables TLS on
the Bolt port.  Without TLS, the listener accepts plaintext (drivers support
both; the TLS routing scheme forces TLS, `bolt://` allows plaintext).

#### Error code mapping

| mushroomdb error | Bolt `code` | Bolt `classification` |
|---|---|---|
| Auth failure | `…ClientError.Security.Unauthorized` | ClientError |
| Role-masked access denied | `…ClientError.Security.Forbidden` | ClientError |
| Constraint violation (W2) | `…ClientError.Schema.ConstraintValidationFailed` | ClientError |
| Cypher syntax error | `…ClientError.Statement.SyntaxError` | ClientError |
| Tx timeout / expired | `…ClientError.Transaction.TransactionTimedOut` | ClientError |
| Database not found | `…ClientError.Database.DatabaseNotFound` | ClientError |
| Transient write conflict | `…TransientError.Transaction.DeadlockDetected` | TransientError |
| IO / fsync failure | `…TransientError.General.DatabaseUnavailable` | TransientError |
| Internal / panic | `…DatabaseError.General.UnknownError` | DatabaseError |

Drivers retry automatically on `TransientError` classification.  The write-
conflict case (`CasConflict` from `write_batch_cas`) maps to `DeadlockDetected`
(the driver retry class for optimistic conflicts).

#### Listening port and config

Default port: **7687** (same as incumbents; configurable).

```
CLI:  mushroomdb serve --bolt-port 7687 --bolt-tls [--allow-unauthenticated]
Env:  MUSHROOMDB_BOLT_PORT, MUSHROOMDB_BOLT_TLS, MUSHROOMDB_ALLOW_UNAUTH
```

Server-config JSON:
```json
{
  "bolt_port": 7687,
  "bolt_tls": false,
  "tx_timeout_secs": 30,
  "bolt_db_name": "mushroomdb"
}
```

#### Built-in CALL procedures (minimal set for tool compatibility)

BI tools and visualization tools issue these procedures on connect:

| Procedure | Returns | Notes |
|---|---|---|
| `db.labels()` | `{label: String}` rows | One row per node label in the store |
| `db.relationshipTypes()` | `{relationshipType: String}` | One row per edge type |
| `db.propertyKeys()` | `{propertyKey: String}` | All interned property field names |
| `db.indexes()` | `{name, type, labelsOrTypes, properties, state}` | All declared indexes |
| `db.constraints()` | `{name, type, labelsOrTypes, properties}` | All declared constraints (W2) |
| `dbms.components()` | `{name:"mushroomdb", versions:[VERSION], edition:"community"}` | Version handshake |
| `db.schema.visualization()` | `{nodes, relationships}` | Schema graph (labels + rel types) |
| `CALL db.ping()` | — | Returns nothing; used as a keepalive |

These are implemented in `crates/bolt/src/procedures.rs`.  They are also
reachable via the HTTP `/query` endpoint as `CALL db.labels()` once W1 adds
the `CALL` keyword.  W3 ships the Bolt-side dispatcher; W1 wires them into
the Cypher grammar.

#### Scoped visibility over Bolt

Each Bolt connection carries one `RoleDef` acquired at HELLO.  Every
`db.reader()` call on that connection passes the role's mask through
`ReaderSnapshot::query_masked`.  A scoped connection cannot see nodes or
edges outside its mask — identical to HTTP enforcement.  Bolt does not expose
any admin API (snapshot, ingest, schema apply) — those remain HTTP-only.

---

## 4. Interaction with Differentiators

**Rules engine**: rules fire at commit only (§3.1).  Rule-derived edges from
uncommitted tx writes are not visible inside the tx.  ROLLBACK discards all
buffered writes; no rule firing occurs.  Rules are correct across one-batch
vs interactive-tx commits because both pass through `write_batch` on the
drain thread.

**Time travel / as-of**: `open_at` reads are HTTP-only; no Bolt equivalent.
The `as_of` parameter is not surfaced in Bolt (Bolt has no equivalent
mechanism).  Time travel is unaffected by the tx design.

**Scoped visibility**: enforced per-connection at Bolt auth (§3.3).  The
`ReaderSnapshot` mask is frozen at HELLO; label/role changes mid-connection
are not re-applied until reconnect (same as HTTP session behavior).

**Snapshots + WAL replay**: a committed tx is a single `Batch` frame on the
WAL.  Replay is identical to replay of a `write_batch` result.  Snapshot
captures state at the latest committed frame; open tx buffers are not in the
snapshot.  The "behavior must be identical across: one batch vs separate
commits; reopened vs never-closed handle; snapshotting vs never-snapshotting;
live vs WAL replay vs `open_at`" invariant is preserved because the buffer
is only materialized as a `Batch` at commit.

**Full-text / vector indexes**: read via `db.reader()` (lock-free).  The
lock-free reader already has access to the frozen full-text index (stored in
snapshot since 0.8).  No interaction with tx design.

**MCP tools**: unchanged.  MCP issues auto-commit single statements only.
MCP tools do not expose BEGIN/COMMIT.

---

## 5. Compatibility and Migration

### Store format

No new WAL discriminants (tx uses existing `Batch` frame).  No snapshot
section changes.  **A 0.9.x binary can open a 0.10 store.**

### API breaks

| Surface | Break | Migration |
|---|---|---|
| Rust `SharedDb` | `begin_transaction()` is new additive | None |
| Python bindings | `db.transaction()` is new additive | None |
| TypeScript client | `db.beginTransaction()` is new additive | None |
| HTTP API | `/tx/*` routes are new additive | None |
| HTTP API | `/query` gains `CALL` keyword (W1) | Queries using `call` as a variable name break (very rare) |
| MCP | Unchanged | None |

No breaking changes to existing APIs.  Bolt is entirely new; no migration.

### Downgrade

A 0.9.x binary can open a 0.10 store (no format change).  Bolt port is
unused by 0.9.x.

---

## 6. Verification

### Unit tests (crates/bolt)

- PackStream: round-trip all `Value` variants including graph structs; fuzz
  with random bytes (expect clean `Err`, no panic).
- Framing: chunk boundary at odd offsets; 0x0000 end marker; partial read.
- Handshake: version negotiation for 5.0 and 4.4; unsupported version →
  `0x00000000`.
- Auth: valid bearer token → READY; invalid → `Unauthorized`; expired tx
  → `TransactionTimedOut`.
- RESET: rolls back open tx; clears cursor; connection returns to READY.

### Integration tests (crates/bolt/tests/)

- Full roundtrip: connect → `HELLO` → `RUN` read → `PULL` all → `GOODBYE`.
- Auto-commit write: `RUN` CREATE → `PULL` → verify node visible via reader.
- Interactive tx commit: `BEGIN` → `RUN` CREATE → `RUN` read (sees own write) → `COMMIT` → verify.
- Interactive tx rollback: `BEGIN` → `RUN` CREATE → `ROLLBACK` → verify node absent.
- Tx idle timeout: open tx, sleep > timeout, send COMMIT → `TransactionTimedOut`.
- Causal bookmark: commit tx on connection A, `BEGIN` with bookmark on connection B → waits, then reads committed node.
- Scoped visibility: role-masked connection cannot see masked labels via Bolt.
- ROUTE: driver uses routing URI, connects successfully.
- `CALL db.labels()`: returns all labels.
- Error retry: server returns `TransientError.Transaction.DeadlockDetected`; Python driver retries and succeeds.

### Differential probes (W3-specific invariant)

- One `write_batch` vs equivalent interactive tx: result of `db.reader().query()` after commit is identical.
- Crash mid-tx (sim-harness): crash any point before COMMIT WAL fsync → node absent after restart.
- Crash at COMMIT (sim-harness): crash after WAL fsync → node present after restart.
- Re-open after tx: reopen store, query committed tx result → identical.
- Snapshot + tx: snapshot fires during open tx; commit tx; verify snapshot and post-commit state consistent.

### Driver compatibility test matrix (CI)

| Driver | Version | Gate | CI feasibility |
|---|---|---|---|
| Official Python driver | 5.x latest | **Required gate** | GitHub Actions (pip install) |
| Official JavaScript driver | 5.x latest | **Required gate** | GitHub Actions (npm install) |
| Official Java driver | 5.x latest | Best effort | Requires JDK in CI image |
| Official Go driver | 5.x latest | Best effort | Requires Go in CI image |
| Official .NET driver | 5.x latest | Best effort | Requires .NET SDK in CI image |

CI scenario list for each driver (gate drivers must pass all; best-effort must pass ≥ connect):

1. Connect and auth (valid token)
2. Auth failure (invalid token) → driver receives `Unauthorized`
3. Auto-commit read: `MATCH (n) RETURN count(n)` → integer result
4. Auto-commit write: CREATE node → verify via subsequent read
5. Explicit tx commit: BEGIN → CREATE → COMMIT → verify
6. Explicit tx rollback: BEGIN → CREATE → ROLLBACK → verify absent
7. Streaming 100k rows: `MATCH (n) RETURN n` on 100k-node graph → all rows received
8. All `Value` types round-trip: Int, Float, String, Bool, Null, List, Map, Date, DateTime, Duration, Bytes (once W2 ships temporal types)
9. Error retry: inject `TransientError` → driver retries → succeeds
10. `CALL db.labels()` → non-empty list
11. Routing-scheme URI → connect via ROUTE → queries succeed
12. Bookmark causal consistency: commit on connection A, read with bookmark on B → sees committed data

### Concurrency benchmark gate

```
Bench: readers_p50_p99_under_sustained_writes (in crates/core-bench)
Config: 16 concurrent reader goroutines (neighborhood depth-1, 10k graph),
        1 writer goroutine (50 set_prop/s continuous)
Gate: p50 ≤ 500 µs, p99 ≤ 2 ms
Baseline: run same bench against v0.9.x binary; report ratio
Fail criteria: p99 regresses vs v0.9.x by > 10%
```

### Performance gates

- `tx_commit_overhead`: time from `Transaction::commit()` call to WAL fsync complete.
  Gate: ≤ 110% of equivalent `write_batch` call (same ops, same graph size).
  Rationale: tx adds only one extra `Vec` clone at submit; overhead should be negligible.
- Bolt connection setup (HELLO → READY): ≤ 5 ms p99 under 16 concurrent connections.

### Docs checks

- `/docs/site/transactions.md` created: documents isolation model, read-your-own-writes,
  rule-firing behavior, idle timeout, HTTP tx endpoints, Python/TypeScript API.
- `/docs/site/bolt.md` created: Bolt port, TLS, auth mapping, value types,
  element_id stability note, driver compatibility matrix, known limitations.
- `limits.md` updated: remove "Multi-statement BEGIN/COMMIT is not supported."

---

## 7. Build Units

| Unit | Description | Size | Files / crates touched | Depends on | Conflict hotspots |
|------|-------------|------|------------------------|------------|-------------------|
| **TX-1** | `Transaction` type + `begin_transaction` in `SharedDb`; optimistic buffer; `commit()`/`rollback()` Rust API | M | `core-api/src/db.rs`, `core-api/src/lib.rs` (new `transaction.rs`) | — | `db.rs` (hot file) |
| **TX-2** | Python `db.transaction()` context-manager binding; **also ledger row 65**: release the GIL on Python write methods, making `upsert_node`'s read-then-write atomic first (master §4.5) | M | `bindings/python/src/lib.rs` | TX-1 | PyO3 bindings |
| **TX-3** | TypeScript `db.beginTransaction()` / callback API | S | `clients/typescript/src/` | TX-1 | TS client |
| **TX-4** | HTTP `/tx/*` endpoints + in-memory tx state map + background reaper | M | `crates/server/src/http.rs` (new `tx.rs`) | TX-1 | `http.rs` (hot file) |
| **TX-5** | Move all read-only HTTP routes from `db.read()` to `db.reader()` + `spawn_blocking` for as-of reads | M | `crates/server/src/http.rs` | — | `http.rs` (hot file) — serialize with TX-4 |
| **TX-6** | Fix defect row 114: expose `derived: true` flag via `ReaderSnapshot` node-edges delta tail | S | `core-api/src/reader.rs`, `core-api/src/db.rs` | — | `reader.rs` |
| **TX-7** | Concurrency benchmark: `readers_p50_p99_under_sustained_writes` + gate | S | `crates/core-bench/` | TX-5, TX-6 | bench crate |
| **TX-8** | PackStream v1 encoder + decoder (all Value variants incl. graph structs; W2 temporal/binary stubs) | M | `crates/bolt/src/packstream.rs` (new crate) | — | new crate |
| **TX-9** | **First:** read the five current official drivers' handshake code and release notes, record each one's offered and minimum Bolt versions in `docs/site/bolt.md`, and fix the server's version set to the smallest one all five negotiate (master §4.3) — adding 5.1 `LOGON`/`LOGOFF` or a newer handshake form if required. Then chunked framing + handshake | M | `crates/bolt/src/framing.rs`, `handshake.rs` | TX-8 | new crate |
| **TX-10** | HELLO/auth → mushroomdb token mapping; RESET; GOODBYE; connection actor (tokio task) | M | `crates/bolt/src/messages/`, `connection.rs` | TX-9 | new crate |
| **TX-11** | RUN / PULL / DISCARD: auto-commit read + write dispatch; cursor state; streaming | M | `crates/bolt/src/messages/run.rs` | TX-10, TX-1 | new crate |
| **TX-12** | BEGIN / COMMIT / ROLLBACK Bolt messages → engine transactions; bookmark encode/decode; causal-wait loop | M | `crates/bolt/src/messages/tx.rs` | TX-11, TX-1 | new crate |
| **TX-13** | ROUTE: single-server routing table | S | `crates/bolt/src/messages/route.rs` | TX-10 | new crate |
| **TX-14** | Error code mapping; error classification table; driver-retry path | S | `crates/bolt/src/error.rs` | TX-10 | new crate |
| **TX-15** | Built-in CALL procedures (`db.labels`, `dbms.components`, etc.) | S | `crates/bolt/src/procedures.rs` | TX-11 | new crate; coordinate W1 for Cypher CALL grammar |
| **TX-16** | TCP listener + TLS integration (share W4's rustls config); `--bolt-port` CLI flag; server.json config | S | `crates/bolt/src/server.rs`, `crates/server/src/http.rs`, `crates/cli/src/main.rs` | TX-10, W4-TLS | `http.rs`, `main.rs` |
| **TX-17** | Scoped-visibility enforcement per-Bolt-connection (RBAC mask on ReaderSnapshot) | S | `crates/bolt/src/connection.rs`, `core-api/src/mask.rs` | TX-10 | mask.rs |
| **TX-18** | Driver compat CI: Python + JS drivers test matrix (all 12 scenarios) | M | `.github/workflows/`, new `tests/bolt/` | TX-16, TX-17 | CI matrix |
| **TX-19** | Driver compat CI: Java + Go + .NET (best-effort; connect + basic read gate) | L | `.github/workflows/` | TX-18 | CI matrix |
| **TX-20** | PackStream: wire W2 temporal/binary types once W2 value variants exist | S | `crates/bolt/src/packstream.rs` | TX-8, W2 value types | packstream.rs |
| **TX-21** | Constraint checking at COMMIT: wire W2's `check_constraints` into `submit_batch` drain thread | S | `crates/core-api/src/shared.rs` | TX-1, W2 constraints | `shared.rs` |
| **TX-22** | Docs: `transactions.md`, `bolt.md`; update `limits.md` | S | `docs/site/` | TX-18 | docs |

Independent-parallel groups: {TX-1} → {TX-2, TX-3, TX-4, TX-5, TX-6} in parallel → TX-7.
{TX-8} → {TX-9} → {TX-10} → {TX-11, TX-12, TX-13, TX-14, TX-15} in parallel → {TX-16, TX-17} → TX-18 → TX-19.
TX-1 and TX-8 can start in parallel.  TX-20 and TX-21 gate on W2 delivery.

---

## 8. Decisions

### (a) Builder's call

**B1 — Optimistic buffering vs hold-the-lock transactions.**  
Recommend: optimistic buffering (TxBuffer, no lock held during tx lifetime).
Holding the writer lock for a transaction's lifetime blocks all writes for the
idle-client scenario; a 30-second idle timeout with lock held is unacceptable.
Optimistic with CAS at commit is the safe, proven approach.

**B2 — Bolt 4.4 + 5.0 as the version target.**  
Recommend: offer `[5.0, 4.4]` in the handshake.  This covers all 5.x-series
official drivers.  Bolt 5.1+ adds LOGON/LOGOFF (session auth refresh, useful
for long-lived connections but not required for correctness).  Defer 5.1+ to 0.11.

**B3 — Hand-written PackStream vs existing crate.**  
Recommend: hand-written.  The `bolt-proto` crate (crates.io) targets Bolt ≤ 3.x
and is unmaintained.  No production-ready Bolt 5.x Rust crate exists.  PackStream v1
is a well-specified, bounded encoding; implementing it is M effort and keeps
dependencies minimal.

**B4 — Python + JS as required CI gates; Java/Go/.NET as best-effort.**  
Recommend: Python and JS are trivially installed in GitHub Actions and cover the
majority of current mushroomdb users.  Java, Go, .NET require toolchain
installation but should be attempted as best-effort.  Gate on Python + JS only
to avoid flaky CI from JVM/dotnet image variability.

**B5 — element_id as decimal u32 string.**  
Recommend: `string(internal_id)`.  The Bolt spec requires string element IDs
(replacing integer IDs in Bolt 5.0+).  The internal u32 is stable within a
server session.  Document that element IDs are not stable across restarts (the
store's `IdMap` is deterministic but only within a WAL replay — a re-ingested
node may get a different u32).

**B6 — Cursor state per-connection, held in memory.**  
Recommend: cursors are per-connection, dropped on RESET or GOODBYE.  No
persistent cursor serialization.  A 100k-row stream is held in a `Vec<Row>` or
lazy iterator; for very large result sets, the lazy iterator approach avoids
materializing all rows.  Use the lazy approach (Cypher executor already supports
pull-based iteration).

### (b) Owner must decide

**O1 — Default Bolt port 7687 vs a different port.**  
Recommendation: use 7687 (the standard).  It conflicts with a locally running
incumbent server graph DB on the same machine — developers will need to stop
the other server.  **Alternative**: use 17687 as the default, document the standard
remapping.  **Consequence of 7687**: drivers connect with zero config but
developers with both running locally hit a port conflict.  **Consequence of 17687**:
no conflict but every driver example needs `bolt://localhost:17687`.  Decision
changes docs and CLI defaults.

**O2 — Bolt `scheme: "basic"` credential semantics.**  
Recommendation: treat `credentials` as the bearer token (ignoring `principal`).
**Alternative**: require `principal` = `"mushroomdb"` (reject mismatched principal).
Consequence: stricter auth rejects drivers that pass a custom principal;
recommendation is the lenient path that maximizes driver compatibility.

**O3 — Driver test matrix: include Java/Go/.NET in required gates.**  
Recommendation: best-effort only (see B4).  **Alternative**: require all five.
Consequence: CI is slower and more fragile; failure modes in JVM/.NET CI images
are unrelated to mushroomdb correctness.

**O4 — Causal consistency wait timeout.**  
Recommendation: 5 s default (`--bookmark-timeout-secs`).  **Alternative**: make
it fail immediately if the target commit_seq is not yet visible (correct but
defeats the purpose of bookmarks).  Decision affects user experience for async
write/read patterns.

---

## 9. New Dependencies

| Crate | Version | Purpose | No-new-dep alternative |
|-------|---------|---------|------------------------|
| `tokio` | already in `server` | async TCP listener for Bolt | already a dep — no new dep |
| `uuid` | 1.x | Generate `tx_id` for HTTP tx endpoints | `rand` (already present) + hex format — avoid adding uuid |
| `rustls` | already in `server` | TLS for Bolt listener | already a dep — no new dep |

**Net new crates required: zero** (uuid avoidable via rand).  The `bolt` crate
itself is a workspace-internal crate, not a new external dependency.

No external PackStream or Bolt protocol crate is proposed (see B3).

---

## 10. Out of Scope / Deferred

| Item | Reason |
|------|--------|
| Bolt 5.1+ LOGON/LOGOFF auth refresh | Additive; required only for long-lived connection auth rotation.  0.11. |
| Bolt 5.4 telemetry (`TELEMETRY` message) | No current use case.  0.11. |
| Bolt 6.0 | Not widely deployed by drivers in 2026.  0.11+. |
| Multi-writer concurrent transactions | Requires architectural change to single-writer WAL.  Post-1.0. |
| Distributed / cross-node transactions | Out of 0.10 scope per brief. |
| Bolt WebSocket transport | Some browser-based tools use Bolt-over-WebSocket.  Deferred; HTTP WebSocket already exists for MCP/events. |
| Persistent cursor state (survive server restart) | Operational complexity not worth it for 0.10. |
| `as_of` / time-travel via Bolt | No Bolt standard for this.  HTTP-only. |
| Full GQL `CALL {}` subqueries | W1 scope; W3 only adds `CALL procedure()`. |
| Disk-paged storage (graphs > RAM) | Brief explicitly defers this. |
| Read replicas / HA | Brief explicitly defers this. |
| `SHOW TRANSACTIONS` / `TERMINATE TRANSACTION` Cypher | Admin surface; post-0.10. |

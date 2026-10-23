# Bolt protocol

`mushroomdb serve` can listen for Bolt beside HTTP, so the official graph-database drivers can connect without a configuration change. This page covers how to turn the listener on, how clients authenticate, TLS, the routing table, and the version negotiation and its audit.

## Turning the listener on

The listener is off unless you give it a port:

```text
mushroomdb serve ./db --bolt-port 7687
```

or set `MUSHROOMDB_BOLT_PORT=7687`. It binds the IP of `--addr` (default `127.0.0.1`) with that port, and prints a second line after the HTTP one:

```text
listening on http://127.0.0.1:8080
listening on bolt://127.0.0.1:7687
```

`--bolt-port 0` picks a free port. Any other `--bolt-*` flag without a port is a startup error.

| Flag | Variable | Default | Meaning |
|---|---|---|---|
| `--bolt-port <port>` | `MUSHROOMDB_BOLT_PORT` | unset (off) | Serve Bolt on `--addr`'s IP and this port |
| `--bolt-tls`, `--bolt-plaintext` | `MUSHROOMDB_BOLT_TLS=on\|off` | TLS when `--tls-cert` is given, else plaintext | See [TLS](#tls) |
| `--bolt-advertised-host <host>` | `MUSHROOMDB_BOLT_ADVERTISED_HOST` | the bind IP | The host `ROUTE` hands drivers |
| `--bolt-advertised-port <port>` | `MUSHROOMDB_BOLT_ADVERTISED_PORT` | the bound port | The port `ROUTE` hands drivers |
| `--bolt-database <name>` | | `mushroomdb` | The store's name for `ROUTE` and database selection |
| `--bolt-max-connections <n>` | `MUSHROOMDB_BOLT_MAX_CONNECTIONS` | `256` | Clients served at once |
| `--bolt-tx-timeout-secs <secs>` | | `30` | Idle limit of an open transaction or result |
| `--bookmark-timeout-secs <secs>` | | `5` | Longest a `BEGIN` or `RUN` waits for a bookmark |

A flag wins over its variable.

## Authentication

Bolt accepts exactly the credentials HTTP accepts, with the same effect:

- the `--token` / `MUSHROOMDB_TOKEN` full-access token;
- a `--role-token` / `MUSHROOMDB_ROLE_TOKENS` token, which runs as its role;
- a key from `keys.json` with its tier: `admin`, `write`, or `read` (which cannot write over Bolt either). A `SIGHUP` reload reaches both listeners.

The driver sends the token as the password of basic authentication (the user name is ignored) or as a bearer token. With no token and no key configured, every client is let in, as on HTTP. A failed login is answered with one `Unauthorized` failure, the same for every cause, and the connection is closed.

The listener binds `--addr`'s IP, so the non-loopback rule is HTTP's: a non-loopback `--addr` with no `--token`, `MUSHROOMDB_TOKEN` or `admin` key is refused at startup, before either listener starts.

## Scoped access

A Bolt connection that logs in with a role token, or with a `read` key, sees and changes exactly what the same credential sees and changes through HTTP [`POST /query`](http.md#post-query) and [`/tx`](http.md#transactions): the same rows, the same refusals with the same text, and the same kind of error. The role is applied to every statement on the connection; nothing about it is chosen by the client.

- **Reads.** Every `RUN` reads through the role's visibility: its `keys`, its `labels` with their `visible_where` test, intersected with its `namespaces` binding. See [Masks](masks.md). A node outside it does not appear, in a pattern, a count or an edge's far end. `MATCH (n) RETURN n` for one role and for another, on the same store, return different nodes.
- **`SHOW INDEXES` and `SHOW CONSTRAINTS`** list only the indexes and constraints on labels that a node the role can see carries, as they do over HTTP.
- **Auto-commit writes.** A role with a write scope writes through the same scoped write path `/query` uses: a write outside the scope (a label it may not create, a namespace outside its binding) fails with `Forbidden` and the message HTTP gives with its 403, and a pattern naming a node the role cannot see matches nothing. A role with no write scope, and every `read` key, is refused every write (`read-tier key: writes are not permitted` for a key).
- **Schema statements.** `CREATE` and `DROP` of an index or constraint fail with `Forbidden` for a role.
- **Explicit transactions.** `BEGIN` resolves the role on the snapshot it freezes. Every statement reads through the role on that snapshot, so a commit that lands after `BEGIN` is not seen. A write fails with `Forbidden`: `role-bound token: writes are not permitted inside a transaction` for a role token, even one with a write scope, and the `read` key text for a key. Use auto-commit to write with a role (ledger row 140, planned for 0.11). `COMMIT` writes nothing and returns the bookmark of the commit the snapshot read. HTTP keeps its transaction open after a refused statement; on Bolt the failure ends the transaction, as the Bolt state machine requires, and the client sends `RESET`.
- **A role that no longer exists.** The role is resolved for each statement and at each `BEGIN`, not only at `HELLO`. Once the store no longer defines it, the next statement or `BEGIN` fails with `Unauthorized`, as HTTP answers 401. A change to the role's definition applies from the next auto-commit statement or `BEGIN`; an open transaction keeps the role as it was at its `BEGIN`.
- **Revoking a credential.** A connection authenticates once, at `HELLO`. A key removed from `keys.json` and reloaded with `SIGHUP` keeps working on a Bolt connection already open until it closes, while HTTP refuses it on its next request. Close such connections yourself, or restart. See [Access keys and the write tier](http.md#access-keys-and-the-write-tier). A change to the role a credential names takes effect as described above.
- **No per-request narrowing.** The `/query` body fields `mask`, `namespace` and `as_of` have no Bolt equivalent. Bolt applies only the credential's own scope.

How HTTP statuses and Bolt failures correspond:

| HTTP | Bolt failure |
|---|---|
| 400, 409, 413 | a client error other than a security one (`SyntaxError`, `Request.Invalid`, `ConstraintValidationFailed`) |
| 401 | `Unauthorized` |
| 403 | `Forbidden`, with the same message |
| 500 | a database error |
| 503 | a transient error, which drivers retry |

A full-access token, an `admin` key and a `write` key are unscoped on both protocols.

## TLS

Bolt serves the same certificate as HTTP, from `--tls-cert` and `--tls-key` (a build with the `tls` feature; see [Native TLS](deployment.md#native-tls---features-tls)).

- With `--tls-cert` and `--tls-key` given, Bolt is TLS by default and the second line reads `listening on bolt+s://…`. A plaintext Bolt listener beside TLS HTTP is started only when asked for, with `--bolt-plaintext` or `MUSHROOMDB_BOLT_TLS=off`.
- Without them Bolt is plaintext, and `--bolt-tls` is a startup error.

Clients choose encryption from the URI scheme: use the `+s` form of the scheme (`bolt+s://`, or the routing scheme's `+s` form) for a TLS listener, and the plain form otherwise. A plaintext client that connects to a TLS listener gets no Bolt reply and is closed.

## The routing table

A driver given a routing URI asks `ROUTE` on connect and then talks only to the address it is told. This server is the whole cluster, so the table names one address for the `WRITE`, `READ` and `ROUTE` roles, valid for 300 seconds:

- the host is `--bolt-advertised-host`, else the bind IP;
- the port is `--bolt-advertised-port`, else the bound port. Set it when a container maps another host port onto the listener (`-p 17687:7687`).

A wildcard `--addr` (`0.0.0.0` or `::`) with no advertised host is refused at startup: a driver handed `0.0.0.0` would dial itself. The table carries addresses only; the scheme, and with it TLS, stays the one the client was given.

Database names: an absent, null or empty name, the drivers' default name, and the `--bolt-database` name select the store, compared without regard to case. Any other name fails with `DatabaseNotFound`, in `ROUTE`, `RUN` and `BEGIN` alike.

## Limits, deadlines and shutdown

- **Handshake deadline.** A client has 10 seconds from connecting to finish the handshake (the TLS handshake gets 10 seconds of its own) and send a `HELLO` that authenticates, or it is closed.
- **Connection limit.** Past `--bolt-max-connections`, a new client gets its handshake answered and then a transient `DatabaseUnavailable` failure to its first message, and is closed. Drivers retry a transient error. A client holds its slot from the moment it connects, before it authenticates, so clients that never log in can fill the limit for up to the handshake deadline (20 seconds with TLS).
- **Isolation.** Each connection is its own task: a failure in one, a panic included, ends that connection only.
- **Shutdown.** On SIGTERM the Bolt listener stops accepting at the start of the drain phase, with HTTP. Each connection is closed at its next idle point (between requests, or after the replies to the request it is running), and an open transaction is rolled back. Connections still open when the drain bound passes are cut off. The whole sequence stays inside `--shutdown-timeout-secs`; see [Stopping cleanly](deployment.md#stopping-cleanly-and-the-grace-period).
- **Logging.** A storage failure, a busy store or corruption reaches the client as a fixed message; its detail is logged on the server.

## Versions the server speaks

| Bolt version | Status |
|---|---|
| 5.0 | offered, preferred |
| 4.4 | offered |
| 5.1 and later (`LOGON`/`LOGOFF`) | not offered |

The server picks the first client slot, in the client's order, that accepts one of its versions. It tries 5.0 before 4.4 within a slot.

## Driver compatibility matrix

Audited 2026-10-20 from each driver's handshake source on its default branch. "Latest version" is the newest release at that date.

| Driver | Latest version | Minimum offered | Maximum offered | Sends `0x000001FF` |
|---|---|---|---|---|
| Official Python driver | 6.4.1 | 3.0 | 5.8 in the opening handshake (6.1 only after a manifest exchange) | yes, first slot |
| Official JavaScript driver | 6.2.0 | 3.0 | 5.8 in the opening handshake (6.1 only after a manifest exchange) | yes, first slot |
| Official Java driver | 6.3.0 | 3.0 | 5.8 in the opening handshake (6.1 only after a manifest exchange) | yes, first slot |
| Official Go driver | 6.3.0 | 3.0 | 5.8 in the opening handshake (6.1 only after a manifest exchange) | yes, first slot |
| Official .NET driver | 6.4.0 | 3.0 | 5.8 in the opening handshake (6.1 only after a manifest exchange) | yes, first slot |

All five send the same four slots, in this order:

| Slot | Bytes | Meaning |
|---|---|---|
| 1 | `00 00 01 FF` | manifest indicator |
| 2 | `00 08 08 05` | 5.8 with range 8: versions 5.0 to 5.8 |
| 3 | `00 02 04 04` | 4.4 with range 2: versions 4.2 to 4.4 |
| 4 | `00 00 00 03` | 3.0 |

## Why 5.0 and 4.4

Slots 2 and 3 of every current driver contain 5.0 and 4.4, so a server that speaks only those two versions connects to all five. At a negotiated 5.0 the driver authenticates inside `HELLO`; `LOGON` and `LOGOFF` start at 5.1 and are not needed. Nothing in the audit requires a later 5.x message.

## How the handshake is matched

- A slot is `[pad, range, minor, major]`. It accepts the server version `major.v` for every `v` from `minor - range` to `minor`.
- The manifest indicator `0x000001FF` is skipped. The server does not run the manifest exchange; it answers with a plain version taken from the other slots, which every current driver accepts.
- The reply is `[0, 0, minor, major]`, or four zero bytes when no slot matches.
- After the handshake, messages travel as chunks: a two-byte big-endian length, then that many bytes, ending with a zero-length chunk.

## Transactions and bookmarks

`BEGIN`, `COMMIT` and `ROLLBACK` are supported. A transaction reads a snapshot frozen at `BEGIN`, sees its own writes, and commits as one atomic batch; if another writer changed a node the transaction read or wrote, `COMMIT` fails with a transient error and the driver retries from `BEGIN`.

| `BEGIN` field | Behaviour |
|---|---|
| `bookmarks` | `BEGIN` waits until the store has reached the highest bookmark, then freezes its snapshot. The wait is 5 seconds by default (`--bookmark-timeout-secs`); past it the request fails with a transient `DatabaseUnavailable` error. |
| `db` | Checked against the routing config, as `ROUTE` and `RUN` are: absent, null, empty, the driver default name or the configured database name (any case) are accepted; any other name fails with `DatabaseNotFound`. |
| `mode` | `"w"` (default) or `"r"`. A read transaction refuses write statements. |
| `tx_timeout`, `tx_metadata` | Checked for shape and otherwise ignored. The connection's idle limit (`--bolt-tx-timeout-secs`, 30 seconds by default) applies to every transaction. |
| `imp_user` | Not supported; naming a user fails with `Forbidden`. |

A bookmark is the text `mushroomdb:commit:<n>`, where `n` is a commit number. `COMMIT` and an auto-commit `PULL` return one; a transaction that wrote nothing returns the commit its snapshot read. Pass it in a later `BEGIN` or `RUN` to read at least that commit.

In this build some write statements are refused inside a transaction and run only as auto-commit `RUN`: `MATCH … CREATE` / `MATCH … MERGE`, `UNWIND … <write>` (such as `UNWIND $rows AS r MATCH … CREATE …`), the `SET n = {…}` / `SET n += {…}` map forms and `REMOVE … RETURN`. Drivers' managed write transactions (`session.execute_write` and the like) send `BEGIN`, so they are refused there too, with `not supported in mushroomdb 0.10 inside a transaction: …`; send such a statement with `session.run` (auto-commit). See [Write statements](query.md#write-statements).

A `COMMIT` or `ROLLBACK` while a result is still open is a protocol violation, as the Bolt state machine specifies; pull or discard the open results first, as the drivers do. A role-bound token's or `read` key's transaction reads through its role and cannot write (see [Scoped access](#scoped-access)); use auto-commit to write with a role. A `write` or `admin` key from `keys.json` writes in a transaction; a `read` key cannot run a writing statement anywhere. See [Access keys and the write tier](http.md#access-keys-and-the-write-tier).

A Bolt null parameter is a null: `WHERE $p IS NULL` is true for it. (The HTTP route currently leaves a JSON null parameter out, so the same statement fails there with a missing-parameter error.)

## Catalog procedures

`RUN` answers the procedure calls graph browsers and driver tools send to
discover a store: `CALL db.labels()`, `CALL db.relationshipTypes()`,
`CALL db.propertyKeys()`, `CALL db.indexes()`, `CALL db.constraints()`,
`CALL dbms.components()` and `CALL db.schema.visualization()`, with the columns
named in [Procedures](query.md#procedures-call). `dbms.components()` reports the
name `mushroomdb`, the engine version, and the edition `open`. A role-bound
token sees only the labels, relationship types, property keys, indexes and
constraints of the nodes its role can read, as it does for every other read.
Calls other than these are an unknown-procedure error.

## Value types

Results and `RUN` parameters use the same mapping in both directions. A value sent as a parameter, stored, and read back arrives as it was sent.

| mushroomdb value | Bolt value | Notes |
|---|---|---|
| null, boolean, integer, float, string, list, map | the PackStream type of the same name | |
| binary | Bytes | Up to 1 MiB as a stored property. |
| date | Date structure (`0x44`) | Days since 1970-01-01. |
| local date-time | LocalDateTime structure (`0x64`) | Seconds and nanoseconds. |
| date-time with a UTC offset | DateTime structure: `0x49` at Bolt 5.0, `0x46` at 4.4 | 5.0 counts seconds in UTC; 4.4 counts seconds in local time. A client must use the form its version uses; the other is refused. The offset is at most ±18:00. |
| duration | Duration structure (`0x45`) | Months, days, seconds and nanoseconds, kept exactly. |

mushroomdb keeps date-times to the microsecond. A result is sent with nanoseconds = microseconds × 1000. A local date-time or date-time parameter with a nanosecond part that is not a whole number of microseconds is refused, not rounded. A parameter too large for the engine's fields is refused too, for example a date past 32 bits of days, or a duration whose seconds overflow 64 bits of nanoseconds.

These parameters are refused with `Neo.ClientError.Request.Invalid`, and the message names the type:

| Bolt value | Why |
|---|---|
| DateTime with a named time zone (`0x69`, `0x66`) | Only a UTC offset is stored, not a zone name. Send the offset form. |
| Time (`0x54`), LocalTime (`0x74`) | There is no time-of-day type. |
| Point2D (`0x58`), Point3D (`0x59`) | There is no spatial type. |
| Node, Relationship, Path | A graph entity is not a parameter value. |
| any other structure tag | Unknown. |

A temporal structure with the wrong number or type of fields fails the same way, as a malformed request. A refused parameter fails the `RUN` before the statement runs, so nothing is written; `RESET` makes the connection ready again.

### RESET while a request is running

A `RESET` sent while an earlier request is still running takes effect at the first of these points:

- a bookmark wait ends within 10 ms, and that request is answered `IGNORED`;
- the next request queued in the same batch is `IGNORED`;
- otherwise when the running statement or commit returns. A statement inside the engine cannot be interrupted: it finishes and its reply is sent, and a `COMMIT` or auto-commit write that was already submitted stays committed.

The `RESET` is answered `SUCCESS` after every earlier request has been answered, and the connection is then ready; an open transaction is rolled back. A client that disconnects during a request has its transaction rolled back.


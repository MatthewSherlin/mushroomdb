# Bolt protocol

mushroomdb is adding a Bolt listener so that the official graph-database drivers can connect without a configuration change. This page records the version negotiation the listener implements and the audit that fixed it. The listener itself is under construction; this page describes the handshake and transactions.

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
| `bookmarks` | `BEGIN` waits until the store has reached the highest bookmark, then freezes its snapshot. The wait is 5 seconds by default; past it the request fails with a transient `DatabaseUnavailable` error. |
| `db` | Checked against the routing config, as `ROUTE` and `RUN` are: absent, null, empty, the driver default name or the configured database name are accepted; any other name fails with `DatabaseNotFound`. |
| `mode` | `"w"` (default) or `"r"`. A read transaction refuses write statements. |
| `tx_timeout`, `tx_metadata` | Checked for shape and otherwise ignored. The connection's idle limit (30 seconds) applies to every transaction. |
| `imp_user` | Not supported; naming a user fails with `Forbidden`. |

A bookmark is the text `mushroomdb:commit:<n>`, where `n` is a commit number. `COMMIT` and an auto-commit `PULL` return one; a transaction that wrote nothing returns the commit its snapshot read. Pass it in a later `BEGIN` or `RUN` to read at least that commit.

A `COMMIT` or `ROLLBACK` while a result is still open is a protocol violation, as the Bolt state machine specifies; pull or discard the open results first, as the drivers do. A role-bound token cannot run statements inside a transaction (they fail with `Forbidden`); use auto-commit for those. A `write` or `admin` key from `keys.json` can; see [Access keys and the write tier](http.md#access-keys-and-the-write-tier).

A Bolt null parameter is a null: `WHERE $p IS NULL` is true for it. (The HTTP route currently leaves a JSON null parameter out, so the same statement fails there with a missing-parameter error.)

### RESET while a request is running

A `RESET` sent while an earlier request is still running takes effect at the first of these points:

- a bookmark wait ends within 10 ms, and that request is answered `IGNORED`;
- the next request queued in the same batch is `IGNORED`;
- otherwise when the running statement or commit returns. A statement inside the engine cannot be interrupted: it finishes and its reply is sent, and a `COMMIT` or auto-commit write that was already submitted stays committed.

The `RESET` is answered `SUCCESS` after every earlier request has been answered, and the connection is then ready; an open transaction is rolled back. A client that disconnects during a request has its transaction rolled back.


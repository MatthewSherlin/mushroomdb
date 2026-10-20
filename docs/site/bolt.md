# Bolt protocol

mushroomdb is adding a Bolt listener so that the official graph-database drivers can connect without a configuration change. This page records the version negotiation the listener implements and the audit that fixed it. The listener itself is under construction; this page describes the handshake only.

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

# Panic policy

mushroomdb is an embedded library. A panic inside it propagates to the host
process. This document states exactly which conditions panic, which return a
typed error, and why — so embedders can reason about their crash story without
reading source code.

---

## The rule

**Disk-reachable decode paths never panic.** Corrupt, truncated, or adversarial
on-disk bytes — snapshot sections, WAL records, sidecar JSON, archive files —
always produce `GraphError::Corrupt { detail }`, never a panic.

**Post-validation invariant expects stay.** A handful of `.expect()` calls in
query paths assert properties that were checked at open time. After a successful
`GraphDb::open` these cannot fire. They are documented in the table below.

**Lock-poison guards stay.** `Mutex::lock().expect("mutex poisoned")` fires only
if a panic already occurred on another thread inside the same guard scope. It is
a second panic during cleanup, not a new failure mode.

---

## What fires at open (`GraphDb::open`) on a corrupt store

| Check location | What it catches | Returned error |
|---|---|---|
| `v8::parse_header` | Bad magic, wrong version, header CRC mismatch, directory length overflow | `GraphError::Corrupt` |
| `v8::validate_section_bounds` | Section `(offset, len)` extends past file end; section `len` below rkyv root minimum for large sections | `GraphError::Corrupt` |
| `restore_v8_base` — cross-section label check | `labels.len() != ids.len()`; live node with sentinel label (`u32::MAX`); label sym out of interner range | `GraphError::Corrupt` |
| `restore_snapshot_state` (V5/V7) — cross-section label check | Same three invariants for legacy format snapshots | `GraphError::Corrupt` |
| `snapshot::decode_v8_from_mapped` | rkyv validated-access failure on any section | `GraphError::Corrupt` |
| `wal::decode_all` | Any truncation or corruption — returns the safe prefix decoded so far | Returns shortened record list; no error |

---

## Post-validation invariants — expects that stay

These `.expect()` calls are unreachable on a valid, fully-opened store.  The
property each one relies on is guaranteed by the checks listed in the table
above.

| Site | Invariant | Guaranteed by |
|---|---|---|
| `NodeRef::label()` | Live node id is within `labels`; sym is not sentinel; sym resolves in interner | `restore_v8_base` + `restore_snapshot_state` cross-section label check |
| `neighborhood_masked()` — label lookup | Same as above | Same |
| `NodeRef::neighborhood()` — label lookup | Same as above | Same |
| `wal::encode_record` — bincode serialize | Serializing a `WalRecord` (write path) cannot fail | Write path, not disk-reachable |
| Various topology/column accessors | `(offset, len)` fits in backing buffer | `validate_section_bounds` |

**If any of these panics in a shipped binary, it is a bug.** File a report with
the snapshot and WAL files.

---

## Lock-poison guards

```
lock.expect("mutex poisoned")
```

These are intentional. A mutex is poisoned only when a previous panic left the
protected data in an unknown state. Continuing to operate on poisoned state
would produce silent data corruption. Panicking again is the correct choice.

---

## No `catch_unwind` wrapper

mushroomdb does not wrap its API in `catch_unwind`. The crash story for
embedders is:

1. All disk-reachable paths return `Result` — check the error before proceeding.
2. If a post-validation invariant fires (table above), the store is already
   open and the data is intact on disk. Close the database, report the bug,
   and reopen.
3. WAL replay on the next open is the recovery path for mid-write crashes — no
   separate crash-recovery API is needed.

Embedders that need process isolation against unexpected panics should use OS
process or thread boundaries (`std::process::Command`, worker threads with
`std::thread::spawn` + `JoinHandle::join`). These are outside the scope of the
embedded library.

---

## Fuzz coverage

`crates/core-storage/tests/format_fuzz.rs` runs five property suites (256 cases
each, 1280 total) on every CI run:

| Block | Generator | Checked path |
|---|---|---|
| (a) | Arbitrary bytes | `wal::decode_all` |
| (b) | Arbitrary bytes | `snapshot::decode` |
| (c) | Bit-flip / truncate / splice of valid encodings | Both WAL and snapshot decode |
| (d) | CRC-reattached mutated bincode payload | `snapshot::decode` bincode path |
| (e) | V8 section directory corruptions (oversized len, tiny len, arbitrary len) with header CRC recomputed | `MappedBase::validate_section_bounds` + `snapshot::decode` |

All five suites assert no panic via `catch_unwind`. Block (e) specifically
covers the `validate_section_bounds` min-rkyv-root-size check added in the
KB-hardening audit.

---

See also: [format-stability](../format-stability.md)
